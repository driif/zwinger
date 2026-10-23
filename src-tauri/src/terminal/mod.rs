pub(crate) mod ghostty;
pub(crate) mod host;
pub(crate) mod output;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::AgentKind;
use crate::cage::apps::AppRun;
use crate::cage::lifecycle::mounted;
use crate::cage::manager::{HostRun, Manager, State, Vm, lock};
use crate::cage::{PaneNode, Project, WORKSPACE};
use crate::editor;
use crate::error::{Error, Result};
use crate::runtime::{Pty, PtyEvent, PtyInput, SandboxRuntime, SessionKind};
use crate::runtimes;
use output::{Output, pump};

/// The backend owns each session, not a view: a view attaches to it and
/// detaches.
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) project_id: String,
    pub(crate) kind: SessionKind,
    pub(crate) input: UnboundedSender<PtyInput>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) output: Arc<Mutex<Output>>,
    /// Set for a shell or agent terminal.
    pub(crate) terminal_id: Option<String>,
    /// Set for an app run.
    pub(crate) app: Option<AppRun>,
    /// Set for a host shell: its process group, which an app exit kills.
    pub(crate) host_group: Option<host::HostGroup>,
}

impl Session {
    /// A host shell gets a hangup first, so its background jobs end too.
    pub(crate) fn kill(&self) {
        match &self.host_group {
            Some(group) => group.terminate(),
            None => {
                let _ = self.input.send(PtyInput::Kill);
            }
        }
    }
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

    /// Adds a terminal of `kind` to the end of the project's list and opens
    /// it. Returns the session id. A failed open adds no terminal.
    pub async fn open_session(&self, project_id: &str, kind: SessionKind, rows: u16, cols: u16) -> Result<String> {
        self.open_terminal(project_id, None, kind, rows, cols).await
    }

    /// Opens a dormant terminal again: a new process under the same terminal
    /// id, in the same place. Returns the session id.
    pub async fn reopen_terminal(&self, project_id: &str, terminal_id: &str, rows: u16, cols: u16) -> Result<String> {
        let kind = {
            let mut state = lock(&self.state);
            let kind = state.project_mut(project_id)?.terminal(terminal_id)?.kind;
            if state.has_live_terminal(terminal_id) {
                return Err(Error::TerminalOpen);
            }
            kind
        };
        self.open_terminal(project_id, Some(terminal_id), kind, rows, cols).await
    }

    /// `terminal_id` `None` adds a new terminal.
    pub(crate) async fn open_terminal(
        &self,
        project_id: &str,
        terminal_id: Option<&str>,
        kind: SessionKind,
        rows: u16,
        cols: u16,
    ) -> Result<String> {
        if kind == SessionKind::Host {
            let folder = lock(&self.state).project_mut(project_id)?.folder.clone();
            if !folder.is_dir() {
                return Err(Error::MissingPath(folder.display().to_string()));
            }
            let (pty, group) = host::open_host_pty(&folder, rows, cols)?;
            return self.register(project_id, terminal_id, kind, pty, Some(group), |_| true);
        }
        let (vm, cwd) = match lock(&self.state).vms.get(project_id) {
            Some(Vm::Running { vm, applied, runtimes, host, .. }) => {
                let cwd = match kind {
                    SessionKind::Agent { agent, .. } if !mounted(runtimes).contains(&agent) => {
                        return Err(self.unmounted(agent, host));
                    }
                    SessionKind::Agent { .. } => applied.harness.as_deref(),
                    SessionKind::Shell | SessionKind::Host => None,
                };
                (vm.clone(), cwd.unwrap_or(WORKSPACE).to_owned())
            }
            _ => return Err(Error::NotRunning),
        };
        let pty = self.runtime.open_pty(&vm, &kind.argv(), &cwd, rows, cols).await?;
        // The cage may have stopped, or stopped and started again, while
        // the process opened. Its stop did not see this session, so kill it.
        self.register(project_id, terminal_id, kind, pty, None, |state| {
            matches!(
                state.vms.get(project_id),
                Some(Vm::Running { vm: current, .. }) if Arc::ptr_eq(current, &vm)
            )
        })
    }

