pub(crate) mod output;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::AgentKind;
use crate::error::{Error, Result};
use crate::runtime::{PtyEvent, PtyInput, SandboxRuntime, SessionKind};
use crate::runtimes;
use crate::shield::WORKSPACE;
use crate::shield::apps::AppRun;
use crate::shield::lifecycle::mounted;
use crate::shield::manager::{HostRun, Manager, State, Vm, lock};
use output::{Output, pump};

/// The backend owns each session, not a view: a view attaches to it and
/// detaches.
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) shield_id: String,
    pub(crate) kind: SessionKind,
    pub(crate) input: UnboundedSender<PtyInput>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) output: Arc<Mutex<Output>>,
    /// Set for a shell or agent terminal.
    pub(crate) terminal_id: Option<String>,
    /// Set for an app run.
    pub(crate) app: Option<AppRun>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: String,
    pub kind: SessionKind,
    pub exit_code: Option<i32>,
    pub terminal_id: Option<String>,
}

impl<V> State<V> {
    /// The terminal has a session whose process runs.
    pub(crate) fn has_live_terminal(&self, terminal_id: &str) -> bool {
        self.sessions.iter().any(|s| s.terminal_id.as_deref() == Some(terminal_id) && s.exit_code.is_none())
    }

    pub(crate) fn session(&self, id: &str) -> Result<&Session> {
        self.sessions.iter().find(|s| s.id == id).ok_or(Error::SessionNotFound)
    }
}

impl<R: SandboxRuntime> Manager<R> {
    pub fn reorder_terminals(&self, id: &str, terminal_ids: &[String]) -> Result<()> {
        self.edit(id, |s| s.reorder_terminals(terminal_ids))
    }

    /// Adds a terminal of `kind` to the end of the shield's list and opens
    /// it. Returns the session id. A failed open adds no terminal.
    pub async fn open_session(&self, shield_id: &str, kind: SessionKind, rows: u16, cols: u16) -> Result<String> {
        self.open_terminal(shield_id, None, kind, rows, cols).await
    }

    /// Opens a dormant terminal again: a new process under the same terminal
    /// id, in the same place. Returns the session id.
    pub async fn reopen_terminal(&self, shield_id: &str, terminal_id: &str, rows: u16, cols: u16) -> Result<String> {
        let kind = {
            let mut state = lock(&self.state);
            let kind = state.shield_mut(shield_id)?.terminal(terminal_id)?.kind;
            if state.has_live_terminal(terminal_id) {
                return Err(Error::TerminalOpen);
            }
            kind
        };
        self.open_terminal(shield_id, Some(terminal_id), kind, rows, cols).await
    }

    /// `terminal_id` `None` adds a new terminal.
    pub(crate) async fn open_terminal(
        &self,
        shield_id: &str,
        terminal_id: Option<&str>,
        kind: SessionKind,
        rows: u16,
        cols: u16,
    ) -> Result<String> {
        let (vm, cwd) = match lock(&self.state).vms.get(shield_id) {
            Some(Vm::Running { vm, applied, runtimes, host, .. }) => {
                let cwd = match kind {
                    SessionKind::Agent { agent, .. } if !mounted(runtimes).contains(&agent) => {
                        return Err(self.unmounted(agent, host));
                    }
                    SessionKind::Agent { .. } => applied.harness.as_deref(),
                    SessionKind::Shell => None,
                };
                (vm.clone(), cwd.unwrap_or(WORKSPACE).to_owned())
            }
            _ => return Err(Error::NotRunning),
        };
        let pty = self.runtime.open_pty(&vm, &kind.argv(), &cwd, rows, cols).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let output = Arc::new(Mutex::new(Output::default()));
        {
            let mut state = lock(&self.state);
            // The shield may have stopped, or stopped and started again, while
            // the process opened. Its stop did not see this session, so kill
            // it. A close or a second open of the terminal may also have run.
            let same_vm = matches!(
                state.vms.get(shield_id),
                Some(Vm::Running { vm: current, .. }) if Arc::ptr_eq(current, &vm)
            );
            let terminal = match terminal_id {
                _ if !same_vm => Err(Error::NotRunning),
                None => self.add_terminal(&mut state, shield_id, kind),
                Some(tid) => match state.shield_mut(shield_id).and_then(|s| s.terminal(tid).map(|_| ())) {
                    Err(e) => Err(e),
                    Ok(()) if state.has_live_terminal(tid) => Err(Error::TerminalOpen),
                    Ok(()) => Ok(tid.to_owned()),
                },
            };
            let terminal = match terminal {
                Ok(terminal) => terminal,
                Err(e) => {
                    let _ = pty.input.send(PtyInput::Kill);
                    return Err(e);
                }
            };
            state.sessions.retain(|s| s.terminal_id.as_ref() != Some(&terminal));
            state.sessions.push(Session {
                id: id.clone(),
                shield_id: shield_id.to_owned(),
                kind,
                input: pty.input,
                exit_code: None,
                output: output.clone(),
                terminal_id: Some(terminal),
                app: None,
            });
        }
        tokio::spawn(pump(self.state.clone(), id.clone(), output, pty.events));
        Ok(id)
    }

