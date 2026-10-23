use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use crate::agent::AgentKind;
use crate::cage::manager::{Manager, Vm, blocking, lock};
use crate::cage::{TaskDraft, WORKSPACE};
use crate::error::{Error, Result};
use crate::runtime::SandboxRuntime;
use crate::task_agents::Exec;
use crate::task_artifacts::{self, Mirror, RoundWriter};
use crate::task_runs::{self, Control, RunHandle, RunState};

impl<R: SandboxRuntime> Manager<R> {
    pub fn put_task(&self, id: &str, draft: TaskDraft) -> Result<String> {
        let mut task_id = String::new();
        self.edit(id, |s| {
            task_id = s.put_task(draft)?;
            Ok(())
        })?;
        Ok(task_id)
    }

    /// Also removes the runs of the task. A task with a live run stays.
    pub fn remove_task(&self, id: &str, task_id: &str) -> Result<()> {
        if self.runs.is_running(id, task_id) {
            return Err(Error::TaskRunning);
        }
        let slug = lock(&self.state).project_mut(id)?.task(task_id)?.slug.clone();
        self.edit(id, |s| s.remove_task(task_id))?;
        self.runs.forget(id, Some(task_id));
        match std::fs::remove_dir_all(self.task_dir(id, &slug)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// The host-only folder of a task's runs: the guest cannot write it.
    pub(crate) fn task_dir(&self, id: &str, slug: &str) -> PathBuf {
        self.data.join("tasks").join(id).join(slug)
    }

    /// The round folder of a run. Only an existing run of the task counts.
    pub fn round_dir(&self, id: &str, task_id: &str, run: &str) -> Result<PathBuf> {
        let slug = lock(&self.state).project_mut(id)?.task(task_id)?.slug.clone();
        let dir = self.task_dir(id, &slug);
        if !run_names(&dir).iter().any(|r| r == run) {
            return Err(Error::RunNotFound);
        }
        Ok(dir.join(run))
    }

    /// Starts a run of the task in the running VM. Returns the run name.
    /// The round goes to host app data; the harness gets a mirror, which
    /// must be writable before the run starts.
    pub fn start_task_run(&self, id: &str, task_id: &str) -> Result<String> {
        let _start = lock(&self.run_start);
        let (task, vm, applied, pi_host, gemini, proxy, unmounted) = {
            let mut state = lock(&self.state);
            let task = state.project_mut(id)?.task(task_id)?.clone();
            match state.vms.get(id) {
                Some(Vm::Running { vm, applied, runtimes, host, .. }) => {
                    let unmounted = task
                        .agents
                        .iter()
                        .find(|a| a.kind != AgentKind::Pi && !runtimes.contains(&a.kind.name()))
                        .map(|a| self.unmounted(a.kind, host));
                    let (pi_host, gemini, proxy) = (host.pi_host, host.gemini.clone(), host.proxy.clone());
                    (task, vm.clone(), applied.clone(), pi_host, gemini, proxy, unmounted)
                }
                _ => return Err(Error::NotRunning),
            }
        };
        if self.runs.is_running(id, task_id) {
            return Err(Error::TaskRunning);
        }
        // The VM has the mounts of its start, so the paths are checked there.
        if applied.host_of(&task.workdir).is_none() {
            return Err(Error::TaskWorkdir(task.title.clone(), task.workdir.clone()));
        }
        let dir = self.task_dir(id, &task.slug);
        let run = format!("{:02}", run_names(&dir).iter().filter_map(|r| r.parse::<u32>().ok()).max().unwrap_or(0) + 1);
        let harness = applied.harness.as_deref().unwrap_or(WORKSPACE);
        let mirror = match applied.host_of(harness) {
            Some((root, rel, true)) => {
                let mut parts: Vec<String> = rel.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                parts.extend([".zwinger", "tasks", &task.slug, &run].map(str::to_owned));
                Mirror { root, parts }
            }
            _ => return Err(Error::MirrorUnwritable(harness.to_owned(), "the folder is read-only".to_owned())),
        };
        if let Some(e) = unmounted {
            return Err(e);
        }
        mirror.check()?;
        let writer = RoundWriter::create(dir.join(&run), Some(mirror))?;
        let exec = Exec {
            runtime: self.runtime.clone(),
            vm,
            cwd: task.workdir.clone(),
            artifacts: Some(guest_artifacts(harness, &task.slug, &run)),
            pi_host,
            gemini,
            proxy,
        };
        self.runs.start(task_runs::Spec {
            project_id: id.to_owned(),
            task,
            run: run.clone(),
            exec,
            writer,
            config: self.run_config,
        });
        Ok(run)
    }

    pub fn control_task_run(&self, id: &str, task_id: &str, run: &str, control: Control) -> Result<()> {
        self.run_handle(id, task_id, run).ok_or(Error::RunNotFound)?.control(control)
    }

    /// The run of this app session, if it is one.
    pub fn run_handle(&self, id: &str, task_id: &str, run: &str) -> Option<Arc<RunHandle>> {
        self.runs.find(id, task_id, run)
    }

    /// The runs of all tasks of the project, oldest first per task.
    pub async fn list_task_runs(&self, id: &str) -> Result<Vec<RunSummary>> {
        let tasks: Vec<(String, PathBuf)> = lock(&self.state)
            .project_mut(id)?
            .tasks
            .iter()
            .map(|t| (t.id.clone(), self.task_dir(id, &t.slug)))
            .collect();
        let found = blocking(move || {
            let mut found = Vec::new();
            for (task_id, dir) in tasks {
                for run in run_names(&dir) {
                    let room = task_artifacts::read_room(&dir.join(&run));
                    found.push((task_id.clone(), run, room));
                }
            }
            Ok(found)
        })
        .await?;
        Ok(found
            .into_iter()
            .map(|(task_id, run, room)| {
                let live = self.runs.find(id, &task_id, &run).map(|r| r.status());
                let ended = match room.ended.as_deref() {
                    Some("done") => RunState::Done,
                    Some("cancelled") => RunState::Cancelled,
                    Some(_) => RunState::Failed,
                    // A run of an earlier app session that did not settle.
                    None => RunState::Failed,
                };
                RunSummary {
                    state: live.as_ref().map_or(ended, |s| s.state),
                    stage: live.as_ref().and_then(|s| s.stage.clone()),
                    ship: room.outcome.as_ref().map(|o| o.ship),
                    started: room.stages.first().and_then(|s| s.started.clone()),
                    task_id,
                    run,
                }
            })
            .collect())
    }

    /// The change of a run: the commits since its start, then the diff from
    /// its start. A run with no known start shows the uncommitted change.
    pub async fn run_diff(&self, id: &str, task_id: &str, run: &str) -> Result<String> {
        let dir = self.round_dir(id, task_id, run)?;
        let base = match self.runs.find(id, task_id, run) {
            Some(handle) => handle.status().base,
            None => blocking(move || Ok(task_artifacts::read_room(&dir).outcome.and_then(|o| o.base))).await?,
        };
        let (task, vm, harness) = {
            let mut state = lock(&self.state);
            let task = state.project_mut(id)?.task(task_id)?.clone();
            match state.vms.get(id) {
                Some(Vm::Running { vm, applied, .. }) => (task, vm.clone(), applied.harness.clone()),
                _ => return Err(Error::NotRunning),
            }
        };
        let exec = Exec {
            runtime: self.runtime.clone(),
            vm,
            cwd: task.workdir,
            artifacts: Some(guest_artifacts(harness.as_deref().unwrap_or(WORKSPACE), &task.slug, run)),
            // The diff runs git only.
            pi_host: false,
            gemini: None,
            proxy: None,
        };
        let (_, mut cancel) = tokio::sync::watch::channel(false);
        Ok(exec
            .diff(base.as_deref(), &mut cancel)
            .await
            .unwrap_or_else(|| "The working dir is not a git work tree, so the app cannot show its change.".to_owned()))
    }
}

/// The guest path of a run's artifact mirror in the harness.
pub(crate) fn guest_artifacts(harness: &str, slug: &str, run: &str) -> String {
    format!("{}/.zwinger/tasks/{slug}/{run}", harness.trim_end_matches('/'))
}

/// A run of a task, for the tree and the room.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub task_id: String,
    pub run: String,
    pub state: RunState,
    /// The active stage of a live run.
    pub stage: Option<String>,
    pub ship: Option<bool>,
    pub started: Option<String>,
}

/// The run folders of a task folder, sorted.
pub(crate) fn run_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use crate::cage::testing::*;

