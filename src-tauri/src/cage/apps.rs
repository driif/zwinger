use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;

use crate::cage::AppDraft;
use crate::cage::manager::{Manager, Vm, lock};
use crate::error::{Error, Result};
use crate::runtime::{PtyInput, SandboxRuntime, SessionKind};
use crate::terminal::Session;
use crate::terminal::output::{Output, pump};

/// How long a stopped app has to exit before its restart fails.
pub(crate) const CLOSE_LIMIT: Duration = Duration::from_secs(5);

/// The terminal size of an app until a view attaches and resizes it.
pub(crate) const APP_SIZE: (u16, u16) = (24, 80);

pub(crate) struct AppRun {
    pub(crate) app_id: String,
    /// Becomes `true` when the process has exited.
    pub(crate) exited: watch::Receiver<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRunView {
    pub app_id: String,
    pub session_id: String,
    pub exit_code: Option<i32>,
}

impl<R: SandboxRuntime> Manager<R> {
    pub fn put_app(&self, id: &str, draft: AppDraft) -> Result<String> {
        let mut app_id = String::new();
        self.edit(id, |s| {
            app_id = s.put_app(draft)?;
            Ok(())
        })?;
        Ok(app_id)
    }

    /// Also stops the app.
    pub fn remove_app(&self, id: &str, app_id: &str) -> Result<()> {
        self.edit(id, |s| s.remove_app(app_id))?;
        self.stop_app(id, app_id)
    }

    pub fn reorder_apps(&self, id: &str, app_ids: &[String]) -> Result<()> {
        self.edit(id, |s| s.reorder_apps(app_ids))
    }

    /// Runs the app's saved command as `/bin/sh -lc` in a PTY that the
    /// backend owns, and returns its session id. A run that has exited is
    /// replaced. Fails if the VM is not running, or if it stopped or started
    /// again while the process opened.
    pub async fn start_app(&self, project_id: &str, app_id: &str) -> Result<String> {
        let key = (project_id.to_owned(), app_id.to_owned());
        let (vm, app, stops) = {
            let mut state = lock(&self.state);
            let app = state.project_mut(project_id)?.app(app_id)?.clone();
            let stops = state.app_stops.get(&key).copied().unwrap_or(0);
            if state
                .sessions
                .iter()
                .any(|s| s.app.as_ref().is_some_and(|r| r.app_id == app_id) && s.exit_code.is_none())
            {
                return Err(Error::AppRunning);
            }
            match state.vms.get(project_id) {
                Some(Vm::Running { vm, .. }) => (vm.clone(), app, stops),
                _ => return Err(Error::NotRunning),
            }
        };
        let argv = ["/bin/sh".to_owned(), "-lc".to_owned(), app.command];
        let (rows, cols) = APP_SIZE;
        let pty = self.runtime.open_pty(&vm, &argv, &app.cwd, rows, cols).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let output = Arc::new(Mutex::new(Output::default()));
        let (exited_tx, exited) = watch::channel(false);
        {
            let mut state = lock(&self.state);
            let same_vm = matches!(
                state.vms.get(project_id),
                Some(Vm::Running { vm: current, .. }) if Arc::ptr_eq(current, &vm)
            );
            let running = state
                .sessions
                .iter()
                .any(|s| s.app.as_ref().is_some_and(|r| r.app_id == app_id) && s.exit_code.is_none());
            // A stop or delete of the app, or a second start, ran while the
            // process opened.
            let error = if !same_vm {
                Some(Error::NotRunning)
            } else if state.project_mut(project_id).and_then(|s| s.app(app_id)).is_err() {
                Some(Error::AppNotFound)
            } else if state.app_stops.get(&key).copied().unwrap_or(0) != stops {
                Some(Error::AppStopped)
            } else if running {
                Some(Error::AppRunning)
            } else {
                None
            };
            if let Some(error) = error {
                let _ = pty.input.send(PtyInput::Kill);
                return Err(error);
            }
            state.sessions.retain(|s| s.app.as_ref().is_none_or(|r| r.app_id != app_id));
            state.sessions.push(Session {
                id: id.clone(),
                project_id: project_id.to_owned(),
                kind: SessionKind::Shell,
                host_group: None,
                input: pty.input,
                exit_code: None,
                output: output.clone(),
                terminal_id: None,
                app: Some(AppRun { app_id: app_id.to_owned(), exited }),
            });
        }

        let state = self.state.clone();
        let session_id = id.clone();
        tokio::spawn(async move {
            pump(state, session_id, output, pty.events).await;
            let _ = exited_tx.send(true);
        });
        Ok(id)
    }

    /// Kills the app's run and forgets it, so the app shows as stopped. A start
    /// that is in progress fails. An app with no run is not an error.
    pub fn stop_app(&self, project_id: &str, app_id: &str) -> Result<()> {
        self.kill_app(project_id, app_id);
        Ok(())
    }

    /// Does the work of `stop_app` and returns the exit of the killed run.
    pub(crate) fn kill_app(&self, project_id: &str, app_id: &str) -> Option<watch::Receiver<bool>> {
        let mut state = lock(&self.state);
        *state.app_stops.entry((project_id.to_owned(), app_id.to_owned())).or_default() += 1;
        let mut exited = None;
        state.sessions.retain(|s| match &s.app {
            Some(run) if s.project_id == project_id && run.app_id == app_id => {
                let _ = s.input.send(PtyInput::Kill);
                exited = Some(run.exited.clone());
                false
            }
            _ => true,
        });
        exited
    }

