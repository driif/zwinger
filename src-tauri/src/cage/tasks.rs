use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::a2a::ShareInfo;
use crate::agent::AgentKind;
use crate::cage::Mount;
use crate::cage::manager::{Manager, Vm, blocking, in_guest_folder, lock};
use crate::error::{Error, Result};
use crate::run_git::{self, Origin};
use crate::runtime::SandboxRuntime;
use crate::session::{Task, TaskDraft};
use crate::task_agents::{Exec, SnapshotError};
use crate::task_artifacts::{self, Mirror, RoundWriter};
use crate::task_recovery::{self, Recovered, Resume, RunRecord, TaskRuns};
use crate::task_runs::{self, Control, RunHandle, RunState};

impl<R: SandboxRuntime> Manager<R> {
    pub fn put_task(&self, id: &str, draft: TaskDraft) -> Result<String> {
        let mut task_id = String::new();
        self.edit_session(id, |s, c| {
            task_id = s.put_task(c.ok_or(Error::NoCage)?, draft)?;
            Ok(())
        })?;
        Ok(task_id)
    }

    /// Also removes the runs of the task. A task with a live run stays.
    pub fn remove_task(&self, id: &str, task_id: &str) -> Result<()> {
        if self.runs.is_running(id, task_id) {
            return Err(Error::TaskRunning);
        }
        let slug = lock(&self.state).session(id)?.task(task_id)?.slug.clone();
        self.edit_session(id, |s, _| s.remove_task(task_id))?;
        self.runs.forget(id, Some(task_id));
        self.shares.forget(id, Some(task_id));
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
        let slug = lock(&self.state).session(id)?.task(task_id)?.slug.clone();
        let dir = self.task_dir(id, &slug);
        if !run_names(&dir).iter().any(|r| r == run) {
            return Err(Error::RunNotFound);
        }
        Ok(dir.join(run))
    }

    /// Starts a run of the task in the running VM. Returns the run name.
    /// The round goes to host app data; the session folder gets a copy, which
    /// must be writable before the run starts.
    pub fn start_task_run(&self, id: &str, task_id: &str) -> Result<String> {
        let _start = lock(&self.run_start);
        let task = lock(&self.state).session(id)?.task(task_id)?.clone();
        let record = RunRecord::new(&task, self.run_config);
        self.launch_run(id, task, record, None)
    }

    /// Continues an ended run in a new run, from its first group that is not
    /// complete. The new run has the task values, gates and limits that the
    /// ended run started with; only the working dir resolves through the
    /// current VM binding. Returns the new run name.
    pub fn resume_task_run(&self, id: &str, task_id: &str, run: &str) -> Result<String> {
        let _start = lock(&self.run_start);
        let task = lock(&self.state).session(id)?.task(task_id)?.clone();
        let dir = self.task_dir(id, &task.slug);
        let names = run_names(&dir);
        if !names.iter().any(|r| r == run) {
            return Err(Error::RunNotFound);
        }
        if self.runs.find(id, task_id, run).is_some_and(|r| r.status().state.is_live()) {
            return Err(Error::Resume("the run is still live".to_owned()));
        }
        let recovered = TaskRuns::read(&dir, &names).load(run).map_err(Error::Resume)?;
        let task = recovered.record.task(&task.id, &task.slug, recovered.record.workdir.clone());
        let record = recovered.record.clone();
        self.launch_run(id, task, record, Some((&recovered, run)))
    }

    /// The one start of a run, new or continued: the cage, the agents and
    /// the working dir must be ready, and no run of the task may be live.
    /// `run.json` is durable before any agent starts. A continuation also
    /// gets the state of its source before its first agent starts.
    fn launch_run(
        &self,
        id: &str,
        mut task: Task,
        mut record: RunRecord,
        resume: Option<(&Recovered, &str)>,
    ) -> Result<String> {
        let (vm, folder, scope, host_folder, shared, pi_host, gemini, proxy, unmounted, origin) = {
            let state = lock(&self.state);
            let (member, cage, running, folder, shared) = state.task_cage(id)?;
            task.workdir = in_guest_folder(&folder, &task.workdir);
            let Vm::Running { vm, runtimes, host, .. } = running else {
                unreachable!("task_cage returns a running VM")
            };
            let scope = git_scope(&task, &folder, &cage.mounts)?;
            let origin = Origin::of(&task.workdir, &scope, &folder, &member.folder, &cage.mounts);
            let unmounted = task
                .agents
                .iter()
                .find(|a| a.kind != AgentKind::Pi && !runtimes.contains(&a.kind.name()))
                .map(|a| self.unmounted(a.kind, host));
            (
                vm.clone(),
                folder,
                scope,
                member.folder.clone(),
                shared,
                host.pi_host,
                host.gemini.clone(),
                host.proxy.clone(),
                unmounted,
                origin,
            )
        };
        // A continuation keeps the origin of its source: a changed mapping
        // makes its git action unavailable.
        if resume.is_none() {
            record.origin = origin;
        }
        if self.runs.is_running(id, &task.id) {
            return Err(Error::TaskRunning);
        }
        let dir = self.task_dir(id, &task.slug);
        let run = format!("{:02}", run_names(&dir).iter().filter_map(|r| r.parse::<u32>().ok()).max().unwrap_or(0) + 1);
        let mirror =
            Mirror { root: host_folder, parts: [".zwinger", "tasks", &task.slug, &run].map(str::to_owned).to_vec() };
        if let Some(e) = unmounted {
            return Err(e);
        }
        mirror.check()?;
        let writer = RoundWriter::create(dir.join(&run), Some(mirror))?;
        match resume {
            None => writer.write_durable(task_recovery::RUN, &serde_json::to_vec_pretty(&record)?)?,
            Some((recovered, source)) => {
                task_recovery::create(&writer, recovered, source)?;
                // The start of the repository belongs to the first run of the chain.
                for name in [run_git::START, run_git::START_STATUS] {
                    if let Ok(Some(bytes)) = task_artifacts::read_record(&dir.join(&recovered.source), name) {
                        writer.write_durable(name, &bytes)?;
                    }
                }
            }
        }
        let exec = Exec {
            runtime: self.runtime.clone(),
            vm,
            cwd: task.workdir.clone(),
            scope,
            artifacts: Some(guest_artifacts(&folder, &task.slug, &run)),
            pi_host,
            gemini,
            proxy,
        };
        let spec = task_runs::Spec {
            session_id: id.to_owned(),
            task,
            run: run.clone(),
            exec,
            writer,
            config: record.config(),
            shared_vm: shared,
        };
        match resume {
            None => self.runs.start(spec),
            Some((recovered, _)) => self.runs.resume(spec, recovered),
        };
        Ok(run)
    }

