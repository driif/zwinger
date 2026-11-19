pub(crate) mod ghostty;
pub(crate) mod host;
pub(crate) mod output;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::AgentKind;
use crate::agent::launch;
use crate::cage::lifecycle::mounted;
use crate::cage::manager::{HostRun, Manager, State, Vm, blocking, in_guest_folder, lock, task_guest_folder};
use crate::cage::{Cage, Members};
use crate::editor;
use crate::error::{Error, Result};
use crate::runtime::{Pty, PtyEvent, PtyInput, SandboxRuntime, TerminalKind};
use crate::runtimes;
use crate::session::{PaneNode, Session};
use output::{Output, pump};

/// One terminal process of a session. The backend owns it, not a view: a
/// view attaches to it and detaches.
pub(crate) struct Terminal {
    pub(crate) id: String,
    pub(crate) session_id: String,
    pub(crate) kind: TerminalKind,
    pub(crate) input: UnboundedSender<PtyInput>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) output: Arc<Mutex<Output>>,
    /// Set for a shell or agent terminal.
    pub(crate) terminal_id: Option<String>,
    /// Set for a host shell: its process group, which an app exit kills.
    pub(crate) host_group: Option<host::HostGroup>,
}

impl Terminal {
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
pub struct TerminalView {
    pub id: String,
    pub kind: TerminalKind,
    pub exit_code: Option<i32>,
    pub terminal_id: Option<String>,
    /// Set only while an agent terminal is live.
    pub agent: Option<AgentState>,
    /// The last known folder of a live process. `None`: exited, or unknown.
    pub folder: Option<TerminalFolder>,
    /// The order of `folder` within its process: a higher one is newer.
    pub folder_seq: u64,
}

/// A folder of a process and its order: a higher `seq` is newer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FolderReport {
    pub seq: u64,
    pub folder: Option<TerminalFolder>,
}

/// Where a live terminal process is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalFolder {
    /// The current folder: a Mac path, or a guest path if `vm`.
    pub path: PathBuf,
    pub vm: bool,
    /// The Mac folder that Open in editor opens for it. `None`: it is not on the Mac.
    pub opens: Option<PathBuf>,
}

impl TerminalFolder {
    fn host(path: PathBuf) -> Self {
        Self { opens: path.is_dir().then(|| path.clone()), path, vm: false }
    }

    /// Reads the file system for the mapping: call it with no lock held.
    fn guest(path: String, applied: &(Vec<Session>, Cage)) -> Self {
        let opens = editor::map_guest(&path, Members { sessions: &applied.0, cage: &applied.1 });
        Self { path: path.into(), vm: true, opens }
    }
}

/// The guest folder that a VM process of `kind` starts in: the applied harness
/// for an agent, else the session's guest folder.
fn guest_start(members: &[Session], session_id: &str, kind: TerminalKind) -> Option<String> {
    let (member, folder, _) = task_guest_folder(members, session_id)?;
    Some(match (kind, member.harness.as_deref()) {
        (TerminalKind::Agent { .. }, Some(harness)) => in_guest_folder(&folder, harness),
        _ => folder,
    })
}

/// Where the folder of a live process comes from.
enum Source {
    Host(host::HostGroup),
    /// The output, its start folder and the applied config.
    Guest(Arc<Mutex<Output>>, String, Box<(Vec<Session>, Cage)>),
    /// A VM process whose VM does not run.
    Stopped,
    /// A VM process with no guest folder.
    Unknown,
}

/// The session of the live process `id`, the output's `folder_seq`, and the
/// source of its folder. It reads no file system.
fn folder_source<V>(state: &Mutex<State<V>>, id: &str) -> Result<(String, u64, Source)> {
    let state = lock(state);
    let process = state.process(id).ok().filter(|t| t.exit_code.is_none()).ok_or(Error::ProcessNotFound)?;
    let seq = lock(&process.output).folder_seq;
    let source = match &process.host_group {
        Some(group) => Source::Host(group.clone()),
        None => match state.vm_of(&process.session_id) {
            Some((_, Vm::Running { applied, .. })) => {
                match guest_start(&applied.0, &process.session_id, process.kind) {
                    Some(start) => Source::Guest(process.output.clone(), start, applied.clone()),
                    None => Source::Unknown,
                }
            }
            _ => Source::Stopped,
        },
    };
    Ok((process.session_id.clone(), seq, source))
}

/// The current guest folder of a VM process: its last OSC 7, else its start folder.
fn guest_cwd(output: &Mutex<Output>, start: String) -> (u64, String) {
    let output = lock(output);
    (output.folder_seq, output.osc.cwd.clone().unwrap_or(start))
}

/// Where the live process `id` is now. A Mac process's folder comes from
/// macOS; a VM process's from its last OSC 7, else its start folder, mapped by
/// [`editor::map_guest`]. `None`: the folder is unknown, such as when macOS
/// gives none. It also gives the output's `folder_seq` that it read with its
/// source. The macOS call and the file system work run with no lock held.
pub(crate) fn resolve_folder<V>(state: &Mutex<State<V>>, id: &str) -> Result<(u64, Option<TerminalFolder>)> {
    let (_, seq, source) = folder_source(state, id)?;
    Ok(match source {
        Source::Host(group) => (seq, group.cwd().map(TerminalFolder::host)),
        Source::Guest(output, start, applied) => {
            let (seq, path) = guest_cwd(&output, start);
            (seq, Some(TerminalFolder::guest(path, &applied)))
        }
        Source::Stopped | Source::Unknown => (seq, None),
    })
}

/// The guest path of the relative path `raw` in the guest folder `cwd`. Each
/// leading `.` and `..` of `raw` applies to `cwd` as text (`..` stops at `/`),
/// so `../bravo/a.rs` reaches a sibling folder that is shared on its own. The
/// rest of `raw` stays as it is: an interior `..` after a link follows the link
/// on the Mac. The caller resolves the links in `cwd` first, because the
/// process opens `../a.rs` from the real folder, not from the OSC 7 text.
fn guest_join(cwd: &str, raw: &str) -> String {
    let mut base: Vec<&str> = cwd.split('/').filter(|s| !s.is_empty()).collect();
    let mut rest = raw;
    loop {
        let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));
        match head {
            "." => {}
            ".." => {
                base.pop();
            }
            _ => break,
        }
        rest = tail.trim_start_matches('/');
    }
    if !rest.is_empty() {
        base.push(rest);
    }
    format!("/{}", base.join("/"))
}