    /// Adds a terminal record and saves it. Returns its id.
    pub(crate) fn add_terminal(&self, state: &mut State<R::Vm>, shield_id: &str, kind: SessionKind) -> Result<String> {
        let mut shields = state.shields.clone();
        let shield = shields.iter_mut().find(|s| s.id == shield_id).ok_or(Error::ShieldNotFound)?;
        let terminal_id = shield.add_terminal(kind);
        self.store.save(&shields)?;
        state.shields = shields;
        Ok(terminal_id)
    }

    /// Sends the kept output of the session to `on_event`, and its exit if
    /// it has exited; then new output. A later attach takes over. Returns the
    /// token for [`Manager::detach_session`] and [`Manager::ack_session`].
    pub fn attach_session(&self, session_id: &str, on_event: impl Fn(PtyEvent) + Send + 'static) -> Result<u64> {
        let output = self.output(session_id)?;
        Ok(lock(&output).attach(Box::new(on_event)))
    }

    /// Stops the output to the view of `token`. A later attach keeps its sink,
    /// and an ended session is not an error.
    pub fn detach_session(&self, session_id: &str, token: u64) {
        if let Ok(output) = self.output(session_id) {
            lock(&output).detach(token);
        }
    }

    /// The view of `token` has written one more output frame.
    pub fn ack_session(&self, session_id: &str, token: u64) {
        if let Ok(output) = self.output(session_id) {
            lock(&output).ack(token);
        }
    }

    pub(crate) fn output(&self, session_id: &str) -> Result<Arc<Mutex<Output>>> {
        Ok(lock(&self.state).session(session_id)?.output.clone())
    }