    pub fn control_task_run(&self, id: &str, task_id: &str, run: &str, control: Control) -> Result<()> {
        self.run_handle(id, task_id, run).ok_or(Error::RunNotFound)?.control(control)
    }

    /// Starts an A2A share of a run of this app session, or returns the
    /// share that runs.
    pub fn share_task_run(&self, id: &str, task_id: &str, run: &str) -> Result<ShareInfo> {
        let dir = self.round_dir(id, task_id, run)?;
        self.shares.start(id, task_id, run, dir, || self.run_handle(id, task_id, run))
    }

    /// The run of this app session, if it is one.
    pub fn run_handle(&self, id: &str, task_id: &str, run: &str) -> Option<Arc<RunHandle>> {
        self.runs.find(id, task_id, run)
    }

    /// The runs of all tasks of the session, oldest first per task.
    pub async fn list_task_runs(&self, id: &str) -> Result<Vec<RunSummary>> {
        let tasks: Vec<(String, PathBuf)> =
            lock(&self.state).session(id)?.tasks.iter().map(|t| (t.id.clone(), self.task_dir(id, &t.slug))).collect();
        let found = blocking(move || {
            let mut found = Vec::new();
            for (task_id, dir) in tasks {
                let names = run_names(&dir);
                let runs = TaskRuns::read(&dir, &names);
                for run in &names {
                    let room = task_artifacts::read_room(&dir.join(run));
                    found.push((task_id.clone(), run.clone(), room, runs.continues(run), runs.inspect(run)));
                }
            }
            Ok(found)
        })
        .await?;
        Ok(found
            .into_iter()
            .map(|(task_id, run, room, continues, resume)| {
                let live = self.runs.find(id, &task_id, &run).map(|r| r.status());
                let ended = match room.ended.as_deref() {
                    Some("done") => RunState::Done,
                    Some("cancelled") => RunState::Cancelled,
                    Some(_) => RunState::Failed,
                    // A run of an earlier app session that did not settle.
                    None => RunState::Failed,
                };
                let state = live.as_ref().map_or(ended, |s| s.state);
                RunSummary {
                    state,
                    stage: live.as_ref().and_then(|s| s.stage.clone()),
                    ship: room.outcome.as_ref().map(|o| o.ship),
                    started: room.stages.first().and_then(|s| s.started.clone()),
                    continues,
                    resume: (!state.is_live()).then_some(resume),
                    task_id,
                    run,
                }
            })
            .collect())
    }

    /// The change of a run: the commits since its start, then the diff from
    /// its start. A run with no known start shows the uncommitted change.
    /// A live run reads the VM. An ended run shows the diff that it saved.
    pub async fn run_diff(&self, id: &str, task_id: &str, run: &str) -> Result<String> {
        let dir = self.round_dir(id, task_id, run)?;
        let record_dir = dir.clone();
        let status = self.runs.find(id, task_id, run).map(|handle| handle.status());
        let live = status.as_ref().is_some_and(|s| s.state.is_live());
        let (saved, base) = match status {
            Some(status) if live => (None, status.base),
            status => {
                blocking(move || {
                    let base = match status {
                        Some(status) => status.base,
                        None => task_artifacts::read_room(&dir).outcome.and_then(|o| o.base),
                    };
                    Ok((task_artifacts::read_diff(&dir), base))
                })
                .await?
            }
        };
        if let Some(saved) = saved {
            return Ok(saved);
        }
        // The working dir of the run: a task edit after the start does not move it.
        let recorded = blocking(move || Ok(task_recovery::recorded_workdir(&record_dir))).await?;
        let (task, vm, folder, scope) = {
            let state = lock(&self.state);
            let session = state.session(id)?;
            let mut task = session.task(task_id)?.clone();
            let (_, cage, running, folder, _) = match state.task_cage(id) {
                Err(Error::NotRunning) if !live => {
                    return Ok("This run saved no diff. Start the cage to read the working dir.".to_owned());
                }
                found => found?,
            };
            let recorded = recorded.map_err(Error::RunRecord)?;
            task.workdir = in_guest_folder(&folder, &recorded.unwrap_or(task.workdir));
            let Vm::Running { vm, .. } = running else { unreachable!("task_cage returns a running VM") };
            let scope = git_scope(&task, &folder, &cage.mounts)?;
            (task, vm.clone(), folder, scope)
        };
        let exec = Exec {
            runtime: self.runtime.clone(),
            vm,
            cwd: task.workdir,
            scope,
            artifacts: Some(guest_artifacts(&folder, &task.slug, run)),
            // The diff runs git only.
            pi_host: false,
            gemini: None,
            proxy: None,
        };
        // A dropped sender reads as a cancel, so it lives until the diff ends.
        let (_keep, mut cancel) = tokio::sync::watch::channel(false);
        let diff = match exec.diff(base.as_deref(), &mut cancel).await {
            Ok(diff) => diff,
            Err(SnapshotError::NotGit) => task_artifacts::NO_GIT_DIFF.to_owned(),
            Err(SnapshotError::Failed(why)) => format!("The app could not read the diff ({why})."),
        };
        Ok(if live { diff } else { format!("This run saved no diff. This is the current working dir.\n\n{diff}") })
    }
}