    /// Adds the session of the opened process. If `same_vm` is false, or a
    /// close or a second open of the terminal ran while the process opened,
    /// it kills the process.
    fn register(
        &self,
        project_id: &str,
        terminal_id: Option<&str>,
        kind: SessionKind,
        pty: Pty,
        host_group: Option<host::HostGroup>,
        same_vm: impl FnOnce(&State<R::Vm>) -> bool,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let output = Arc::new(Mutex::new(Output::default()));
        {
            let mut state = lock(&self.state);
            let terminal = match terminal_id {
                _ if !same_vm(&state) => Err(Error::NotRunning),
                None => self.add_terminal(&mut state, project_id, kind),
                Some(tid) => match state.project_mut(project_id).and_then(|s| s.terminal(tid).map(|_| ())) {
                    Err(e) => Err(e),
                    Ok(()) if state.has_live_terminal(tid) => Err(Error::TerminalOpen),
                    Ok(()) => Ok(tid.to_owned()),
                },
            };
            let terminal = match terminal {
                Ok(terminal) => terminal,
                Err(e) => {
                    match host_group {
                        Some(group) => group.kill(),
                        None => {
                            let _ = pty.input.send(PtyInput::Kill);
                        }
                    }
                    return Err(e);
                }
            };
            state.sessions.retain(|s| s.terminal_id.as_ref() != Some(&terminal));
            state.sessions.push(Session {
                id: id.clone(),
                project_id: project_id.to_owned(),
                kind,
                input: pty.input,
                exit_code: None,
                output: output.clone(),
                terminal_id: Some(terminal),
                app: None,
                host_group,
            });
        }
        tokio::spawn(pump(self.state.clone(), id.clone(), output, pty.events));
        Ok(id)
    }