/// The session of the live process `id` and the Mac file that the path `raw`
/// in its output names. A relative path resolves against the current folder of
/// the process, never against the project folder. A VM path maps to the Mac
/// only below a member folder or a mount. The result is a regular file.
pub(crate) fn locate<V>(state: &Mutex<State<V>>, id: &str, raw: &str) -> Result<(String, PathBuf)> {
    let (session_id, _, source) = folder_source(state, id)?;
    let absolute = raw.starts_with('/');
    let unknown = || Error::LinkFolderUnknown(raw.to_owned());
    let path = match source {
        Source::Host(group) => {
            let path = if absolute {
                PathBuf::from(raw)
            } else {
                group.cwd().filter(|cwd| cwd.is_dir()).ok_or_else(unknown)?.join(raw)
            };
            path.canonicalize().map_err(|_| Error::LinkMissing(raw.to_owned()))?
        }
        Source::Guest(output, start, applied) => {
            let members = Members { sessions: &applied.0, cage: &applied.1 };
            let guest = if absolute {
                raw.to_owned()
            } else {
                let cwd = guest_cwd(&output, start).1;
                let cwd =
                    editor::physical_guest_dir(&cwd, members).ok_or_else(|| Error::LinkNotShared(raw.to_owned()))?;
                guest_join(&cwd, raw)
            };
            editor::map_guest_path(&guest, members).ok_or_else(|| Error::LinkNotShared(raw.to_owned()))?
        }
        Source::Stopped => return Err(Error::LinkVmStopped(raw.to_owned())),
        Source::Unknown => return Err(unknown()),
    };
    if !path.is_file() {
        return Err(Error::LinkNotFile(raw.to_owned()));
    }
    Ok((session_id, path))
}

/// What an agent in a terminal does, as its hooks report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentState {
    /// It works on a turn.
    Active,
    /// It waits for the user: a permission prompt or a question.
    Blocked,
    /// Its turn ended, and it waits for a new prompt.
    Done,
}

impl<V> State<V> {
    /// The terminal record has a process that runs.
    pub(crate) fn has_live_terminal(&self, terminal_id: &str) -> bool {
        self.terminals.iter().any(|t| t.terminal_id.as_deref() == Some(terminal_id) && t.exit_code.is_none())
    }

    pub(crate) fn process(&self, id: &str) -> Result<&Terminal> {
        self.terminals.iter().find(|t| t.id == id).ok_or(Error::ProcessNotFound)
    }
}

impl<R: SandboxRuntime> Manager<R> {
    pub fn reorder_terminals(&self, id: &str, terminal_ids: &[String]) -> Result<()> {
        self.edit_session(id, |s, _| s.reorder_terminals(terminal_ids))
    }

    /// Adds a terminal of `kind` to the end of the session's list and opens
    /// it. Returns the process id. A failed open adds no terminal.
    pub async fn open_session(&self, session_id: &str, kind: TerminalKind, rows: u16, cols: u16) -> Result<String> {
        self.open_terminal(session_id, None, kind, rows, cols).await
    }

    /// Opens a dormant terminal again: a new process under the same terminal
    /// id, in the same place. Returns the process id.
    pub async fn reopen_terminal(&self, session_id: &str, terminal_id: &str, rows: u16, cols: u16) -> Result<String> {
        let kind = {
            let state = lock(&self.state);
            let kind = state.session(session_id)?.terminal(terminal_id)?.kind;
            if state.has_live_terminal(terminal_id) {
                return Err(Error::TerminalOpen);
            }
            kind
        };
        self.open_terminal(session_id, Some(terminal_id), kind, rows, cols).await
    }

    /// `terminal_id` `None` adds a new terminal.
    pub(crate) async fn open_terminal(
        &self,
        session_id: &str,
        terminal_id: Option<&str>,
        kind: TerminalKind,
        rows: u16,
        cols: u16,
    ) -> Result<String> {
        let host_argv = match kind {
            TerminalKind::Host => Some(host::shell_argv()),
            // Checked at each open, so a stale UI or a cage change after the
            // confirm cannot start the agent on the Mac.
            TerminalKind::HostAgent { agent, resume } => {
                if lock(&self.state).effective_cage(session_id)?.is_some() {
                    return Err(Error::HasCage);
                }
                let detection = self.host.clone();
                let found = blocking(move || Ok(detection.last().agent(agent).cloned())).await?;
                let argv = found
                    .and_then(|found| launch::host_argv(&found, resume))
                    .ok_or_else(|| Error::HostAgentMissing(agent.label().to_owned()))?;
                Some(host::login_exec(argv))
            }
            TerminalKind::Shell | TerminalKind::Agent { .. } => None,
        };
        if let Some(argv) = host_argv {
            let folder = lock(&self.state).session(session_id)?.folder.clone();
            if !folder.is_dir() {
                return Err(Error::MissingPath(folder.display().to_string()));
            }
            let start = TerminalFolder::host(folder.clone());
            // The last cage check and the spawn hold the state lock, which
            // every cage change also takes: no cage can come in between.
            return self.register(session_id, terminal_id, kind, start, |state| {
                if matches!(kind, TerminalKind::HostAgent { .. }) && state.effective_cage(session_id)?.is_some() {
                    return Err(Error::HasCage);
                }
                let (pty, group) = host::open_host_pty(&folder, argv, rows, cols)?;
                Ok((pty, Some(group)))
            });
        }
        let argv = kind.vm_argv().ok_or(Error::NotRunning)?;
        let (key, vm, cwd, applied) = {
            let state = lock(&self.state);
            state.session(session_id)?;
            match state.vm_of(session_id) {
                Some((key, Vm::Running { vm, applied, runtimes, host, .. })) => {
                    let cwd = guest_start(&applied.0, session_id, kind).ok_or(Error::NotRunning)?;
                    if let TerminalKind::Agent { agent, .. } = kind
                        && !mounted(runtimes).contains(&agent)
                    {
                        return Err(self.unmounted(agent, host));
                    }
                    (key.to_owned(), vm.clone(), cwd, applied.clone())
                }
                _ => return Err(Error::NotRunning),
            }
        };
        let start = TerminalFolder::guest(cwd.clone(), &applied);
        let pty = self.runtime.open_pty(&vm, &argv, &cwd, rows, cols).await?;
        // The VM may have stopped, or stopped and started again, while
        // the process opened. Its stop did not see this process, so kill it.
        self.register(session_id, terminal_id, kind, start, |state| match state.vms.get(&key) {
            Some(Vm::Running { vm: current, .. }) if Arc::ptr_eq(current, &vm) => Ok((pty, None)),
            _ => {
                let _ = pty.input.send(PtyInput::Kill);
                Err(Error::NotRunning)
            }
        })
    }