/// The guest path of a run's copy in the session folder.
pub(crate) fn guest_artifacts(folder: &str, slug: &str, run: &str) -> String {
    format!("{}/.zwinger/tasks/{slug}/{run}", folder.trim_end_matches('/'))
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
    /// The run that this run continues.
    pub continues: Option<String>,
    /// Whether an ended run can continue. `None` while the run is live.
    pub resume: Option<Resume>,
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

/// The guest folder that the git check and the diff of a task cover: the
/// session guest folder when the working dir is in it, else the working dir
/// in its mount. A working dir in neither is an error.
pub(crate) fn git_scope(task: &Task, folder: &str, mounts: &[Mount]) -> Result<String> {
    let workdir = Path::new(&task.workdir);
    if workdir.starts_with(folder) {
        Ok(folder.to_owned())
    } else if mounts.iter().any(|mount| workdir.starts_with(&mount.guest)) {
        Ok(task.workdir.clone())
    } else {
        Err(Error::TaskWorkdir(task.title.clone(), task.workdir.clone()))
    }
}

#[cfg(test)]
mod tests {
    use crate::cage::Mount;
    use crate::cage::testing::*;
    use crate::session::{Task, TaskAgent};
    use crate::task_artifacts::{self, RoundWriter};
    use crate::task_recovery::{self, Resume, RunRecord};
    use crate::task_runs::tests::{Fake, no_git, serve};
    use crate::task_runs::{Contribution, Control, RunState};

    fn task_in(workdir: &str) -> Task {
        Task {
            id: "t".into(),
            slug: "t".into(),
            title: "T".into(),
            request: "Do it.".into(),
            agents: [AgentKind::Pi, AgentKind::Claude].map(|kind| TaskAgent { kind, model: None }),
            gates: Vec::new(),
            workdir: workdir.into(),
        }
    }

    #[test]
    fn git_scope_is_the_session_folder_or_the_mount_workdir() {
        let mounts = [Mount { host: PathBuf::from("/h"), id: None, guest: "/mnt/repo".into(), readonly: false }];
        let scope = |workdir| super::git_scope(&task_in(workdir), "/workspace/a", &mounts);
        assert_eq!(scope("/workspace/a").unwrap(), "/workspace/a");
        assert_eq!(scope("/workspace/a/src").unwrap(), "/workspace/a");
        assert_eq!(scope("/mnt/repo/app").unwrap(), "/mnt/repo/app");
        assert!(matches!(scope("/workspace/ab"), Err(Error::TaskWorkdir(..))));
        assert!(matches!(scope("/workspace/b"), Err(Error::TaskWorkdir(..))));
    }

    #[tokio::test]
    async fn tasks_are_saved_and_a_run_needs_a_mounted_agent() {
        let (manager, _runtime, id) = manager();
        let kinds = [AgentKind::Pi, AgentKind::Claude];
        let task_id = manager.put_task(&id, task_draft(None, "Fix the map!", kinds)).unwrap();
        manager.put_task(&id, task_draft(None, "Fix the map", kinds)).unwrap();
        let tasks = manager.list().sessions[0].session.tasks.clone();
        assert_eq!(tasks.iter().map(|t| t.slug.as_str()).collect::<Vec<_>>(), ["fix-the-map", "fix-the-map-2"]);
        assert_eq!(
            (tasks[0].request.as_str(), tasks[0].gates.as_slice()),
            ("Fix the map.", ["pnpm build".to_owned()].as_slice())
        );
        // A rename keeps the folder of the runs.
        manager.put_task(&id, task_draft(Some(task_id.clone()), "Renamed", kinds)).unwrap();
        assert_eq!(manager.list().sessions[0].session.tasks[0].slug, "fix-the-map");
        assert!(matches!(manager.start_task_run(&id, &task_id), Err(Error::NotRunning)));

        // The harness does not control where the run copy goes.
        let mount = temp_dir();
        manager.add_mount(&id, mount.to_str().unwrap(), "/mnt/ro", true).unwrap();
        manager.set_harness(&id, Some("/mnt/ro")).unwrap();
        manager.start(&id).await.unwrap();
        assert!(matches!(manager.start_task_run(&id, &task_id), Err(Error::RuntimeMissing(name)) if name == "claude"));
        // A task edit needs no restart.
        manager.put_task(&id, task_draft(Some(task_id.clone()), "Again", kinds)).unwrap();
        assert!(!manager.list().cages[0].shared().pending_restart);

        manager.stop(&id, None).await.unwrap();
        manager.set_mount_readonly(&id, "/mnt/ro", false).unwrap();
        manager.start(&id).await.unwrap();
        assert!(matches!(manager.start_task_run(&id, &task_id), Err(Error::RuntimeMissing(name)) if name == "claude"));
        assert!(!mount.join(".zwinger").exists());
        assert!(manager.list_task_runs(&id).await.unwrap().is_empty());
        assert!(matches!(manager.round_dir(&id, &task_id, "01"), Err(Error::RunNotFound)));

        manager.remove_task(&id, &task_id).unwrap();
        assert_eq!(manager.list().sessions[0].session.tasks.len(), 1);
    }

    #[tokio::test]
    async fn a_task_can_pair_two_agents_of_one_kind() {
        let (manager, _runtime, id) = manager();
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Pi] {
            let mut draft = task_draft(None, &format!("Pair {}", kind.name()), [kind, kind]);
            draft.agents[0].model = Some(" provider/model.a ".into());
            draft.agents[1].model = Some("provider/model.b".into());
            let task_id = manager.put_task(&id, draft).unwrap();
            let saved = lock(&manager.state).session(&id).unwrap().task(&task_id).unwrap().agents.clone();
            assert_eq!(
                saved.map(|a| (a.kind, a.model)),
                [(kind, Some("provider/model.a".to_owned())), (kind, Some("provider/model.b".to_owned()))]
            );
            // Equal and default models are valid too, and the model checks stay.
            manager.put_task(&id, task_draft(Some(task_id.clone()), "Defaults", [kind, kind])).unwrap();
            let mut bad = task_draft(Some(task_id), "Bad", [kind, kind]);
            bad.agents[1].model = Some("two words".into());
            assert!(matches!(manager.put_task(&id, bad), Err(Error::InvalidTask(..))));
        }
    }

    #[tokio::test]
    async fn an_ended_run_shows_its_saved_diff_with_a_stopped_vm() {
        let (manager, _runtime, id) = manager();
        let task_id = manager.put_task(&id, task_draft(None, "Fix", [AgentKind::Pi, AgentKind::Claude])).unwrap();
        let round = manager.task_dir(&id, "fix").join("01");
        std::fs::create_dir_all(&round).unwrap();
        std::fs::write(round.join("events.jsonl"), "{\"at\":\"t\",\"kind\":\"done\",\"detail\":\"ship=true\"}\n")
            .unwrap();
        std::fs::write(round.join("diff.txt"), "the saved diff").unwrap();
        assert_eq!(manager.run_diff(&id, &task_id, "01").await.unwrap(), "the saved diff");

        std::fs::remove_file(round.join("diff.txt")).unwrap();
        assert_eq!(
            manager.run_diff(&id, &task_id, "01").await.unwrap(),
            "This run saved no diff. Start the cage to read the working dir."
        );
    }

    #[tokio::test]
    async fn converted_project_keeps_its_old_run_records() {
        let data = temp_dir();
        let folder = temp_dir();
        let project = serde_json::json!([{
            "id": "legacy", "name": "Old project", "folder": folder,
            "mounts": [], "network": { "default": "block", "offline": false, "rules": [] },
            "tasks": [{ "id": "old-task", "slug": "fix", "title": "Fix", "request": "Fix it",
                "agents": [{ "kind": "pi", "model": null }, { "kind": "claude", "model": null }],
                "gates": [], "workdir": "/workspace" }]
        }]);
        std::fs::write(data.join("projects.json"), project.to_string()).unwrap();
        let round = data.join("tasks/legacy/fix/01");
        std::fs::create_dir_all(&round).unwrap();
        std::fs::write(round.join("events.jsonl"), "{\"at\":\"t\",\"kind\":\"stage\",\"detail\":\"plan\"}\n{\"at\":\"t\",\"kind\":\"done\",\"detail\":\"ship=false\"}\n").unwrap();
        let manager =
            Manager::new(FakeRuntime::default(), Store::new(data.clone()), vault(), data.clone(), host()).unwrap();

        let runs = manager.list_task_runs("legacy").await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].task_id.as_str(), runs[0].run.as_str(), runs[0].state), ("old-task", "01", RunState::Done));
        assert_eq!(manager.round_dir("legacy", "old-task", "01").unwrap(), round);
        assert!(data.join("projects.json").exists());
    }

    /// A running cage with claude, a task, and its run 01 that a cancel
    /// stopped in the plan. The agents of a later run hang until a cancel.
    async fn ended_run() -> (Manager<FakeRuntime>, String, String, PathBuf) {
        let (manager, runtime, id) = manager();
        serve(&runtime, Arc::new(no_git), Arc::new(|_: &str, _, _: &str| Fake::Hang));
        runtimes::fake_install(&manager.data, "claude");
        let task_id = manager.put_task(&id, task_draft(None, "Fix", [AgentKind::Pi, AgentKind::Claude])).unwrap();
        manager.start(&id).await.unwrap();
        let task = lock(&manager.state).session(&id).unwrap().task(&task_id).unwrap().clone();
        let round = manager.task_dir(&id, "fix").join("01");
        let writer = RoundWriter::create(round.clone(), None).unwrap();
        let record = serde_json::to_vec(&RunRecord::new(&task, manager.run_config)).unwrap();
        writer.write_durable(task_recovery::RUN, &record).unwrap();
        let checkpoint = serde_json::to_vec(&task_recovery::Checkpoint::empty()).unwrap();
        writer.write_durable(task_recovery::CHECKPOINT, &checkpoint).unwrap();
        let events = "{\"at\":\"t\",\"kind\":\"stage\",\"detail\":\"plan\"}\n{\"at\":\"t\",\"kind\":\"cancelled\",\"detail\":\"cancelled by the user\"}\n";
        writer.write_durable("events.jsonl", events.as_bytes()).unwrap();
        (manager, id, task_id, round)
    }

    async fn cancel(manager: &Manager<FakeRuntime>, id: &str, task_id: &str, run: &str) {
        let handle = manager.run_handle(id, task_id, run).unwrap();
        handle.control(Control::Cancel { reason: None }).unwrap();
        tokio::time::timeout(Duration::from_secs(10), handle.settled()).await.unwrap();
    }

    fn record(dir: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(dir.join("run.json")).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn an_ended_run_continues_in_a_new_run_with_its_recorded_values() {
        let (manager, id, task_id, round) = ended_run().await;
        // A task edit after the stop does not change the values of the run.
        let mut edit = task_draft(Some(task_id.clone()), "Renamed", [AgentKind::Pi, AgentKind::Claude]);
        edit.gates = vec!["cargo test".into()];
        manager.put_task(&id, edit).unwrap();
        let runs = manager.list_task_runs(&id).await.unwrap();
        assert_eq!(
            runs[0].resume,
            Some(Resume::Ready { from: "plan".into(), label: "Plan".into(), repeats: true, recheck: None })
        );

        assert_eq!(manager.resume_task_run(&id, &task_id, "01").unwrap(), "02");
        let next = round.with_file_name("02");
        let written = record(&next);
        assert_eq!(written["title"], "Fix");
        assert_eq!(written["gates"], serde_json::json!(["pnpm build"]));
        assert_eq!(written["agents"][0]["model"], serde_json::Value::Null);
        assert_eq!(written["continues"], serde_json::json!({ "run": "01", "from": "plan", "repeats": true }));
        assert!(next.join("checkpoint.json").exists());
        let runs = manager.list_task_runs(&id).await.unwrap();
        assert_eq!((runs[1].continues.as_deref(), runs[1].resume.as_ref()), (Some("01"), None));

        // A repeated Resume names the newer run; a live run does not resume.
        let again = manager.resume_task_run(&id, &task_id, "01").unwrap_err();
        assert!(again.to_string().contains("run 02 continues this run; resume run 02"), "{again}");
        assert!(manager.resume_task_run(&id, &task_id, "02").unwrap_err().to_string().contains("still live"));
        assert!(matches!(manager.start_task_run(&id, &task_id), Err(Error::TaskRunning)));

        cancel(&manager, &id, &task_id, "02").await;
        let runs = manager.list_task_runs(&id).await.unwrap();
        assert!(matches!(&runs[0].resume, Some(Resume::Unavailable { unavailable }) if unavailable.contains("run 02")));
        assert_eq!(manager.resume_task_run(&id, &task_id, "02").unwrap(), "03");
        assert_eq!(record(&round.with_file_name("03"))["continues"]["run"], "02");
        cancel(&manager, &id, &task_id, "03").await;
        // The source keeps its record.
        assert!(std::fs::read_to_string(round.join("events.jsonl")).unwrap().contains("cancelled"));
    }

    #[tokio::test]
    async fn git_actions_need_a_shipped_run_with_a_recorded_start() {
        use crate::run_git::{Origin, Repo, Request, START};

        let (manager, id, task_id, round) = ended_run().await;
        let refused =
            async || manager.review_run_git(&id, &task_id, "01", Request::default()).await.unwrap_err().to_string();
        assert!(refused().await.contains("only a run that ended with ship"));
        std::fs::write(round.join("events.jsonl"), "{\"at\":\"t\",\"kind\":\"done\",\"detail\":\"ship=false\"}\n")
            .unwrap();
        std::fs::write(round.join("manifest.json"), "{\"ship\": false}").unwrap();
        assert!(refused().await.contains("only a run that ended with ship"));
        std::fs::write(round.join("manifest.json"), "{\"ship\": true}").unwrap();
        assert!(refused().await.contains("state at start unknown"), "{}", refused().await);

        // A recorded start whose folder is not mapped into the VM as it was.
        let mut written = record(&round);
        let origin = Origin {
            host: "/elsewhere".into(),
            guest: "/workspace".into(),
            workdir: "/workspace".into(),
            scope: "/workspace".into(),
        };
        written["origin"] = serde_json::to_value(&origin).unwrap();
        std::fs::write(round.join("run.json"), written.to_string()).unwrap();
        std::fs::write(round.join(START), serde_json::to_vec(&Repo::default()).unwrap()).unwrap();
        assert!(refused().await.contains("not mapped into the VM"), "{}", refused().await);

        manager.stop(&id, None).await.unwrap();
        assert!(refused().await.contains("start the cage"), "{}", refused().await);
        assert!(manager.run_git_operation(&id, &task_id, "01", "t").await.unwrap().is_none());
        assert!(manager.run_git_unfinished(&id, &task_id, "01").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_same_kind_pair_resumes_with_its_recorded_participants() {
        let (manager, runtime, id) = manager();
        serve(&runtime, Arc::new(no_git), Arc::new(|_: &str, _, _: &str| Fake::Hang));
        runtimes::fake_install(&manager.data, "claude");
        let mut draft = task_draft(None, "Fix", [AgentKind::Claude, AgentKind::Claude]);
        draft.agents[0].model = Some("opus".into());
        draft.agents[1].model = Some("sonnet".into());
        let task_id = manager.put_task(&id, draft).unwrap();
        manager.start(&id).await.unwrap();
        assert_eq!(manager.start_task_run(&id, &task_id).unwrap(), "01");
        assert_eq!(manager.run_handle(&id, &task_id, "01").unwrap().agents, ["claude-1", "claude-2"]);
        cancel(&manager, &id, &task_id, "01").await;

        // A later edit to a mixed pair does not change the participants of the run.
        manager.put_task(&id, task_draft(Some(task_id.clone()), "Fix", [AgentKind::Pi, AgentKind::Claude])).unwrap();
        assert_eq!(manager.resume_task_run(&id, &task_id, "01").unwrap(), "02");
        assert_eq!(manager.run_handle(&id, &task_id, "02").unwrap().agents, ["claude-1", "claude-2"]);
        let round = manager.round_dir(&id, &task_id, "02").unwrap();
        // The run records where its working dir was, and a continuation keeps it.
        let first = record(&manager.round_dir(&id, &task_id, "01").unwrap())["origin"].clone();
        assert_eq!(first["workdir"], guest_folder(&manager, &id));
        assert_eq!(record(&round)["origin"], first);
        assert_eq!(record(&round)["agents"][1]["model"], "sonnet");
        let room = task_artifacts::read_room(&round);
        assert_eq!(room.agents, ["claude-1", "claude-2"]);
        assert_eq!(room.participants[0].model.as_deref(), Some("opus"));
        cancel(&manager, &id, &task_id, "02").await;
    }

    fn guest_folder(manager: &Manager<FakeRuntime>, id: &str) -> String {
        lock(&manager.state).task_cage(id).unwrap().3
    }

    /// Edits the task to work in `/workspace/sub`.
    fn move_workdir(manager: &Manager<FakeRuntime>, id: &str, task_id: &str) {
        let folder = lock(&manager.state).session(id).unwrap().folder.clone();
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        let mut edit = task_draft(Some(task_id.to_owned()), "Fix", [AgentKind::Pi, AgentKind::Claude]);
        edit.workdir = "/workspace/sub".into();
        manager.put_task(id, edit).unwrap();
    }

    #[tokio::test]
    async fn the_diff_of_a_continuation_reads_its_recorded_workdir() {
        let (manager, id, task_id, round) = ended_run().await;
        let folder = guest_folder(&manager, &id);
        let cwds = manager.runtime.cwds.clone();
        assert_eq!(manager.resume_task_run(&id, &task_id, "01").unwrap(), "02");
        move_workdir(&manager, &id, &task_id);

        // Live: the diff runs in the working dir that run 02 recorded.
        assert_eq!(manager.run_diff(&id, &task_id, "02").await.unwrap(), task_artifacts::NO_GIT_DIFF);
        assert_eq!(cwds.lock().unwrap().last(), Some(&folder));

        // Ended with no saved diff: the same working dir.
        cancel(&manager, &id, &task_id, "02").await;
        assert!(!round.with_file_name("02").join("diff.txt").exists());
        let diff = manager.run_diff(&id, &task_id, "02").await.unwrap();
        assert!(diff.starts_with("This run saved no diff. This is the current working dir."), "{diff}");
        assert_eq!(cwds.lock().unwrap().last(), Some(&folder));
    }

    #[tokio::test]
    async fn the_diff_of_a_run_with_no_record_reads_the_task_workdir() {
        let (manager, id, task_id, round) = ended_run().await;
        let folder = guest_folder(&manager, &id);
        std::fs::remove_file(round.join("run.json")).unwrap();
        move_workdir(&manager, &id, &task_id);
        manager.run_diff(&id, &task_id, "01").await.unwrap();
        assert_eq!(manager.runtime.cwds.lock().unwrap().last(), Some(&format!("{folder}/sub")));
    }

    #[tokio::test]
    async fn a_damaged_record_blocks_only_a_diff_from_the_vm() {
        let (manager, id, task_id, round) = ended_run().await;
        std::fs::write(round.join("run.json"), "{").unwrap();
        // With no checkpoint, the room reads `diff.txt`.
        std::fs::remove_file(round.join("checkpoint.json")).unwrap();
        std::fs::write(round.join("diff.txt"), "the saved diff").unwrap();
        assert_eq!(manager.run_diff(&id, &task_id, "01").await.unwrap(), "the saved diff");

        std::fs::remove_file(round.join("diff.txt")).unwrap();
        let error = manager.run_diff(&id, &task_id, "01").await.unwrap_err().to_string();
        assert!(error.starts_with("The app could not read the run record (run.json is damaged"), "{error}");

        manager.stop(&id, None).await.unwrap();
        assert_eq!(
            manager.run_diff(&id, &task_id, "01").await.unwrap(),
            "This run saved no diff. Start the cage to read the working dir."
        );
    }

    #[tokio::test]
    async fn resume_refuses_with_a_reason_and_starts_nothing() {
        let (manager, id, task_id, round) = ended_run().await;
        let refused = |run: &str| manager.resume_task_run(&id, &task_id, run).unwrap_err().to_string();

        std::fs::rename(round.join("run.json"), round.join("run.json.keep")).unwrap();
        assert!(refused("01").contains("before zwinger saved recovery records"));
        let runs = manager.list_task_runs(&id).await.unwrap();
        assert!(matches!(&runs[0].resume, Some(Resume::Unavailable { .. })));
        std::fs::write(round.join("run.json"), "{").unwrap();
        assert!(refused("01").contains("run.json is damaged"));
        std::fs::rename(round.join("run.json.keep"), round.join("run.json")).unwrap();
        let checkpoint = std::fs::read(round.join("checkpoint.json")).unwrap();
        std::fs::write(round.join("checkpoint.json"), "{\"version\": 7}").unwrap();
        assert!(refused("01").contains("version 7"));
        // A run that started its stages has lost its state without its checkpoint.
        std::fs::remove_file(round.join("checkpoint.json")).unwrap();
        assert!(refused("01").contains("checkpoint.json is missing"), "{}", refused("01"));
        std::fs::write(round.join("checkpoint.json"), checkpoint).unwrap();
        assert!(matches!(manager.resume_task_run(&id, &task_id, "09"), Err(Error::RunNotFound)));

        manager.stop(&id, None).await.unwrap();
        assert!(matches!(manager.resume_task_run(&id, &task_id, "01"), Err(Error::NotRunning)));
        assert_eq!(super::run_names(round.parent().unwrap()), ["01"], "a stopped cage starts no run");
    }

    #[tokio::test]
    async fn a_continuation_that_stopped_before_its_checkpoint_resolves_back() {
        let (manager, id, task_id, round) = ended_run().await;
        // A crash during the creation of run 02: its record has the link, but no checkpoint.
        let mut link = record(&round);
        link["continues"] = serde_json::json!({ "run": "01", "from": "plan", "repeats": true });
        let broken = round.with_file_name("02");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("run.json"), link.to_string()).unwrap();

        let error = manager.resume_task_run(&id, &task_id, "01").unwrap_err().to_string();
        assert!(error.contains("resume run 02"), "{error}");
        assert_eq!(manager.resume_task_run(&id, &task_id, "02").unwrap(), "03");
        let next = round.with_file_name("03");
        assert_eq!(record(&next)["continues"], serde_json::json!({ "run": "02", "from": "plan", "repeats": true }));
        cancel(&manager, &id, &task_id, "03").await;
    }

    #[tokio::test]
    async fn a_run_that_a_cage_stop_ended_continues_in_a_new_app_session() {
        let (manager, runtime, id) = manager();
        serve(&runtime, Arc::new(no_git), Arc::new(|_: &str, _, _: &str| Fake::Hang));
        runtimes::fake_install(&manager.data, "claude");
        let task_id = manager.put_task(&id, task_draft(None, "Fix", [AgentKind::Pi, AgentKind::Claude])).unwrap();
        manager.start(&id).await.unwrap();
        assert_eq!(manager.start_task_run(&id, &task_id).unwrap(), "01");
        let handle = manager.run_handle(&id, &task_id, "01").unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while handle.status().stage.as_deref() != Some("plan") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        manager.stop(&id, None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), handle.settled()).await.unwrap();
        let round = manager.task_dir(&id, "fix").join("01");
        assert!(std::fs::read_to_string(round.join("events.jsonl")).unwrap().contains("the cage stopped"));

        // A new app session on the same data: the run has no handle, only its records.
        let fresh = Manager::new(
            runtime.clone(),
            Store::new(manager.store.dir().to_owned()),
            vault(),
            manager.data.clone(),
            manager.host.clone(),
        )
        .unwrap();
        drop(manager);
        assert!(fresh.run_handle(&id, &task_id, "01").is_none());
        let runs = fresh.list_task_runs(&id).await.unwrap();
        assert_eq!(
            runs[0].resume,
            Some(Resume::Ready { from: "plan".into(), label: "Plan".into(), repeats: true, recheck: None })
        );
        assert!(matches!(fresh.resume_task_run(&id, &task_id, "01"), Err(Error::NotRunning)));
        fresh.start(&id).await.unwrap();
        assert_eq!(fresh.resume_task_run(&id, &task_id, "01").unwrap(), "02");

        // The room of the continuation, as `watch_room` sends it.
        let next = fresh.round_dir(&id, &task_id, "02").unwrap();
        let live = fresh.run_handle(&id, &task_id, "02").unwrap();
        let watches = crate::room_watch::Watches::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let token = watches.watch("main", next, Some(live), false, move |event| tx.send(event).is_ok());
        let (room, state) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let crate::room_watch::RoomEvent::Room { room, run, .. } = rx.recv().await.unwrap()
                    && room.stages.iter().any(|s| s.id == "plan" && s.state == "active")
                {
                    return (room, run.unwrap().state);
                }
            }
        })
        .await
        .unwrap();
        watches.retire(token);
        assert!(state.is_live());
        let plan = room.stages.iter().find(|s| s.id == "plan").unwrap();
        assert_eq!(plan.label, "Plan (repeated)");
        cancel(&fresh, &id, &task_id, "02").await;
    }

    /// A process kill at each point of a message write, as the prefix that the
    /// product write order persists: the reservation, the apply-start, the
    /// message line, the apply-complete. A kill after a complete message line
    /// and a kill before the apply-complete persist the same prefix.
    #[tokio::test]
    async fn a_kill_during_a_message_write_resumes_with_its_charge() {
        use serde_json::{Value, json};

        for external in [false, true] {
            for kill in ["after apply-start", "after a partial message line", "before apply-complete"] {
                let (manager, id, task_id, round) = ended_run().await;
                let body = if external { "Mind the cache." } else { "Keep it small." };
                let digest = task_recovery::digest(body.as_bytes());
                let (reserve, start, line) = if external {
                    (
                        json!({ "at": "t", "kind": "external", "bytes": body.len(), "ref": "note-1", "digest": digest }),
                        json!({ "kind": "apply-start", "ref": "note-1" }),
                        json!({ "at": "t", "stage": "plan", "kind": "external", "from": crate::task_runs::EXTERNAL, "ref": "note-1", "body": body }),
                    )
                } else {
                    (
                        json!({ "at": "t", "kind": "message", "bytes": body.len() }),
                        json!({ "kind": "apply-start", "digest": digest }),
                        json!({ "at": "t", "stage": "plan", "kind": "user", "body": body }),
                    )
                };
                std::fs::write(round.join("budget.jsonl"), format!("{reserve}\n{start}\n")).unwrap();
                let line = line.to_string();
                let messages = match kill {
                    "after apply-start" => String::new(),
                    "after a partial message line" => line[..10].to_owned(),
                    _ => line + "\n",
                };
                std::fs::write(round.join("messages.jsonl"), messages).unwrap();
                let applied = kill == "before apply-complete";

                let runs = manager.list_task_runs(&id).await.unwrap();
                assert!(matches!(runs[0].resume, Some(Resume::Ready { .. })), "{kill}: {:?}", runs[0].resume);
                assert_eq!(manager.resume_task_run(&id, &task_id, "01").unwrap(), "02", "{kill}");
                let next = round.with_file_name("02");
                let ledger = || -> Vec<Value> {
                    let text = std::fs::read_to_string(next.join("budget.jsonl")).unwrap();
                    text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
                };
                let kinds: Vec<String> = ledger().iter().map(|l| l["kind"].as_str().unwrap().to_owned()).collect();
                let charge = reserve["kind"].as_str().unwrap();
                let expected: &[&str] = if applied { &[charge, "apply-start", "apply-complete"] } else { &[charge] };
                assert_eq!(kinds, expected, "{kill}");
                assert_eq!(ledger()[0]["bytes"], body.len(), "{kill}");
                let copied = std::fs::read_to_string(next.join("messages.jsonl")).unwrap_or_default();
                assert_eq!(copied.lines().count(), usize::from(applied), "{kill}: {copied}");
                assert_eq!(copied.contains(body), applied, "{kill}: the text is not restored");

                if external {
                    // The id stays reserved or applied as the record shows, with no new charge.
                    let handle = manager.run_handle(&id, &task_id, "02").unwrap();
                    match handle.contribute("note-1", body) {
                        Ok(Contribution::Applied) => assert!(applied, "{kill}"),
                        Ok(Contribution::Queued(done)) => {
                            assert!(!applied, "{kill}");
                            tokio::time::timeout(Duration::from_secs(10), done).await.unwrap().unwrap();
                        }
                        other => panic!("{kill}: {other:?}"),
                    }
                    let charges = ledger().iter().filter(|l| l["kind"] == "external").count();
                    assert_eq!(charges, 1, "{kill}");
                }
                cancel(&manager, &id, &task_id, "02").await;
            }
        }
    }

    /// A failed write, then a crash before the run records the failure:
    /// the budget keeps the failure, so Resume refuses.
    #[test]
    fn a_write_failure_before_a_crash_gives_no_resume() {
        let app = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let (manager, id, task_id, round) = app.block_on(async {
            let (manager, runtime, id) = manager();
            serve(&runtime, Arc::new(no_git), Arc::new(|_: &str, _, _: &str| Fake::Hang));
            runtimes::fake_install(&manager.data, "claude");
            let task_id = manager.put_task(&id, task_draft(None, "Fix", [AgentKind::Pi, AgentKind::Claude])).unwrap();
            manager.start(&id).await.unwrap();
            assert_eq!(manager.start_task_run(&id, &task_id).unwrap(), "01");
            let handle = manager.run_handle(&id, &task_id, "01").unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while handle.status().stage.as_deref() != Some("plan") {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let round = manager.task_dir(&id, "fix").join("01");
            std::fs::create_dir(round.join("messages.jsonl")).unwrap();
            let Ok(Contribution::Queued(applied)) = handle.contribute("m-1", "Check the cache.") else {
                panic!("a new contribution is queued");
            };
            assert!(tokio::time::timeout(Duration::from_secs(5), applied).await.unwrap().is_err());
            assert!(handle.status().state.is_live(), "the run did not settle");
            (manager, id, task_id, round)
        });
        // The crash: the run task ends with no settlement.
        drop(app);
        let (store, data, host) = (manager.store.dir().to_owned(), manager.data.clone(), manager.host.clone());
        drop(manager);
        let events = std::fs::read_to_string(round.join("events.jsonl")).unwrap();
        assert!(!events.contains("run_failed"), "{events}");
        std::fs::remove_dir(round.join("messages.jsonl")).unwrap();

        let app = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        app.block_on(async {
            let runtime = FakeRuntime::default();
            let (calls, shells) = serve(&runtime, Arc::new(no_git), Arc::new(|_: &str, _, _: &str| Fake::Hang));
            let fresh = Manager::new(runtime, Store::new(store), vault(), data, host).unwrap();
            fresh.start(&id).await.unwrap();
            let before = shells.lock().unwrap().len();
            let error = fresh.resume_task_run(&id, &task_id, "01").unwrap_err().to_string();
            assert!(error.contains("could not write its record"), "{error}");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(shells.lock().unwrap().len(), before);
            assert!(calls.lock().unwrap().is_empty());
            assert_eq!(super::run_names(round.parent().unwrap()), ["01"]);
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_resumes_start_one_continuation() {
        let (manager, id, task_id, _) = ended_run().await;
        let manager = Arc::new(manager);
        let resumes: Vec<_> = (0..2)
            .map(|_| {
                let (manager, id, task_id) = (manager.clone(), id.clone(), task_id.clone());
                tokio::task::spawn_blocking(move || manager.resume_task_run(&id, &task_id, "01"))
            })
            .collect();
        let mut started = Vec::new();
        for resume in resumes {
            if let Ok(run) = resume.await.unwrap() {
                started.push(run);
            }
        }
        assert_eq!(started, ["02"]);
        cancel(&manager, &id, &task_id, "02").await;
    }
}