    #[tokio::test]
    async fn tasks_are_saved_and_a_run_needs_a_writable_mirror() {
        let (manager, _runtime, id) = manager();
        let kinds = [AgentKind::Pi, AgentKind::Claude];
        assert!(matches!(
            manager.put_task(&id, task_draft(None, "Fix", [AgentKind::Pi, AgentKind::Pi])),
            Err(Error::InvalidTask(..))
        ));
        let task_id = manager.put_task(&id, task_draft(None, "Fix the map!", kinds)).unwrap();
        manager.put_task(&id, task_draft(None, "Fix the map", kinds)).unwrap();
        let tasks = manager.list()[0].project.tasks.clone();
        assert_eq!(tasks.iter().map(|t| t.slug.as_str()).collect::<Vec<_>>(), ["fix-the-map", "fix-the-map-2"]);
        assert_eq!(
            (tasks[0].request.as_str(), tasks[0].gates.as_slice()),
            ("Fix the map.", ["pnpm build".to_owned()].as_slice())
        );
        // A rename keeps the folder of the runs.
        manager.put_task(&id, task_draft(Some(task_id.clone()), "Renamed", kinds)).unwrap();
        assert_eq!(manager.list()[0].project.tasks[0].slug, "fix-the-map");
        assert!(matches!(manager.start_task_run(&id, &task_id), Err(Error::NotRunning)));

        // The harness is in a read-only mount: the run cannot write its mirror.
        let mount = temp_dir();
        manager.add_mount(&id, mount.to_str().unwrap(), "/mnt/ro", true).unwrap();
        manager.set_harness(&id, Some("/mnt/ro")).unwrap();
        manager.start(&id).await.unwrap();
        assert!(matches!(manager.start_task_run(&id, &task_id), Err(Error::MirrorUnwritable(..))));
        // A task edit needs no restart.
        manager.put_task(&id, task_draft(Some(task_id.clone()), "Again", kinds)).unwrap();
        assert!(!manager.list()[0].pending_restart);

        manager.stop(&id).await.unwrap();
        manager.set_mount_readonly(&id, "/mnt/ro", false).unwrap();
        manager.start(&id).await.unwrap();
        assert!(matches!(manager.start_task_run(&id, &task_id), Err(Error::RuntimeMissing(name)) if name == "claude"));
        assert!(!mount.join(".zwinger").exists());
        assert!(manager.list_task_runs(&id).await.unwrap().is_empty());
        assert!(matches!(manager.round_dir(&id, &task_id, "01"), Err(Error::RunNotFound)));

        manager.remove_task(&id, &task_id).unwrap();
        assert_eq!(manager.list()[0].project.tasks.len(), 1);
    }
}