    /// Adds the process that `open` gives under the state lock. `open`
    /// kills its process if it fails. If a close or a second open of the
    /// terminal ran while the process opened, it kills the process. `start`
    /// is the folder of the process until its first report.
    fn register(
        &self,
        session_id: &str,
        terminal_id: Option<&str>,
        kind: TerminalKind,
        start: TerminalFolder,
        open: impl FnOnce(&State<R::Vm>) -> Result<(Pty, Option<host::HostGroup>)>,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let output = Arc::new(Mutex::new(Output { folder: Some(start), ..Output::default() }));
        let events = {
            let mut state = lock(&self.state);
            let (pty, host_group) = open(&state)?;
            let terminal = match terminal_id {
                None => self.add_terminal(&mut state, session_id, kind),
                Some(tid) => match state.session(session_id).and_then(|s| s.terminal(tid).map(|_| ())) {
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
            state.terminals.retain(|t| t.terminal_id.as_ref() != Some(&terminal));
            state.terminals.push(Terminal {
                id: id.clone(),
                session_id: session_id.to_owned(),
                kind,
                input: pty.input,
                exit_code: None,
                output: output.clone(),
                terminal_id: Some(terminal),
                host_group,
            });
            pty.events
        };
        tokio::spawn(pump(
            self.state.clone(),
            id.clone(),
            output,
            events,
            kind,
            self.agent_sink.clone(),
            self.folder_sink.clone(),
        ));
        Ok(id)
    }

    /// Adds a terminal record and saves it. Returns its id.
    pub(crate) fn add_terminal(
        &self,
        state: &mut State<R::Vm>,
        session_id: &str,
        kind: TerminalKind,
    ) -> Result<String> {
        let mut sessions = state.sessions.clone();
        let session = sessions.iter_mut().find(|s| s.id == session_id).ok_or(Error::SessionNotFound)?;
        let terminal_id = session.add_terminal(kind);
        self.store.save_sessions(&sessions)?;
        state.sessions = sessions;
        Ok(terminal_id)
    }

    /// Sends the kept output of the process to `on_event`, and its exit if
    /// it has exited; then new output. A later attach takes over. Returns the
    /// token for [`Manager::detach_session`] and [`Manager::ack_session`].
    pub fn attach_session(&self, id: &str, on_event: impl Fn(PtyEvent) + Send + 'static) -> Result<u64> {
        let output = self.output(id)?;
        Ok(lock(&output).attach(Box::new(on_event)))
    }

    /// Stops the output to the view of `token`. A later attach keeps its sink,
    /// and an ended process is not an error.
    pub fn detach_session(&self, id: &str, token: u64) {
        if let Ok(output) = self.output(id) {
            lock(&output).detach(token);
        }
    }

    /// The view of `token` has written one more output frame.
    pub fn ack_session(&self, id: &str, token: u64) {
        if let Ok(output) = self.output(id) {
            lock(&output).ack(token);
        }
    }

    pub(crate) fn output(&self, id: &str) -> Result<Arc<Mutex<Output>>> {
        Ok(lock(&self.state).process(id)?.output.clone())
    }

    pub fn write(&self, id: &str, data: Vec<u8>) -> Result<()> {
        self.send(id, PtyInput::Data(data))
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<()> {
        self.send(id, PtyInput::Resize { rows, cols })
    }

    pub(crate) fn send(&self, id: &str, input: PtyInput) -> Result<()> {
        lock(&self.state).process(id)?.input.send(input).map_err(|_| Error::SessionClosed)
    }

    /// Removes the terminal and kills its process. An unknown terminal is
    /// not an error: it is already gone.
    pub fn close_terminal(&self, session_id: &str, terminal_id: &str) -> Result<()> {
        self.edit_session(session_id, |s, _| {
            s.remove_terminal(terminal_id);
            Ok(())
        })?;
        lock(&self.state).terminals.retain(|t| {
            let closed = t.terminal_id.as_deref() == Some(terminal_id);
            if closed {
                t.kill();
            }
            !closed
        });
        Ok(())
    }

    /// Hangs up all host shells and agents, and kills the ones that have not exited
    /// after [`host::HANGUP_LIMIT`]. It blocks for at most that limit and
    /// needs no async runtime, so an app exit can call it.
    pub fn kill_host_sessions(&self) {
        let mut groups = Vec::new();
        lock(&self.state).terminals.retain(|t| {
            let killed = t.kind.on_host();
            if killed && let Some(group) = &t.host_group {
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

    /// The host folder that Open in editor opens: the session folder, or the
    /// current folder of the terminal's live process, resolved again now by
    /// [`resolve_folder`]. The flag is false when the terminal's folder is
    /// unknown or not on the Mac, and the session folder is used instead.
    pub fn editor_folder(&self, session_id: &str, terminal_id: Option<&str>) -> Result<(PathBuf, bool)> {
        let (folder, process) = {
            let state = lock(&self.state);
            let folder = state.session(session_id)?.folder.clone();
            let process = terminal_id.and_then(|tid| {
                state.terminals.iter().find(|t| {
                    t.session_id == session_id && t.terminal_id.as_deref() == Some(tid) && t.exit_code.is_none()
                })
            });
            (folder, process.map(|t| t.id.clone()))
        };
        if !folder.is_dir() {
            return Err(Error::MissingPath(folder.display().to_string()));
        }
        let opens = process.and_then(|id| resolve_folder(&self.state, &id).ok()?.1?.opens);
        Ok(match opens {
            Some(path) => (path, true),
            None => (folder, terminal_id.is_none()),
        })
    }

    /// The session of the live process `process_id` and the Mac file that the
    /// path `raw` in its output names. See [`locate`].
    pub fn location(&self, process_id: &str, raw: &str) -> Result<(String, PathBuf)> {
        locate(&self.state, process_id, raw)
    }

    /// The current folder of each live process of `ids`, resolved now. An
    /// unknown or exited process is left out. It opens and changes no
    /// process; it updates only the kept folder that the listing shows, so a
    /// change with no output, or a new mount mapping, shows there too. A
    /// folder read before a newer report is left out: the next call reads it.
    pub fn terminal_folders(&self, ids: &[String]) -> HashMap<String, FolderReport> {
        ids.iter()
            .filter_map(|id| {
                let output = self.output(id).ok()?;
                let (seq, folder) = resolve_folder(&self.state, id).ok()?;
                let (seq, _) = output::keep_folder(&output, &folder, seq)?;
                Some((id.clone(), FolderReport { seq, folder }))
            })
            .collect()
    }

    pub fn set_pane_layout(&self, session_id: &str, layout: Option<PaneNode>) -> Result<()> {
        self.edit_session(session_id, |s, _| s.set_pane_layout(layout))
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
    use super::{FolderReport, TerminalFolder};
    use crate::cage::testing::*;
    use crate::session::{PaneNode, SplitDirection};

    type Seen = Arc<Mutex<Vec<(String, u64, Option<TerminalFolder>)>>>;

    fn follow_folders(manager: &Manager<FakeRuntime>) -> Seen {
        let seen = Seen::default();
        let into = seen.clone();
        manager.set_folder_sink(Box::new(move |process, seq, folder| {
            lock(&into).push((process.to_owned(), seq, folder.cloned()));
        }));
        seen
    }

    /// The folders of the batch call, without their order.
    fn batch(manager: &Manager<FakeRuntime>, ids: &[String]) -> Vec<(String, Option<TerminalFolder>)> {
        manager.terminal_folders(ids).into_iter().map(|(id, report)| (id, report.folder)).collect()
    }

    fn listed_folder(manager: &Manager<FakeRuntime>, process: &str) -> Option<TerminalFolder> {
        let listing = manager.list();
        listing.sessions[0].processes.iter().find(|p| p.id == process).and_then(|p| p.folder.clone())
    }

    fn mac(path: &Path) -> Option<TerminalFolder> {
        Some(TerminalFolder { path: path.to_owned(), vm: false, opens: Some(path.to_owned()) })
    }

    fn vm(guest: &str, opens: Option<&Path>) -> Option<TerminalFolder> {
        Some(TerminalFolder { path: guest.into(), vm: true, opens: opens.map(Path::to_owned) })
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let (manager, runtime, id) = manager();
        assert!(matches!(manager.open_session(&id, TerminalKind::Shell, 24, 80).await, Err(Error::NotRunning)));

        manager.start(&id).await.unwrap();
        assert_eq!(manager.list().cages[0].shared().status, Status::Running);

        let session = manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        let (_, mut rx) = attach(&manager, &session);
        assert_eq!(next(&mut rx).await, output("24x80"));

        manager.write(&session, b"ls\r".to_vec()).unwrap();
        assert_eq!(next(&mut rx).await, output("ls\r"));

        manager.resize(&session, 40, 120).unwrap();
        assert_eq!(next(&mut rx).await, output("40x120"));

        manager.send(&session, PtyInput::Kill).unwrap();
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
        assert_eq!(manager.list().sessions[0].processes[0].exit_code, Some(137));

        let terminal = manager.list().sessions[0].session.terminals[0].id.clone();
        manager.close_terminal(&id, &terminal).unwrap();
        assert!(manager.list().sessions[0].processes.is_empty());
        assert!(manager.list().sessions[0].session.terminals.is_empty());
        assert!(matches!(manager.write(&session, b"x".to_vec()), Err(Error::ProcessNotFound)));

        manager.stop(&id, None).await.unwrap();
        assert_eq!(manager.list().cages[0].shared().status, Status::Stopped);
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
        assert_eq!(wrappers(&manager, &id), ["pi", "pi-status.js"]);
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

        manager.stop(&id, None).await.unwrap();
        manager.start(&id).await.unwrap();
        assert_eq!(
            wrappers(&manager, &id),
            ["claude", "claude-status.json", "codex", "pi", "pi-status.js", "zwinger-agent-status"]
        );
        manager.open_session(&id, agent(AgentKind::Codex, true), 24, 80).await.unwrap();
        assert_eq!(runtime.argvs.lock().unwrap().last().unwrap(), &["/opt/zwinger/bin/codex", "resume", "--last"]);
        assert_eq!(manager.list().sessions[0].processes[0].kind, agent(AgentKind::Codex, true));
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
            manager.open_session(&id, TerminalKind::Host, 24, 80).await.unwrap();
        }
        let t = terminal_ids(&manager);
        let layout = split(
            SplitDirection::Row,
            0.4,
            leaf(&t[0]),
            Box::new(split(SplitDirection::Column, 0.5, leaf(&t[1]), leaf(&t[2]))),
        );
        manager.set_pane_layout(&id, Some(layout.clone())).unwrap();
        assert_eq!(manager.list().sessions[0].session.pane_layout, Some(layout));
        assert!(!manager.list().cages[0].shared().pending_restart);

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
        assert_eq!(manager.list().sessions[0].session.pane_layout, Some(collapsed));
        manager.close_terminal(&id, &t[0]).unwrap();
        assert_eq!(manager.list().sessions[0].session.pane_layout, Some(*leaf(&t[2])));
        manager.close_terminal(&id, &t[2]).unwrap();
        assert_eq!(manager.list().sessions[0].session.pane_layout, None);
        manager.set_pane_layout(&id, None).unwrap();
    }

    #[tokio::test]
    async fn host_shells_need_no_start_and_outlive_a_stop_but_not_a_delete() {
        host_sh();
        let (manager, runtime, id) = manager();
        let session = manager.open_session(&id, TerminalKind::Host, 24, 80).await.unwrap();
        assert_eq!(manager.list().cages[0].shared().status, Status::Stopped);
        assert_eq!(manager.list().sessions[0].session.terminals[0].kind, TerminalKind::Host);
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
        manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        manager.stop(&id, None).await.unwrap();
        let sessions = &manager.list().sessions[0].processes;
        assert_eq!((sessions.len(), sessions[0].id.as_str()), (1, session.as_str()));
        assert_eq!(*runtime.argvs.lock().unwrap(), [["/bin/bash", "-l"]], "the host shell is not in the VM");

        manager.delete(&id).await.unwrap();
        assert!(lock(&manager.state).sessions.is_empty());
        while !matches!(next(&mut rx).await, PtyEvent::Exit(_)) {}
    }

    #[tokio::test]
    async fn a_mac_agent_opens_only_with_no_cage_and_outlives_a_vm_stop() {
        use std::os::unix::fs::PermissionsExt;
        host_sh();
        let (manager, _, id) = manager_with_host();
        let claude = TerminalKind::HostAgent { agent: AgentKind::Claude, resume: true };
        // The attached cage, then the workspace default: the agent does not open.
        assert!(matches!(manager.open_session(&id, claude, 24, 80).await, Err(Error::HasCage)));
        manager.start(&id).await.unwrap();
        manager.set_session_cage(&id, None).unwrap();
        let workspace = lock(&manager.state).workspaces[0].id.clone();
        manager.set_workspace_cage(&workspace, Some(&id)).unwrap();
        assert!(matches!(manager.open_session(&id, claude, 24, 80).await, Err(Error::HasCage)));
        assert!(manager.list().sessions[0].session.terminals.is_empty());

        // No effective cage, while the old VM still runs: the agent opens on the Mac.
        manager.set_workspace_cage(&workspace, None).unwrap();
        let bin = manager.host.last().agent(AgentKind::Claude).unwrap().bin.clone().unwrap();
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\necho \"mac-claude $*\"\nexec sleep 1000\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let process = manager.open_session(&id, claude, 24, 80).await.unwrap();
        let (_, mut rx) = attach(&manager, &process);
        let mut out = String::new();
        while !out.contains("mac-claude -c") {
            match next(&mut rx).await {
                PtyEvent::Output(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
                PtyEvent::Exit(code) => panic!("exit {code}: {out}"),
            }
        }
        let listed = manager.list();
        assert_eq!(listed.sessions[0].session.terminals[0].kind, claude);
        assert_eq!(listed.sessions[0].processes[0].agent, None, "no hooks, so no status");

        // A VM stop does not end it; an app exit does.
        manager.stop(&id, None).await.unwrap();
        assert_eq!(manager.list().sessions[0].processes.len(), 1);
        manager.kill_host_sessions();
        assert!(manager.list().sessions[0].processes.is_empty());
        while !matches!(next(&mut rx).await, PtyEvent::Exit(_)) {}

        // A reopen checks the cage again.
        manager.set_session_cage(&id, Some(&id)).unwrap();
        let terminal = terminal_ids(&manager)[0].clone();
        assert!(matches!(manager.reopen_terminal(&id, &terminal, 24, 80).await, Err(Error::HasCage)));
    }

    #[tokio::test]
    async fn a_mac_agent_that_is_not_installed_does_not_open() {
        let (manager, _, id) = manager();
        manager.set_session_cage(&id, None).unwrap();
        let pi = TerminalKind::HostAgent { agent: AgentKind::Pi, resume: false };
        assert!(matches!(
            manager.open_session(&id, pi, 24, 80).await,
            Err(Error::HostAgentMissing(name)) if name == "pi"
        ));
        assert!(manager.list().sessions[0].session.terminals.is_empty());
    }

    #[tokio::test]
    async fn a_host_shell_in_a_missing_folder_does_not_open() {
        let (manager, _, id) = manager();
        let folder = manager.list().sessions[0].session.folder.clone();
        std::fs::remove_dir_all(&folder).unwrap();
        assert!(matches!(
            manager.open_session(&id, TerminalKind::Host, 24, 80).await,
            Err(Error::MissingPath(path)) if path == folder.display().to_string()
        ));
        assert!(manager.list().sessions[0].session.terminals.is_empty());
    }

    #[tokio::test]
    async fn the_editor_folder_of_a_host_shell_is_its_current_folder() {
        host_sh();
        let (manager, _, id) = manager();
        let folder = manager.list().sessions[0].session.folder.clone();
        std::fs::create_dir(folder.join("sub")).unwrap();
        assert_eq!(manager.editor_folder(&id, None).unwrap(), (folder.clone(), true));
        assert_eq!(manager.editor_folder(&id, Some("gone")).unwrap(), (folder.clone(), false));

        let session = manager.open_session(&id, TerminalKind::Host, 24, 80).await.unwrap();
        let terminal = terminal_ids(&manager)[0].clone();
        until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.clone(), true)).await;
        manager.write(&session, b"cd sub\n".to_vec()).unwrap();
        until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.join("sub"), true)).await;
        assert!(matches!(manager.editor_folder("other", None), Err(Error::SessionNotFound)));
        manager.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn the_editor_folder_of_a_vm_terminal_follows_osc7_below_the_workspace_only() {
        let (manager, _, id) = manager();
        let folder = manager.list().sessions[0].session.folder.clone();
        std::fs::create_dir(folder.join("sub")).unwrap();
        manager.start(&id).await.unwrap();
        let session = manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        let terminal = terminal_ids(&manager)[0].clone();
        assert_eq!(manager.editor_folder(&id, Some(&terminal)).unwrap(), (folder.clone(), true));

        // The fake VM echoes its input as output.
        manager.write(&session, b"\x1b]7;file://vm/workspace/sub\x07".to_vec()).unwrap();
        until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.join("sub"), true)).await;
        manager.write(&session, b"\x1b]7;file://vm/etc\x07".to_vec()).unwrap();
        until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.clone(), false)).await;

        // The real output of the bash and the zsh emitters maps to the Mac folder.
        let odd = folder.join("a b/\u{e4}%");
        std::fs::create_dir_all(&odd).unwrap();
        for osc7 in crate::agent::launch::emitted_osc7("/workspace/a b/\u{e4}%") {
            assert_eq!(osc7, "\x1b]7;file://vm/workspace/a%20b/%C3%A4%25\x07".as_bytes());
            manager.write(&session, osc7).unwrap();
            until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (odd.clone(), true)).await;
            manager.write(&session, b"\x1b]7;file://vm/etc\x07".to_vec()).unwrap();
            until(|| manager.editor_folder(&id, Some(&terminal)).unwrap() == (folder.clone(), false)).await;
        }

        // An exited session has no folder.
        manager.send(&session, PtyInput::Kill).unwrap();
        until(|| manager.list().sessions[0].processes[0].exit_code.is_some()).await;
        assert_eq!(manager.editor_folder(&id, Some(&terminal)).unwrap(), (folder.clone(), false));

        std::fs::remove_dir_all(&folder).unwrap();
        assert!(matches!(manager.editor_folder(&id, None), Err(Error::MissingPath(_))));
    }