    /// Starts the app again after its old run has exited, so the new run can
    /// take the ports of the old one.
    pub async fn restart_app(&self, project_id: &str, app_id: &str) -> Result<String> {
        if let Some(mut exited) = self.kill_app(project_id, app_id) {
            // An error means that the run task ended, so the process is gone.
            let wait = exited.wait_for(|done| *done);
            if tokio::time::timeout(CLOSE_LIMIT, wait).await.is_err() {
                return Err(Error::AppStillRunning);
            }
        }
        self.start_app(project_id, app_id).await
    }
}

#[cfg(test)]
mod tests {
    use crate::cage::testing::*;

    #[tokio::test]
    async fn autostart_runs_apps_after_start() {
        let (manager, runtime, id) = manager();
        let web = app(&manager, &id, "pnpm dev", true);
        app(&manager, &id, "cargo run", false);
        assert!(matches!(manager.start_app(&id, &web).await, Err(Error::NotRunning)));

        manager.start(&id).await.unwrap();
        assert_eq!(runs(&manager), [(web.clone(), None)]);
        assert!(manager.list()[0].sessions.is_empty());
        assert_eq!(runtime.argvs.lock().unwrap().last().unwrap(), &["/bin/sh", "-lc", "pnpm dev"]);
        assert_eq!(runtime.cwds.lock().unwrap().last().unwrap(), WORKSPACE);
        assert!(matches!(manager.start_app(&id, &web).await, Err(Error::AppRunning)));
    }

    #[tokio::test]
    async fn stop_ends_apps() {
        let (manager, runtime, id) = manager();
        app(&manager, &id, "pnpm dev", true);
        manager.start(&id).await.unwrap();
        manager.stop(&id).await.unwrap();
        assert!(runs(&manager).is_empty());
        until(|| runtime.log.lock().unwrap().contains(&"kill".to_owned())).await;
    }

    #[tokio::test]
    async fn app_output_is_kept_for_a_later_attach() {
        let (manager, _, id) = manager();
        let web = app(&manager, &id, "pnpm dev", false);
        manager.start(&id).await.unwrap();
        let session = manager.start_app(&id, &web).await.unwrap();
        manager.write(&session, b"ready".to_vec()).unwrap();

        let attach = || attach(&manager, &session).1;
        let mut rx = attach();
        // The echo of the write reaches the ring in a task.
        while rx.try_recv() != Ok(output("24x80ready")) {
            tokio::task::yield_now().await;
            rx = attach();
        }
        manager.write(&session, b"!".to_vec()).unwrap();
        assert_eq!(next(&mut rx).await, output("!"));

        manager.send(&session, PtyInput::Kill).unwrap();
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
        assert_eq!(runs(&manager), [(web.clone(), Some(137))]);

        let mut rx = attach();
        assert_eq!(next(&mut rx).await, output("24x80ready!"));
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));

        let restarted = manager.start_app(&id, &web).await.unwrap();
        assert_ne!(restarted, session);
        assert_eq!(runs(&manager), [(web.clone(), None)]);
        manager.stop_app(&id, &web).unwrap();
        assert!(runs(&manager).is_empty());
        let session = manager.restart_app(&id, &web).await.unwrap();
        let again = manager.restart_app(&id, &web).await.unwrap();
        assert_ne!(again, session);
        assert_eq!(runs(&manager), [(web, None)]);
    }

    #[tokio::test]
    async fn app_started_during_stop_is_killed() {
        let (manager, runtime, id) = manager();
        let web = app(&manager, &id, "pnpm dev", false);
        manager.start(&id).await.unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.pty_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let starting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.start_app(&id, &web).await }
        });
        gate.entered.notified().await;
        manager.stop(&id).await.unwrap();
        *runtime.pty_gate.lock().unwrap() = None;
        manager.start(&id).await.unwrap();
        gate.release.notify_one();

        assert!(matches!(starting.await.unwrap(), Err(Error::NotRunning)));
        assert!(runs(&manager).is_empty());
        until(|| runtime.log.lock().unwrap().contains(&"kill".to_owned())).await;
    }

    #[tokio::test]
    async fn app_stopped_during_its_start_does_not_run() {
        app_start_during(false).await;
    }

    #[tokio::test]
    async fn app_removed_during_its_start_does_not_run() {
        app_start_during(true).await;
    }

    #[tokio::test]
    async fn restart_waits_for_the_old_run_to_exit() {
        let (manager, runtime, id) = manager();
        let web = app(&manager, &id, "pnpm dev", false);
        manager.start(&id).await.unwrap();
        let old = manager.start_app(&id, &web).await.unwrap();
        let opened = runtime.argvs.lock().unwrap().len();
        let gate = Arc::new(PtyGate::default());
        *runtime.kill_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let restarting = tokio::spawn({
            let (manager, id, web) = (manager.clone(), id.clone(), web.clone());
            async move { manager.restart_app(&id, &web).await }
        });
        gate.entered.notified().await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.argvs.lock().unwrap().len(), opened, "the new run started first");
        gate.release.notify_one();

        let new = restarting.await.unwrap().unwrap();
        assert_ne!(new, old);
        assert_eq!(runs(&manager), [(web, None)]);
    }
}