    /// Adds a terminal record and saves it. Returns its id.
    pub(crate) fn add_terminal(&self, state: &mut State<R::Vm>, project_id: &str, kind: SessionKind) -> Result<String> {
        let mut projects = state.projects.clone();
        let project = projects.iter_mut().find(|s| s.id == project_id).ok_or(Error::ProjectNotFound)?;
        let terminal_id = project.add_terminal(kind);
        self.store.save(&projects)?;
        state.projects = projects;
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
    pub fn close_terminal(&self, project_id: &str, terminal_id: &str) -> Result<()> {
        self.edit(project_id, |s| {
            s.remove_terminal(terminal_id);
            Ok(())
        })?;
        lock(&self.state).sessions.retain(|s| {
            let closed = s.terminal_id.as_deref() == Some(terminal_id);
            if closed {
                s.kill();
            }
            !closed
        });
        Ok(())
    }

    /// Hangs up all host shells, and kills the ones that have not exited
    /// after [`host::HANGUP_LIMIT`]. It blocks for at most that limit and
    /// needs no async runtime, so an app exit can call it.
    pub fn kill_host_sessions(&self) {
        let mut groups = Vec::new();
        lock(&self.state).sessions.retain(|s| {
            let killed = s.kind == SessionKind::Host;
            if killed && let Some(group) = &s.host_group {
                group.hangup();
                groups.push(group.clone());
            }
            !killed
        });
        if groups.is_empty() {
            return;
        }
        host::wait_retired(&groups, std::time::Instant::now() + host::HANGUP_LIMIT);
        for group in &groups {
            group.kill();
        }
    }

    /// The host folder that Open in editor opens: the project folder, or the
    /// current folder of the terminal's live session. A host shell's folder
    /// comes from macOS; a VM terminal's from its last OSC 7, else its start
    /// folder, mapped by [`editor::map_guest`]. The flag is false when the
    /// terminal's folder is unknown and the project folder is used instead.
    pub fn editor_folder(&self, project_id: &str, terminal_id: Option<&str>) -> Result<(PathBuf, bool)> {
        enum Cwd {
            Host(host::HostGroup),
            /// The output, its start folder and the applied config.
            Guest(Arc<Mutex<Output>>, Option<String>, Box<Project>),
        }
        let (folder, cwd) = {
            let mut state = lock(&self.state);
            let folder = state.project_mut(project_id)?.folder.clone();
            let session = terminal_id.and_then(|tid| {
                state.sessions.iter().find(|s| {
                    s.project_id == project_id && s.terminal_id.as_deref() == Some(tid) && s.exit_code.is_none()
                })
            });
            let cwd = match (session, state.vms.get(project_id)) {
                (Some(Session { host_group: Some(group), .. }), _) => Some(Cwd::Host(group.clone())),
                (Some(session), Some(Vm::Running { applied, .. })) => {
                    let start = match session.kind {
                        SessionKind::Agent { .. } => applied.harness.clone(),
                        SessionKind::Shell | SessionKind::Host => None,
                    };
                    Some(Cwd::Guest(session.output.clone(), start, applied.clone()))
                }
                _ => None,
            };
            (folder, cwd)
        };
        if !folder.is_dir() {
            return Err(Error::MissingPath(folder.display().to_string()));
        }
        let found = match cwd {
            Some(Cwd::Host(group)) => group.cwd().filter(|path| path.is_dir()),
            Some(Cwd::Guest(output, start, applied)) => {
                let guest = lock(&output).osc7.cwd.clone().or(start);
                editor::map_guest(guest.as_deref().unwrap_or(WORKSPACE), &applied)
            }
            None => None,
        };
        Ok(match found {
            Some(path) => (path, true),
            None => (folder, terminal_id.is_none()),
        })
    }

    pub fn set_pane_layout(&self, project_id: &str, layout: Option<PaneNode>) -> Result<()> {
        self.edit(project_id, |s| s.set_pane_layout(layout))
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
    use crate::cage::testing::*;
    use crate::cage::{PaneNode, SplitDirection};

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

        let terminal = manager.list()[0].project.terminals[0].id.clone();
        manager.close_terminal(&id, &terminal).unwrap();
        assert!(manager.list()[0].sessions.is_empty());
        assert!(manager.list()[0].project.terminals.is_empty());
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

    fn leaf(terminal: &str) -> Box<PaneNode> {
        Box::new(PaneNode::Leaf { terminal: terminal.into() })
    }

    fn split(direction: SplitDirection, ratio: f64, a: Box<PaneNode>, b: Box<PaneNode>) -> PaneNode {
        PaneNode::Split { direction, ratio, a, b }
    }

    #[tokio::test]
    async fn pane_layouts_name_each_terminal_once_and_close_collapses_the_split() {
        host_sh();
        let (manager, _, id) = manager();
        for _ in 0..3 {
            manager.open_session(&id, SessionKind::Host, 24, 80).await.unwrap();
        }
        let t = terminal_ids(&manager);
        let layout = split(
            SplitDirection::Row,
            0.4,
            leaf(&t[0]),
            Box::new(split(SplitDirection::Column, 0.5, leaf(&t[1]), leaf(&t[2]))),
        );
        manager.set_pane_layout(&id, Some(layout.clone())).unwrap();
        assert_eq!(manager.list()[0].project.pane_layout, Some(layout));
        assert!(!manager.list()[0].pending_restart);

        let invalid = |layout| matches!(manager.set_pane_layout(&id, Some(layout)), Err(Error::InvalidPaneLayout(_)));
        assert!(invalid(split(SplitDirection::Row, 0.5, leaf(&t[0]), leaf(&t[0]))));
        assert!(invalid(split(SplitDirection::Row, 0.5, leaf(&t[0]), leaf("gone"))));
        assert!(invalid(split(SplitDirection::Row, 0.95, leaf(&t[0]), leaf(&t[1]))));
        assert!(invalid(split(SplitDirection::Row, f64::NAN, leaf(&t[0]), leaf(&t[1]))));
        let mut deep = PaneNode::Leaf { terminal: t[0].clone() };
        for _ in 0..9 {
            deep = split(SplitDirection::Row, 0.5, Box::new(deep), leaf(&t[1]));
        }
        assert!(invalid(deep));

        // A close removes the pane; its sibling takes the place of the split.
        manager.close_terminal(&id, &t[1]).unwrap();
        let collapsed = split(SplitDirection::Row, 0.4, leaf(&t[0]), leaf(&t[2]));
        assert_eq!(manager.list()[0].project.pane_layout, Some(collapsed));
        manager.close_terminal(&id, &t[0]).unwrap();
        assert_eq!(manager.list()[0].project.pane_layout, Some(*leaf(&t[2])));
        manager.close_terminal(&id, &t[2]).unwrap();
        assert_eq!(manager.list()[0].project.pane_layout, None);
        manager.set_pane_layout(&id, None).unwrap();
    }

    #[tokio::test]
    async fn host_shells_need_no_start_and_outlive_a_stop_but_not_a_delete() {
        host_sh();
        let (manager, runtime, id) = manager();
        let session = manager.open_session(&id, SessionKind::Host, 24, 80).await.unwrap();
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert_eq!(manager.list()[0].project.terminals[0].kind, SessionKind::Host);
        let (_, mut rx) = attach(&manager, &session);
        manager.write(&session, b"echo hi-$((40+2))\n".to_vec()).unwrap();
        let mut out = String::new();
        while !out.contains("hi-42") {
            match next(&mut rx).await {
                PtyEvent::Output(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
                PtyEvent::Exit(code) => panic!("exit {code}: {out}"),
            }
        }

        manager.start(&id).await.unwrap();
        manager.open_session(&id, SessionKind::Shell, 24, 80).await.unwrap();
        manager.stop(&id).await.unwrap();
        let sessions = &manager.list()[0].sessions;
        assert_eq!((sessions.len(), sessions[0].id.as_str()), (1, session.as_str()));
        assert_eq!(*runtime.argvs.lock().unwrap(), [["/bin/bash", "-l"]], "the host shell is not in the VM");

        manager.delete(&id).await.unwrap();
        assert!(lock(&manager.state).sessions.is_empty());
        while !matches!(next(&mut rx).await, PtyEvent::Exit(_)) {}
    }

    #[tokio::test]
    async fn a_host_shell_in_a_missing_folder_does_not_open() {
        let (manager, _, id) = manager();
        let folder = manager.list()[0].project.folder.clone();
        std::fs::remove_dir_all(&folder).unwrap();
        assert!(matches!(
            manager.open_session(&id, SessionKind::Host, 24, 80).await,
            Err(Error::MissingPath(path)) if path == folder.display().to_string()
        ));
        assert!(manager.list()[0].project.terminals.is_empty());
    }

    #[tokio::test]
    async fn the_editor_folder_of_a_host_shell_is_its_current_folder() {
        host_sh();
        let (manager, _, id) = manager();
        let folder = manager.list()[0].project.folder.clone();
        std::fs::create_dir(folder.join("sub")).unwrap();
        assert_eq!(manager.editor_folder(&id, None).unwrap(), (folder.clone(), true));
        assert_eq!(manager.editor_folder(&id, Some("gone")).unwrap(), (folder.clone(), false));

        let session = manager.open_session(&id, SessionKind::Host, 24, 80).await.unwrap();
        let terminal = terminal_ids(&manager)[0].clone();
        until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.clone(), true)).await;
        manager.write(&session, b"cd sub\n".to_vec()).unwrap();
        until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.join("sub"), true)).await;
        assert!(matches!(manager.editor_folder("other", None), Err(Error::ProjectNotFound)));
        manager.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn the_editor_folder_of_a_vm_terminal_follows_osc7_below_the_workspace_only() {
        let (manager, _, id) = manager();
        let folder = manager.list()[0].project.folder.clone();
        std::fs::create_dir(folder.join("sub")).unwrap();
        manager.start(&id).await.unwrap();
        let session = manager.open_session(&id, SessionKind::Shell, 24, 80).await.unwrap();
        let terminal = terminal_ids(&manager)[0].clone();
        assert_eq!(manager.editor_folder(&id, Some(&terminal)).unwrap(), (folder.clone(), true));

        // The fake VM echoes its input as output.
        manager.write(&session, b"\x1b]7;file://vm/workspace/sub\x07".to_vec()).unwrap();
        until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.join("sub"), true)).await;
        manager.write(&session, b"\x1b]7;file://vm/etc\x07".to_vec()).unwrap();
        until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.clone(), false)).await;

        // An exited session has no folder.
        manager.send(&session, PtyInput::Kill).unwrap();
        until(|| manager.list()[0].sessions[0].exit_code.is_some()).await;
        assert_eq!(manager.editor_folder(&id, Some(&terminal)).unwrap(), (folder.clone(), false));

        std::fs::remove_dir_all(&folder).unwrap();
        assert!(matches!(manager.editor_folder(&id, None), Err(Error::MissingPath(_))));
    }

    #[test]
    fn close_unknown_terminal_is_ok() {
        let (manager, _, id) = manager();
        manager.close_terminal(&id, "gone").unwrap();
    }

    #[tokio::test]
    async fn sessions_start_in_the_applied_harness() {
        let (manager, runtime, id) = manager();
        let folder = manager.list()[0].project.folder.clone();
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
        let store = || Store::new(dir.join("projects.json"));
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