    #[tokio::test]
    async fn a_host_shell_folder_follows_its_cd_also_with_no_output() {
        host_sh();
        let (manager, _, id) = manager();
        let seen = follow_folders(&manager);
        let folder = manager.list().sessions[0].session.folder.clone();
        for sub in ["sub", "quiet"] {
            std::fs::create_dir(folder.join(sub)).unwrap();
        }
        let process = manager.open_session(&id, TerminalKind::Host, 24, 80).await.unwrap();
        // The start folder until the first report.
        assert_eq!(listed_folder(&manager, &process), mac(&folder));

        // The output pump never asks macOS: only the batch call sees a cd.
        let ids = [process.clone(), "gone".to_owned()];
        let wait_for = |sub: &'static str| {
            let want = mac(&folder.join(sub));
            let manager = &manager;
            let ids = &ids;
            async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while manager.terminal_folders(ids).get(&ids[0]).map(|r| &r.folder) != Some(&want) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("the batch call did not see the cd");
            }
        };
        manager.write(&process, b"cd sub\n".to_vec()).unwrap();
        wait_for("sub").await;
        assert_eq!(listed_folder(&manager, &process), mac(&folder.join("sub")));

        // With no echo and no prompt, the cd writes nothing.
        manager.write(&process, b"stty -echo; PS1=; PS2=\n".to_vec()).unwrap();
        let output = manager.output(&process).unwrap();
        let mut len = usize::MAX;
        while lock(&output).ring.len() != len {
            len = lock(&output).ring.len();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        manager.write(&process, b"cd ../quiet\n".to_vec()).unwrap();
        wait_for("quiet").await;
        assert_eq!(lock(&output).ring.len(), len, "the cd wrote output");
        assert_eq!(manager.terminal_folders(&ids).len(), 1, "an unknown id is left out");
        // The batch call keeps the folder for the listing, and each change gets a newer seq.
        assert_eq!(listed_folder(&manager, &process), mac(&folder.join("quiet")));
        let seq = manager.list().sessions[0].processes[0].folder_seq;
        assert_eq!(manager.terminal_folders(&ids)[&process].seq, seq);
        assert!(lock(&seen).is_empty(), "the pump sent a Mac folder");
        manager.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn a_host_shell_whose_folder_macos_does_not_give_has_no_folder() {
        host_sh();
        let (manager, _, id) = manager();
        let folder = manager.list().sessions[0].session.folder.clone();
        let process = manager.open_session(&id, TerminalKind::Host, 24, 80).await.unwrap();
        let terminal = terminal_ids(&manager)[0].clone();
        assert_eq!(listed_folder(&manager, &process), mac(&folder));

        // A group that macOS gives no cwd for, while the process stays live.
        let swap = |group| {
            let mut state = lock(&manager.state);
            let t = state.terminals.iter_mut().find(|t| t.id == process).unwrap();
            std::mem::replace(&mut t.host_group, group)
        };
        let real = swap(Some(super::host::HostGroup::retired()));
        let ids = [process.clone()];
        assert_eq!(batch(&manager, &ids), [(process.clone(), None)]);
        assert_eq!(listed_folder(&manager, &process), None, "the start folder is not kept");
        assert_eq!(manager.editor_folder(&id, Some(&terminal)).unwrap(), (folder.clone(), false));

        swap(real);
        assert_eq!(manager.terminal_folders(&ids)[&process].folder, mac(&folder));
        manager.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn a_vm_shell_folder_follows_osc7_and_maps_to_the_mac_folder() {
        let (manager, _, id) = manager();
        let seen = follow_folders(&manager);
        let folder = manager.list().sessions[0].session.folder.clone();
        std::fs::create_dir(folder.join("sub")).unwrap();
        let shared = temp_dir();
        std::fs::create_dir(shared.join("x")).unwrap();
        manager.add_mount(&id, shared.to_str().unwrap(), "/data", false).unwrap();
        manager.start(&id).await.unwrap();
        let process = manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        let terminal = terminal_ids(&manager)[0].clone();
        // The start folder before the first OSC 7, also after the first output.
        let (_, mut rx) = attach(&manager, &process);
        assert_eq!(next(&mut rx).await, output("24x80"));
        assert_eq!(listed_folder(&manager, &process), vm("/workspace", Some(&folder)));
        assert!(lock(&seen).is_empty());

        // The fake VM echoes its input as output.
        let cases = [("/workspace/sub", Some(folder.join("sub"))), ("/data/x", Some(shared.join("x"))), ("/etc", None)];
        for (guest, opens) in &cases {
            manager.write(&process, format!("\x1b]7;file://vm{guest}\x07").into_bytes()).unwrap();
            let want = vm(guest, opens.as_deref());
            until(|| listed_folder(&manager, &process) == want).await;
            let seq = manager.list().sessions[0].processes[0].folder_seq;
            assert_eq!(lock(&seen).last(), Some(&(process.clone(), seq, want.clone())));
            assert_eq!(
                manager.terminal_folders(std::slice::from_ref(&process))[&process],
                FolderReport { seq, folder: want }
            );
            let editor = opens.clone().map_or((folder.clone(), false), |path| (path, true));
            assert_eq!(manager.editor_folder(&id, Some(&terminal)).unwrap(), editor);
        }
        assert_eq!(lock(&seen).len(), cases.len(), "only a change goes to the sink");

        // An exited process has no folder, and its replacement starts again.
        manager.send(&process, PtyInput::Kill).unwrap();
        until(|| manager.list().sessions[0].processes[0].exit_code.is_some()).await;
        assert_eq!(listed_folder(&manager, &process), None);
        assert!(manager.terminal_folders(std::slice::from_ref(&process)).is_empty());
        let again = manager.reopen_terminal(&id, &terminal, 24, 80).await.unwrap();
        let both = [process.clone(), again.clone()];
        assert_eq!(batch(&manager, &both), [(again, vm("/workspace", Some(&folder)))]);
    }

    #[tokio::test]
    async fn a_host_link_resolves_against_the_current_folder_only() {
        host_sh();
        let (manager, _, id) = manager();
        let folder = manager.list().sessions[0].session.folder.clone().canonicalize().unwrap();
        std::fs::create_dir_all(folder.join("sub/dir")).unwrap();
        std::fs::write(folder.join("top.rs"), "").unwrap();
        std::fs::write(folder.join("sub/inner.rs"), "").unwrap();
        let process = manager.open_session(&id, TerminalKind::Host, 24, 80).await.unwrap();
        let at = |raw: &str| manager.location(&process, raw);
        until(|| at("top.rs").ok() == Some((id.clone(), folder.join("top.rs")))).await;
        assert!(matches!(at("inner.rs"), Err(Error::LinkMissing(_))));
        assert!(matches!(at("sub/dir"), Err(Error::LinkNotFile(_))));

        // After a cd, a relative path resolves in the new folder; an absolute path anywhere.
        manager.write(&process, b"cd sub\n".to_vec()).unwrap();
        until(|| at("inner.rs").is_ok()).await;
        assert!(matches!(at("top.rs"), Err(Error::LinkMissing(_))));
        assert_eq!(at("../top.rs").unwrap().1, folder.join("top.rs"));
        let absolute = folder.join("top.rs");
        assert_eq!(at(absolute.to_str().unwrap()).unwrap().1, absolute);

        // A folder that is gone, or that macOS does not give, is never the project folder.
        std::fs::remove_dir_all(folder.join("sub")).unwrap();
        assert!(matches!(at("top.rs"), Err(Error::LinkFolderUnknown(_))));
        let real = {
            let mut state = lock(&manager.state);
            let t = state.terminals.iter_mut().find(|t| t.id == process).unwrap();
            t.host_group.replace(super::host::HostGroup::retired())
        };
        assert!(matches!(at("top.rs"), Err(Error::LinkFolderUnknown(_))));
        assert_eq!(at(absolute.to_str().unwrap()).unwrap().1, absolute);
        lock(&manager.state).terminals.iter_mut().find(|t| t.id == process).unwrap().host_group = real;
        assert!(matches!(manager.location("gone", "top.rs"), Err(Error::ProcessNotFound)));
        manager.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn a_vm_link_maps_through_the_shared_folders_only() {
        let (manager, _, id) = manager();
        let folder = manager.list().sessions[0].session.folder.clone().canonicalize().unwrap();
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::write(folder.join("top.rs"), "").unwrap();
        std::fs::write(folder.join("sub/inner.rs"), "").unwrap();
        let shared = temp_dir().canonicalize().unwrap();
        std::fs::write(shared.join("data.rs"), "").unwrap();
        manager.add_mount(&id, shared.to_str().unwrap(), "/data", false).unwrap();
        manager.start(&id).await.unwrap();
        let process = manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        let at = |raw: &str| manager.location(&process, raw);
        assert_eq!(at("top.rs").unwrap(), (id.clone(), folder.join("top.rs")));
        assert_eq!(at("/workspace/sub/inner.rs").unwrap().1, folder.join("sub/inner.rs"));
        assert_eq!(at("/data/data.rs").unwrap().1, shared.join("data.rs"));
        assert!(matches!(at("sub"), Err(Error::LinkNotFile(_))));
        for raw in ["gone.rs", "../etc/passwd", "/etc/hosts", "/workspace/../top.rs"] {
            assert!(matches!(at(raw), Err(Error::LinkNotShared(_))), "{raw}");
        }

        // The fake VM echoes its input as output: an OSC 7 changes the folder.
        manager.write(&process, b"\x1b]7;file://vm/workspace/sub\x07".to_vec()).unwrap();
        until(|| at("inner.rs").is_ok()).await;
        assert!(matches!(at("top.rs"), Err(Error::LinkNotShared(_))));
        assert_eq!(at("../top.rs").unwrap().1, folder.join("top.rs"));
        // The process can remain in its physical directory after its reported alias disappears.
        std::fs::create_dir_all(folder.join("deep/sub")).unwrap();
        std::fs::write(folder.join("deep/top.rs"), "").unwrap();
        let alias = folder.join("alias");
        std::os::unix::fs::symlink(folder.join("deep/sub"), &alias).unwrap();
        manager.write(&process, b"\x1b]7;file://vm/workspace/alias\x07".to_vec()).unwrap();
        until(|| at("../top.rs").is_ok_and(|(_, file)| file == folder.join("deep/top.rs"))).await;
        std::fs::remove_file(&alias).unwrap();
        assert!(at("../top.rs").is_err());
        std::fs::write(&alias, "").unwrap();
        assert!(at("../top.rs").is_err());
        assert_eq!(at("/workspace/top.rs").unwrap().1, folder.join("top.rs"));

        manager.write(&process, b"\x1b]7;file://vm/etc\x07".to_vec()).unwrap();
        until(|| matches!(at("inner.rs"), Err(Error::LinkNotShared(_)))).await;
        assert_eq!(at("/workspace/top.rs").unwrap().1, folder.join("top.rs"));

        manager.send(&process, PtyInput::Kill).unwrap();
        until(|| manager.list().sessions[0].processes[0].exit_code.is_some()).await;
        assert!(matches!(at("/workspace/top.rs"), Err(Error::ProcessNotFound)));
    }

    #[test]
    fn a_leading_dot_dot_reaches_a_sibling_member_and_an_interior_one_follows_links() {
        use super::{Cage, Members, Session, editor, guest_join};
        assert_eq!(guest_join("/workspace/alpha", "../bravo/a.rs"), "/workspace/bravo/a.rs");
        assert_eq!(guest_join("/workspace/alpha/", "./.././a.rs"), "/workspace/a.rs");
        assert_eq!(guest_join("/workspace", "../../../a.rs"), "/a.rs");
        assert_eq!(guest_join("/", ".."), "/");
        assert_eq!(guest_join("/workspace/alpha", "near/link/../a.rs"), "/workspace/alpha/near/link/../a.rs");

        let root = temp_dir().canonicalize().unwrap();
        let (alpha, bravo) = (root.join("alpha"), root.join("bravo"));
        std::fs::create_dir_all(alpha.join("deep/sub")).unwrap();
        std::fs::create_dir_all(alpha.join("near")).unwrap();
        std::fs::create_dir_all(bravo.join("src")).unwrap();
        std::fs::write(bravo.join("src/main.rs"), "").unwrap();
        std::fs::write(alpha.join("deep/correct.rs"), "").unwrap();
        std::os::unix::fs::symlink(alpha.join("deep/sub"), alpha.join("near/link")).unwrap();
        let first = Session::new(alpha.to_str().unwrap(), Some("c".into()), []).unwrap();
        let second = Session::new(bravo.to_str().unwrap(), Some("c".into()), [first.slug.as_str()]).unwrap();
        let cwd = format!("/workspace/{}", first.slug);
        let sibling = format!("../{}/src/main.rs", second.slug);
        let cage = Cage::new("c".into(), "c".into());
        let sessions = [first, second];
        let members = || Members { sessions: &sessions, cage: &cage };
        let map_from = |cwd: &str, raw: &str| {
            let cwd = editor::physical_guest_dir(cwd, members())?;
            editor::map_guest_path(&guest_join(&cwd, raw), members())
        };
        let map = |raw: &str| map_from(&cwd, raw);
        assert_eq!(map(&sibling), Some(bravo.join("src/main.rs")));
        assert_eq!(map("../unmapped/x.rs"), None);
        // `near/link/..` is `deep` on the Mac, not `near`.
        assert_eq!(map("near/link/../correct.rs"), Some(alpha.join("deep/correct.rs")));
        // In the linked folder `near/link`, `..` is `deep`, not `near`.
        let linked = format!("{cwd}/near/link");
        assert_eq!(editor::physical_guest_dir(&linked, members()), Some(format!("{cwd}/deep/sub")));
        assert_eq!(map_from(&linked, "../correct.rs"), Some(alpha.join("deep/correct.rs")));
        assert_eq!(map_from(&linked, "../../../x.rs"), None);
        assert_eq!(editor::physical_guest_dir(&cwd, members()), Some(cwd.clone()));
    }

    #[test]
    fn close_unknown_terminal_is_ok() {
        let (manager, _, id) = manager();
        manager.close_terminal(&id, "gone").unwrap();
    }

    #[tokio::test]
    async fn sessions_start_in_the_applied_harness() {
        let (manager, runtime, id) = manager();
        let folder = manager.list().sessions[0].session.folder.clone();
        std::fs::create_dir(folder.join("agent")).unwrap();
        manager.start(&id).await.unwrap();
        manager.set_harness(&id, Some("/workspace/agent")).unwrap();
        assert!(manager.list().cages[0].shared().pending_restart);
        manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80).await.unwrap();

        manager.stop(&id, None).await.unwrap();
        manager.start(&id).await.unwrap();
        manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80).await.unwrap();
        manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        assert_eq!(*runtime.cwds.lock().unwrap(), ["/workspace", "/workspace/agent", "/workspace"]);
    }

    #[tokio::test]
    async fn terminals_keep_their_order_and_close_drops_them() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        manager.start(&id).await.unwrap();
        for kind in [TerminalKind::Shell, agent(AgentKind::Pi, false), TerminalKind::Shell] {
            manager.open_session(&id, kind, 24, 80).await.unwrap();
        }
        let mut ids = terminal_ids(&manager);
        ids.reverse();
        manager.reorder_terminals(&id, &ids).unwrap();
        assert!(!manager.list().cages[0].shared().pending_restart);
        assert!(matches!(manager.reorder_terminals(&id, &ids[1..]), Err(Error::InvalidOrder)));
        manager.close_terminal(&id, &ids[1]).unwrap();
        assert_eq!(terminal_ids(&manager), [ids[0].clone(), ids[2].clone()]);
        assert_eq!(manager.list().sessions[0].processes.len(), 2);

        // A stop keeps the records as dormant rows.
        manager.stop(&id, None).await.unwrap();
        assert_eq!(terminal_ids(&manager), [ids[0].clone(), ids[2].clone()]);

        // After a process restart, the saved records come back with no session.
        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        assert_eq!(terminal_ids(&reloaded), [ids[0].clone(), ids[2].clone()]);
        assert!(reloaded.list().sessions[0].processes.is_empty());

        // A dormant row opens again under its id and in its place.
        reloaded.start(&id).await.unwrap();
        let session = reloaded.reopen_terminal(&id, &ids[2], 24, 80).await.unwrap();
        assert_eq!(terminal_ids(&reloaded), [ids[0].clone(), ids[2].clone()]);
        let view = &reloaded.list().sessions[0].processes[0];
        assert_eq!((view.id.as_str(), view.terminal_id.as_deref()), (session.as_str(), Some(ids[2].as_str())));
        assert!(matches!(reloaded.reopen_terminal(&id, &ids[2], 24, 80).await, Err(Error::TerminalOpen)));
        assert!(matches!(reloaded.reopen_terminal(&id, &ids[1], 24, 80).await, Err(Error::TerminalNotFound)));

        // An exited terminal opens again in place of its old session.
        reloaded.send(&session, PtyInput::Kill).unwrap();
        until(|| reloaded.list().sessions[0].processes[0].exit_code.is_some()).await;
        let again = reloaded.reopen_terminal(&id, &ids[2], 24, 80).await.unwrap();
        let sessions = &reloaded.list().sessions[0].processes;
        assert_eq!((sessions.len(), sessions[0].id.as_str()), (1, again.as_str()));
    }
}