    pub fn write(&self, id: &str, data: Vec<u8>) -> Result<()> {
        self.send(id, PtyInput::Data(data))
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<()> {
        self.send(id, PtyInput::Resize { rows, cols })
    }

    pub(crate) fn send(&self, id: &str, input: PtyInput) -> Result<()> {
        lock(&self.state).session(id)?.input.send(input).map_err(|_| Error::SessionClosed)
    }

    /// Removes the terminal and kills its process. An unknown terminal is
    /// not an error: it is already gone.
    pub fn close_terminal(&self, shield_id: &str, terminal_id: &str) -> Result<()> {
        self.edit(shield_id, |s| {
            s.remove_terminal(terminal_id);
            Ok(())
        })?;
        lock(&self.state).sessions.retain(|s| {
            let closed = s.terminal_id.as_deref() == Some(terminal_id);
            if closed {
                let _ = s.input.send(PtyInput::Kill);
            }
            !closed
        });
        Ok(())
    }

    /// Why the running VM has no runtime of the agent.
    pub(crate) fn unmounted(&self, kind: AgentKind, host: &HostRun) -> Error {
        let name = kind.name().to_owned();
        if kind == AgentKind::Gemini {
            return match host.failed.iter().find(|(k, _)| *k == kind) {
                Some((_, why)) => Error::HostAgentFailed(kind.label().to_owned(), why.clone()),
                None => Error::NotHostAgent(kind.label().to_owned()),
            };
        }
        if runtimes::installed(&self.data, &HashMap::new()).iter().any(|r| r.name == name) {
            Error::RuntimeNotMounted(name)
        } else {
            Error::RuntimeMissing(name)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::shield::testing::*;

    #[tokio::test]
    async fn session_lifecycle() {
        let (manager, runtime, id) = manager();
        assert!(matches!(manager.open_session(&id, SessionKind::Shell, 24, 80).await, Err(Error::NotRunning)));

        manager.start(&id).await.unwrap();
        assert_eq!(manager.list()[0].status, Status::Running);

        let session = manager.open_session(&id, SessionKind::Shell, 24, 80).await.unwrap();
        let (_, mut rx) = attach(&manager, &session);
        assert_eq!(next(&mut rx).await, output("24x80"));

        manager.write(&session, b"ls\r".to_vec()).unwrap();
        assert_eq!(next(&mut rx).await, output("ls\r"));

        manager.resize(&session, 40, 120).unwrap();
        assert_eq!(next(&mut rx).await, output("40x120"));

        manager.send(&session, PtyInput::Kill).unwrap();
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
        assert_eq!(manager.list()[0].sessions[0].exit_code, Some(137));

        let terminal = manager.list()[0].shield.terminals[0].id.clone();
        manager.close_terminal(&id, &terminal).unwrap();
        assert!(manager.list()[0].sessions.is_empty());
        assert!(manager.list()[0].shield.terminals.is_empty());
        assert!(matches!(manager.write(&session, b"x".to_vec()), Err(Error::SessionNotFound)));

        manager.stop(&id).await.unwrap();
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert_eq!(*runtime.log.lock().unwrap(), [format!("start {id}"), "kill".into(), format!("stop {id}")]);
    }

    #[tokio::test]
    async fn session_opened_during_stop_is_killed() {
        open_during(false).await;
    }

    #[tokio::test]
    async fn session_opened_during_restart_is_killed() {
        open_during(true).await;
    }

    #[tokio::test]
    async fn agent_sessions_run_the_wrappers_of_the_mounted_agents() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        // Before any session: the profile file, and the bin dir bound read-only.
        assert_eq!(*runtime.profiles.lock().unwrap(), [launch::profile_argv()]);
        assert_eq!(
            *runtime.binds.lock().unwrap(),
            [
                Bind { guest: "/opt/zwinger/bin".into(), host: manager.bin_dir(&id) },
                Bind { guest: launch::CA_DIR.into(), host: Ca::dir(&manager.data) },
            ]
        );
        assert_eq!(wrappers(&manager, &id), ["pi"]);
        let pi = std::fs::read_to_string(manager.bin_dir(&id).join("pi")).unwrap();
        // The wrapper sets the proxy env itself, so a cleared env keeps it.
        assert!(pi.contains(" HTTPS_PROXY='http://zw:") && pi.contains(" NODE_USE_ENV_PROXY=1 "), "{pi}");
        let after_env = |w: &str| {
            let env = |l: &&str| l.starts_with("export ") || l.starts_with("case \" $NODE_OPTIONS");
            w.lines().skip(2).filter(|l| !env(l)).map(str::to_owned).collect::<Vec<_>>()
        };
        let bare = launch::wrapper(AgentKind::Pi, launch::Found::default(), true).unwrap();
        assert_eq!(after_env(&pi), after_env(&bare));

        manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80).await.unwrap();
        assert_eq!(runtime.argvs.lock().unwrap().last().unwrap(), &["/opt/zwinger/bin/pi"]);
        assert!(matches!(
            manager.open_session(&id, agent(AgentKind::Claude, false), 24, 80).await,
            Err(Error::RuntimeMissing(name)) if name == "claude"
        ));
        runtimes::fake_install(&manager.data, "claude");
        runtimes::fake_install(&manager.data, "codex");
        // Installed after the start: the VM has not mounted it.
        assert!(matches!(
            manager.open_session(&id, agent(AgentKind::Codex, false), 24, 80).await,
            Err(Error::RuntimeNotMounted(_))
        ));
        assert!(matches!(
            manager.open_session(&id, agent(AgentKind::Gemini, false), 24, 80).await,
            Err(Error::NotHostAgent(_))
        ));

        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        assert_eq!(wrappers(&manager, &id), ["claude", "codex", "pi"]);
        manager.open_session(&id, agent(AgentKind::Codex, true), 24, 80).await.unwrap();
        assert_eq!(runtime.argvs.lock().unwrap().last().unwrap(), &["/opt/zwinger/bin/codex", "resume", "--last"]);
        assert_eq!(manager.list()[0].sessions[0].kind, agent(AgentKind::Codex, true));
    }

    #[test]
    fn close_unknown_terminal_is_ok() {
        let (manager, _, id) = manager();
        manager.close_terminal(&id, "gone").unwrap();
    }

    #[tokio::test]
    async fn sessions_start_in_the_applied_harness() {
        let (manager, runtime, id) = manager();
        let folder = manager.list()[0].shield.folder.clone();
        std::fs::create_dir(folder.join("agent")).unwrap();
        manager.start(&id).await.unwrap();
        manager.set_harness(&id, Some("/workspace/agent")).unwrap();
        assert!(manager.list()[0].pending_restart);
        manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80).await.unwrap();

        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80).await.unwrap();
        manager.open_session(&id, SessionKind::Shell, 24, 80).await.unwrap();
        assert_eq!(*runtime.cwds.lock().unwrap(), ["/workspace", "/workspace/agent", "/workspace"]);
    }

    #[tokio::test]
    async fn terminals_keep_their_order_and_close_drops_them() {
        let dir = temp_dir();
        let store = || Store::new(dir.join("shields.json"));
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.start(&id).await.unwrap();
        for kind in [SessionKind::Shell, agent(AgentKind::Pi, false), SessionKind::Shell] {
            manager.open_session(&id, kind, 24, 80).await.unwrap();
        }
        let mut ids = terminal_ids(&manager);
        ids.reverse();
        manager.reorder_terminals(&id, &ids).unwrap();
        assert!(!manager.list()[0].pending_restart);
        assert!(matches!(manager.reorder_terminals(&id, &ids[1..]), Err(Error::InvalidOrder)));
        manager.close_terminal(&id, &ids[1]).unwrap();
        assert_eq!(terminal_ids(&manager), [ids[0].clone(), ids[2].clone()]);
        assert_eq!(manager.list()[0].sessions.len(), 2);

        // A stop keeps the records as dormant rows.
        manager.stop(&id).await.unwrap();
        assert_eq!(terminal_ids(&manager), [ids[0].clone(), ids[2].clone()]);

        // After a process restart, the saved records come back with no session.
        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        assert_eq!(terminal_ids(&reloaded), [ids[0].clone(), ids[2].clone()]);
        assert!(reloaded.list()[0].sessions.is_empty());

        // A dormant row opens again under its id and in its place.
        reloaded.start(&id).await.unwrap();
        let session = reloaded.reopen_terminal(&id, &ids[2], 24, 80).await.unwrap();
        assert_eq!(terminal_ids(&reloaded), [ids[0].clone(), ids[2].clone()]);
        let view = &reloaded.list()[0].sessions[0];
        assert_eq!((view.id.as_str(), view.terminal_id.as_deref()), (session.as_str(), Some(ids[2].as_str())));
        assert!(matches!(reloaded.reopen_terminal(&id, &ids[2], 24, 80).await, Err(Error::TerminalOpen)));
        assert!(matches!(reloaded.reopen_terminal(&id, &ids[1], 24, 80).await, Err(Error::TerminalNotFound)));

        // An exited terminal opens again in place of its old session.
        reloaded.send(&session, PtyInput::Kill).unwrap();
        until(|| reloaded.list()[0].sessions[0].exit_code.is_some()).await;
        let again = reloaded.reopen_terminal(&id, &ids[2], 24, 80).await.unwrap();
        let sessions = &reloaded.list()[0].sessions;
        assert_eq!((sessions.len(), sessions[0].id.as_str()), (1, again.as_str()));
    }
}
