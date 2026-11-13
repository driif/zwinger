use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use serde::Serialize;
use tokio::task::AbortHandle;

use crate::agent::{AgentKind, launch};
use crate::cage::lifecycle::mounted;
use crate::cage::network::placeholders;
use crate::cage::progress::{Phase, ProgressSink, Timings};
use crate::cage::{self, Bound, Cage, HostAgentUse, WORKSPACE};
use crate::error::{Error, Result};
use crate::host_agents::HostAgents;
use crate::host_agents::login;
use crate::network::{self, Action, RuleDraft, Target};
use crate::proxy::ca::Ca;
use crate::proxy::log::Entry;
use crate::proxy::{Live, Proxy};
use crate::runtime::{SandboxRuntime, TerminalKind};
use crate::secrets::{self, Vault, VaultEntry};
use crate::session::{Pos, Session};
use crate::store::{self, Store};
use crate::task_runs;
use crate::terminal::{AgentState, Terminal, TerminalView};
use crate::workspaces::{self, Arrangement, Workspace, WorkspaceStore};

/// The network log entries kept per cage.
pub(crate) const NET_RING: usize = 2000;

/// The dir of the per-VM agent state. It keeps its name from before the
/// rename to projects, so the agent state of existing cages stays.
pub(crate) const LEGACY_STATE_DIR: &str = "shields";

/// Gets each new network log entry with its cage id.
pub type NetSink = Box<dyn Fn(&str, &Entry) + Send + Sync>;

/// Gets the process id and the new agent state of a terminal.
pub type AgentSink = Box<dyn Fn(&str, Option<AgentState>) + Send + Sync>;

/// Gets the id of a cage whose guest logins changed.
pub type GuestSink = Box<dyn Fn(&str) + Send + Sync>;

/// Owns the sessions and cages, the running VMs and their terminals.
pub struct Manager<R: SandboxRuntime> {
    pub(crate) runtime: R,
    pub(crate) store: Store,
    pub(crate) workspace_store: WorkspaceStore,
    /// The app data dir. It holds the agent runtimes, `shields/<id>`, the
    /// agent state of each VM, and `bin/<id>`, the host-only agent
    /// wrappers of each VM.
    pub(crate) data: PathBuf,
    /// The canonical dirs that no session folder or mount may nest with:
    /// the app data dir and `~/.zwinger`.
    pub(crate) reserved: Vec<PathBuf>,
    pub(crate) vault: Arc<dyn Vault>,
    /// The zwinger CA of this app run.
    pub(crate) ca: Arc<Ca>,
    /// The boot reap failed, so the VMs of the last run can still run. The
    /// next cage start reaps them first, under `reap_gate`.
    pub(crate) reap_pending: AtomicBool,
    pub(crate) reap_gate: tokio::sync::Mutex<()>,
    /// Serializes the secret edits, so that a rollback cannot undo a later edit.
    /// The state lock is not held during vault calls.
    pub(crate) secret_edit: Mutex<()>,
    /// Per account, the value that the saved config expects (`None`: no
    /// value) after a restore that did not end. Until the vault is seen to
    /// hold it, starts and edits that use the account fail.
    pub(crate) unsettled: Mutex<HashMap<String, Option<VaultEntry>>>,
    pub(crate) state: Arc<Mutex<State<R::Vm>>>,
    pub(crate) net_sink: Arc<OnceLock<NetSink>>,
    pub(crate) agent_sink: Arc<OnceLock<AgentSink>>,
    pub(crate) runs: Arc<task_runs::Registry>,
    /// Serializes run starts, so two starts cannot take the same run name.
    pub(crate) run_start: Mutex<()>,
    /// The stage limits of new task runs.
    pub(crate) run_config: task_runs::Config,
    pub(crate) host: Arc<HostAgents>,
    pub(crate) guest_sink: Arc<OnceLock<GuestSink>>,
    pub(crate) progress_sink: Arc<OnceLock<ProgressSink>>,
    pub(crate) app_notices: Mutex<Vec<String>>,
}

pub(crate) struct State<V> {
    pub(crate) sessions: Vec<Session>,
    pub(crate) cages: Vec<Cage>,
    /// Each session is in exactly one workspace, and there is at least one.
    pub(crate) workspaces: Vec<Workspace>,
    /// Per cage id: one VM per cage. It runs for one session, which keeps
    /// it after its cage changes, until it stops.
    pub(crate) vms: HashMap<String, Vm<V>>,
    pub(crate) terminals: Vec<Terminal>,
    /// Per cage that starts or runs, the policy and the credential values
    /// of its proxy. An edit changes them at once.
    pub(crate) live: HashMap<String, Arc<Live>>,
    /// Per cage, the last network log entries. They stay after a stop, so
    /// the user can still allow a blocked host.
    pub(crate) net: HashMap<String, VecDeque<Entry>>,
    /// The generation of the last VM that started. A stop names the
    /// generation it resolved, so a delayed stop cannot hit a later VM.
    pub(crate) generation: u64,
}

pub(crate) enum Vm<V> {
    /// The phase that the start for `session` runs now.
    Starting {
        session: String,
        phase: Phase,
    },
    /// `applied` is the session and the cage the VM booted with; they differ
    /// from the stored config after an edit, until the next start. `proxy`
    /// is the cage's proxy; a drop stops it. `net` follows the VM's network
    /// events. It is `None` if the runtime log could not be opened.
    /// `runtimes` are the agent runtimes that the start mounted.
    /// `generation` is unique to this start.
    Running {
        generation: u64,
        vm: Arc<V>,
        applied: Box<(Session, Cage)>,
        proxy: Arc<Proxy>,
        net: Option<AbortHandle>,
        runtimes: Vec<&'static str>,
        host: Box<HostRun>,
    },
    /// Shutdown is in progress. If it fails, the VM goes back to `Running`.
    /// It still belongs to `session` until the shutdown succeeds.
    Stopping {
        session: String,
    },
    /// The start for `session` failed with `message`.
    Failed {
        session: String,
        message: String,
    },
}

/// What a start did for the cage's host agents.
#[derive(Default)]
pub(crate) struct HostRun {
    /// pi runs from the host install; else the bundled pi runs.
    pub(crate) pi_host: bool,
    /// The guest path of Gemini CLI's entry point, if it runs in the VM.
    pub(crate) gemini: Option<String>,
    /// The URL of the cage's proxy.
    pub(crate) proxy: Option<String>,
    /// What the user should know, such as a fallback.
    pub(crate) notices: Vec<String>,
    /// The host agents that do not run in the VM, with the reason.
    pub(crate) failed: Vec<(AgentKind, String)>,
    /// The agents with a login that was made in the guest.
    pub(crate) guest_logins: Vec<AgentKind>,
    /// Looks for guest logins while the VM runs. A stop aborts it.
    pub(crate) guest_watch: Option<AbortHandle>,
    /// The phase durations of the start.
    pub(crate) timings: Timings,
}

impl HostRun {
    pub(crate) fn found(&self) -> launch::Found<'_> {
        launch::Found { pi_host: self.pi_host, gemini: self.gemini.as_deref(), proxy: self.proxy.as_deref() }
    }
}

/// All sessions and cages, read under one lock.
#[derive(Debug, Serialize)]
pub struct Listing {
    pub sessions: Vec<SessionView>,
    pub cages: Vec<CageView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    #[serde(flatten)]
    pub session: Session,
    /// The live terminal processes of the session.
    pub processes: Vec<TerminalView>,
    /// The hosts that the session's agents need, with the proxy's decision.
    pub agent_needs: Vec<network::AgentNeed>,
    pub effective_cage: Option<EffectiveCage>,
    /// The cage whose VM starts or runs for the session, or whose start for
    /// the session failed. It differs from the effective cage after a cage
    /// change, until that VM stops.
    pub vm_cage: Option<String>,
}

/// The cage that a session uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveCage {
    pub id: String,
    /// The cage is the default cage of the session's workspace.
    pub inherited: bool,
}

/// The attached cage of `session`, else the default cage of its workspace.
pub(crate) fn effective(session: &Session, workspaces: &[Workspace]) -> Option<EffectiveCage> {
    if let Some(id) = &session.cage {
        return Some(EffectiveCage { id: id.clone(), inherited: false });
    }
    let workspace = workspaces.iter().find(|w| w.sessions.contains(&session.id))?;
    workspace.default_cage.clone().map(|id| EffectiveCage { id, inherited: true })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CageView {
    #[serde(flatten)]
    pub cage: Cage,
    pub status: Status,
    pub pending_restart: bool,
    /// The agent runtimes that the running VM has mounted. A host Gemini CLI
    /// that loads counts as one.
    pub runtimes: Vec<&'static str>,
    /// The earliest expiry of the host logins that the proxy of the running
    /// VM has now, in ms. The proxy reads a newer login when it is due.
    pub login_expires_at: Option<i64>,
    pub claude_token: Option<ClaudeTokenView>,
    /// What the start of the running VM did differently for the host
    /// agents, such as a fallback.
    pub host_notices: Vec<String>,
    /// The agents of the running VM with a login that was made in the
    /// guest: the agent can read its real token.
    pub guest_logins: Vec<AgentKind>,
    /// The phase durations of the start of the running VM.
    pub start_timings: Option<Timings>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeTokenView {
    pub stored: bool,
    pub expires_at: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    Stopped,
    Starting { phase: Phase },
    Running,
    Stopping,
    Error { message: String },
}

/// Deletes the values that no configured credential uses, such as the
/// values of a cage whose delete failed. A failure does not stop the app load.
pub(crate) fn prune_vault(vault: &dyn Vault, cages: &[Cage]) {
    let accounts = match vault.accounts() {
        Ok(accounts) => accounts,
        Err(e) => return eprintln!("secret prune: cannot list the vault: {e}"),
    };
    // A host login has no value in the vault, so its account is not kept.
    let configured: HashSet<String> = cages
        .iter()
        .flat_map(|cage| cage.network.credentials().into_iter().map(|c| secrets::account(&cage.id, &c.name)))
        .collect();
    for account in accounts.iter().filter(|a| !configured.contains(*a)) {
        let Some((cage_id, name)) = account.split_once('/') else {
            continue;
        };
        if vault.delete(cage_id, name).is_err() {
            eprintln!("secret prune: cannot delete {account}");
        }
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<V> State<V> {
    pub(crate) fn session(&self, id: &str) -> Result<&Session> {
        self.sessions.iter().find(|s| s.id == id).ok_or(Error::SessionNotFound)
    }

    /// The session as mounted in its running VM, its cage, and its guest folder.
    /// A session with no effective cage cannot run a task.
    pub(crate) fn task_cage(&self, id: &str) -> Result<(&Session, &Cage, &Vm<V>, String, bool)> {
        self.effective_cage(id)?.ok_or(Error::NoCage)?;
        let Some((_, vm @ Vm::Running { applied, .. })) = self.vm_of(id) else {
            return Err(Error::NotRunning);
        };
        let (member, folder, shared) =
            task_guest_folder(std::slice::from_ref(&applied.0), id).ok_or(Error::NotRunning)?;
        Ok((member, &applied.1, vm, folder, shared))
    }

    pub(crate) fn cage(&self, id: &str) -> Result<&Cage> {
        self.cages.iter().find(|c| c.id == id).ok_or(Error::CageNotFound)
    }

    pub(crate) fn effective_cage(&self, session_id: &str) -> Result<Option<EffectiveCage>> {
        Ok(effective(self.session(session_id)?, &self.workspaces))
    }

    /// The session and its effective cage.
    pub(crate) fn bound(&self, session_id: &str) -> Result<Bound<'_>> {
        let session = self.session(session_id)?;
        let cage = effective(session, &self.workspaces).ok_or(Error::NoCage)?;
        Ok(Bound { session, cage: self.cage(&cage.id)? })
    }

    /// The sessions whose effective cage is `cage_id`.
    pub(crate) fn sessions_of(&self, cage_id: &str) -> Vec<&Session> {
        let uses = |s: &&Session| effective(s, &self.workspaces).is_some_and(|c| c.id == cage_id);
        self.sessions.iter().filter(uses).collect()
    }

    /// The cage id and the VM that starts, runs or stops for the session.
    pub(crate) fn vm_of(&self, session_id: &str) -> Option<(&str, &Vm<V>)> {
        self.vms
            .iter()
            .find(|(_, vm)| match vm {
                Vm::Starting { session, .. } | Vm::Stopping { session } => session == session_id,
                Vm::Running { applied, .. } => applied.0.id == session_id,
                Vm::Failed { .. } => false,
            })
            .map(|(id, vm)| (id.as_str(), vm))
    }

    /// Numbers the entry and adds it to the cage's ring.
    pub(crate) fn push_net(&mut self, cage_id: &str, mut event: Entry) -> Entry {
        let ring = self.net.entry(cage_id.to_owned()).or_default();
        event.seq = ring.back().map_or(1, |last| last.seq + 1);
        if ring.len() == NET_RING {
            ring.pop_front();
        }
        ring.push_back(event.clone());
        event
    }
}

fn task_guest_folder<'a>(members: &'a [Session], id: &str) -> Option<(&'a Session, String, bool)> {
    let member = members.iter().find(|s| s.id == id)?;
    let shared = members.len() > 1;
    let folder = if shared { format!("{WORKSPACE}/{}", member.slug) } else { WORKSPACE.to_owned() };
    Some((member, folder, shared))
}

fn ids(sessions: &[Session]) -> Vec<String> {
    sessions.iter().map(|s| s.id.clone()).collect()
}

/// The app data dir and `~/.zwinger`, canonical where they exist, so that
/// they compare with the canonical host paths of the sessions and cages.
fn reserved_dirs(data: &Path, home: Option<PathBuf>) -> Vec<PathBuf> {
    let canonical = |path: PathBuf| path.canonicalize().unwrap_or(path);
    let mut dirs = vec![canonical(data.to_owned())];
    if let Some(home) = home {
        dirs.push(canonical(canonical(home).join(".zwinger")));
    }
    dirs
}

impl<R: SandboxRuntime> Manager<R> {
    pub fn new(runtime: R, store: Store, vault: Arc<dyn Vault>, data: PathBuf, host: Arc<HostAgents>) -> Result<Self> {
        let (sessions, cages) = store.load()?;
        prune_vault(vault.as_ref(), &cages);
        let ca = Ca::load(&Ca::dir(&data))?;
        eprintln!("proxy CA sha256 {}", ca.fingerprint());
        let mut workspace_store = WorkspaceStore::new(store.dir().join(workspaces::FILE));
        // A file that cannot be read or moved aside stays as it is: the
        // default workspace is used and the store refuses to save over it.
        let stored = workspace_store.load().unwrap_or_else(|e| {
            eprintln!("the workspaces could not be loaded, the default is used: {e}");
            None
        });
        let workspaces = workspaces::reconcile(stored.clone(), &ids(&sessions));
        for w in &workspaces {
            if let Some(cage) = &w.default_cage {
                store::check_cage(&cages, &format!("workspace {} ({})", w.name, w.id), cage)?;
            }
        }
        if stored.as_ref() != Some(&workspaces)
            && let Err(e) = workspace_store.save(&workspaces)
        {
            eprintln!("the workspaces could not be saved: {e}");
        }
        Ok(Self {
            runtime,
            store,
            workspace_store,
            reserved: reserved_dirs(&data, dirs::home_dir()),
            data,
            vault,
            ca: Arc::new(ca),
            reap_pending: AtomicBool::new(false),
            reap_gate: tokio::sync::Mutex::new(()),
            secret_edit: Mutex::new(()),
            unsettled: Mutex::default(),
            state: Arc::new(Mutex::new(State {
                sessions,
                cages,
                workspaces,
                vms: HashMap::new(),
                terminals: Vec::new(),
                live: HashMap::new(),
                net: HashMap::new(),
                generation: 0,
            })),
            net_sink: Arc::new(OnceLock::new()),
            agent_sink: Arc::new(OnceLock::new()),
            runs: Arc::default(),
            run_start: Mutex::new(()),
            run_config: task_runs::Config::default(),
            host,
            guest_sink: Arc::new(OnceLock::new()),
            progress_sink: Arc::new(OnceLock::new()),
            app_notices: Mutex::default(),
        })
    }

    /// Only the first sink counts.
    pub fn set_net_sink(&self, sink: NetSink) {
        let _ = self.net_sink.set(sink);
    }

    /// Only the first sink counts.
    pub fn set_agent_sink(&self, sink: AgentSink) {
        let _ = self.agent_sink.set(sink);
    }

    /// Only the first sink counts.
    pub fn set_guest_sink(&self, sink: GuestSink) {
        let _ = self.guest_sink.set(sink);
    }

    /// Only the first sink counts.
    pub fn set_progress_sink(&self, sink: ProgressSink) {
        let _ = self.progress_sink.set(sink);
    }

    pub fn list(&self) -> Listing {
        let state = lock(&self.state);
        let sessions = state
            .sessions
            .iter()
            .map(|session| SessionView {
                agent_needs: state.bound(&session.id).map_or_else(|_| Vec::new(), network::agent_needs),
                effective_cage: effective(session, &state.workspaces),
                vm_cage: state.vm_of(&session.id).map(|(id, _)| id.to_owned()).or_else(|| {
                    // A failed start shows on the session that started it.
                    state
                        .vms
                        .iter()
                        .find(|(_, vm)| matches!(vm, Vm::Failed { session: s, .. } if *s == session.id))
                        .map(|(id, _)| id.clone())
                }),
                processes: state
                    .terminals
                    .iter()
                    .filter(|t| t.session_id == session.id)
                    .map(|t| TerminalView {
                        id: t.id.clone(),
                        kind: t.kind,
                        exit_code: t.exit_code,
                        terminal_id: t.terminal_id.clone(),
                        agent: lock(&t.output)
                            .osc
                            .agent
                            .filter(|_| t.exit_code.is_none() && matches!(t.kind, TerminalKind::Agent { .. })),
                    })
                    .collect(),
                session: session.clone(),
            })
            .collect();
        let cages = state.cages.iter().map(|cage| cage_view(&state, cage, self.vault.as_ref())).collect();
        Listing { sessions, cages }
    }

    /// The host-only dir of the VM's agent wrappers, bound read-only at
    /// [`launch::BIN_DIR`]. It is not in the state dir: the guest can write that.
    pub(crate) fn bin_dir(&self, id: &str) -> PathBuf {
        bin_dir(&self.data, id)
    }

    pub(crate) fn state_dir(&self, id: &str) -> PathBuf {
        self.data.join(LEGACY_STATE_DIR).join(id)
    }

    /// The host-owned profile snapshots of the VM's host agents.
    pub(crate) fn profile_dir(&self, id: &str) -> PathBuf {
        self.data.join("profiles").join(id)
    }

    /// Adds a session of `folder` at the end of `workspace`, or of the
    /// first workspace. The session inherits the default cage of the
    /// workspace. With no default, it gets a new cage with the id and the
    /// name of the session, as the converted ones have. Returns the session id.
    pub fn create(&self, folder: &str, workspace: Option<&str>) -> Result<String> {
        let mut state = lock(&self.state);
        let taken: Vec<&str> = state.sessions.iter().map(|s| s.slug.as_str()).collect();
        let mut session = Session::new(folder, None, taken.iter().copied())?;
        let id = session.id.clone();
        let mut workspaces = state.workspaces.clone();
        let target = match workspace {
            Some(wanted) => workspaces.iter_mut().find(|w| w.id == wanted).ok_or(Error::WorkspaceNotFound)?,
            None => &mut workspaces[0],
        };
        target.sessions.push(id.clone());
        let mut cages = state.cages.clone();
        let cage = match target.default_cage.clone() {
            Some(default) => state.cage(&default)?.clone(),
            None => {
                session.cage = Some(id.clone());
                let cage = Cage::new(id.clone(), session.name.clone());
                cages.push(cage.clone());
                cage
            }
        };
        self.check_session_cage(Bound { session: &session, cage: &cage })?;
        let mut sessions = state.sessions.clone();
        sessions.push(session);
        let added = cages.len() != state.cages.len();
        if added {
            self.store.save_cages(&cages)?;
        }
        if let Err(e) = self.save_both(&state.sessions, &sessions, &workspaces) {
            if added && let Err(undo) = self.store.save_cages(&state.cages) {
                eprintln!("the cages could not be restored: {undo}");
            }
            return Err(e);
        }
        state.sessions = sessions;
        state.cages = cages;
        state.workspaces = workspaces;
        Ok(id)
    }

    /// Saves the sessions, then the workspaces. If the second save fails,
    /// the sessions file gets `old` back. The load reconciles what is left.
    fn save_both(&self, old: &[Session], sessions: &[Session], workspaces: &[Workspace]) -> Result<()> {
        self.store.save_sessions(sessions)?;
        if let Err(e) = self.workspace_store.save(workspaces) {
            if let Err(undo) = self.store.save_sessions(old) {
                eprintln!("the sessions could not be restored: {undo}");
            }
            return Err(e);
        }
        Ok(())
    }

    pub fn workspaces(&self) -> Vec<Workspace> {
        lock(&self.state).workspaces.clone()
    }

    /// Adds an empty workspace at the end and returns its id.
    pub fn create_workspace(&self) -> Result<String> {
        let workspace = Workspace::new(Vec::new());
        let id = workspace.id.clone();
        self.edit_workspaces(|workspaces| {
            workspaces.push(workspace);
            Ok(())
        })?;
        Ok(id)
    }

    /// A blank name reverts to the default name.
    pub fn rename_workspace(&self, id: &str, name: &str) -> Result<()> {
        let name = workspaces::workspace_name(name)?;
        self.edit_workspaces(|workspaces| {
            workspaces.iter_mut().find(|w| w.id == id).ok_or(Error::WorkspaceNotFound)?.name = name;
            Ok(())
        })
    }

    /// Only an empty workspace that is not the last one.
    pub fn delete_workspace(&self, id: &str) -> Result<()> {
        self.edit_workspaces(|workspaces| {
            let i = workspaces.iter().position(|w| w.id == id).ok_or(Error::WorkspaceNotFound)?;
            if !workspaces[i].sessions.is_empty() {
                return Err(Error::WorkspaceNotEmpty);
            }
            if workspaces.len() == 1 {
                return Err(Error::LastWorkspace);
            }
            workspaces.remove(i);
            Ok(())
        })
    }

    /// `wanted` must name each workspace and each session exactly once. A
    /// session that inherits its cage gets the default cage of its new
    /// workspace; a VM that runs for it keeps running until it stops.
    pub fn arrange_workspaces(&self, wanted: Vec<Arrangement>) -> Result<()> {
        let mut state = lock(&self.state);
        let workspaces = workspaces::arrange(&state.workspaces, wanted, &ids(&state.sessions))?;
        self.check_cage_changes(&state, &state.sessions, &workspaces)?;
        self.workspace_store.save(&workspaces)?;
        state.workspaces = workspaces;
        Ok(())
    }

    /// Checks each session whose effective cage changes from `state` to
    /// `sessions` and `workspaces` against its new cage.
    fn check_cage_changes(&self, state: &State<R::Vm>, sessions: &[Session], workspaces: &[Workspace]) -> Result<()> {
        for session in sessions {
            let id = |c: Option<EffectiveCage>| c.map(|c| c.id);
            let old = id(state.effective_cage(&session.id).ok().flatten());
            let Some(new) = id(effective(session, workspaces)) else {
                continue;
            };
            if old.as_ref() != Some(&new) {
                self.check_session_cage(Bound { session, cage: state.cage(&new)? })?;
            }
        }
        Ok(())
    }

    /// Checks a session against a cage that it starts to use.
    fn check_session_cage(&self, bound: Bound<'_>) -> Result<()> {
        bound.check()?;
        bound.check_app_data(&self.reserved)
    }

    /// Adds a cage with no mounts, rules or agents. Returns its id.
    pub fn create_cage(&self, name: &str) -> Result<String> {
        let cage = Cage::new(uuid::Uuid::new_v4().to_string(), cage::display_name(name)?);
        let id = cage.id.clone();
        let mut state = lock(&self.state);
        let mut cages = state.cages.clone();
        cages.push(cage);
        self.store.save_cages(&cages)?;
        state.cages = cages;
        Ok(id)
    }

    pub fn rename_cage(&self, id: &str, name: &str) -> Result<()> {
        let name = cage::display_name(name)?;
        self.edit_cage(id, |c, _| {
            c.name = name;
            Ok(())
        })
    }

    /// `None`: the session inherits the default cage of its workspace. A VM
    /// that runs for the session keeps running until it stops.
    pub fn set_session_cage(&self, session_id: &str, cage_id: Option<&str>) -> Result<()> {
        let mut state = lock(&self.state);
        if let Some(id) = cage_id {
            state.cage(id)?;
        }
        let mut sessions = state.sessions.clone();
        sessions.iter_mut().find(|s| s.id == session_id).ok_or(Error::SessionNotFound)?.cage =
            cage_id.map(str::to_owned);
        self.check_cage_changes(&state, &sessions, &state.workspaces)?;
        self.store.save_sessions(&sessions)?;
        state.sessions = sessions;
        Ok(())
    }

    /// The cage of each session of the workspace that inherits its cage.
    pub fn set_workspace_cage(&self, workspace_id: &str, cage_id: Option<&str>) -> Result<()> {
        let mut state = lock(&self.state);
        if let Some(id) = cage_id {
            state.cage(id)?;
        }
        let mut workspaces = state.workspaces.clone();
        workspaces.iter_mut().find(|w| w.id == workspace_id).ok_or(Error::WorkspaceNotFound)?.default_cage =
            cage_id.map(str::to_owned);
        self.check_cage_changes(&state, &state.sessions, &workspaces)?;
        self.workspace_store.save(&workspaces)?;
        state.workspaces = workspaces;
        Ok(())
    }

    /// Only a cage that no session uses, that is no workspace default and
    /// whose VM does not run. Also deletes its secret values, its agent
    /// state, its wrappers, its old chats and its profiles. The first error
    /// is returned after all deletes ran.
    pub fn delete_cage(&self, id: &str) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        let cage = {
            let mut state = lock(&self.state);
            let cage = state.cage(id)?.clone();
            let in_use = |reason: String| Err(Error::CageInUse(cage.name.clone(), reason));
            if let Some(session) = state.sessions_of(id).first() {
                return in_use(format!("session {} uses it", session.name));
            }
            if let Some(w) = state.workspaces.iter().find(|w| w.default_cage.as_deref() == Some(id)) {
                return in_use(format!("it is the default cage of workspace {}", w.name));
            }
            if state.vms.get(id).is_some_and(|vm| !matches!(vm, Vm::Failed { .. })) {
                return in_use("its VM runs".into());
            }
            let cages: Vec<Cage> = state.cages.iter().filter(|c| c.id != id).cloned().collect();
            self.store.save_cages(&cages)?;
            state.cages = cages;
            state.vms.remove(id);
            state.live.remove(id);
            state.net.remove(id);
            cage
        };
        let mut result = Ok(());
        for credential in cage.network.credentials() {
            if let Err(e) = self.vault.delete(id, &credential.name)
                && result.is_ok()
            {
                result = Err(e);
            }
        }
        for dir in [self.state_dir(id), self.bin_dir(id), self.data.join("chats").join(id), self.profile_dir(id)] {
            if let Err(e) = std::fs::remove_dir_all(dir)
                && e.kind() != std::io::ErrorKind::NotFound
                && result.is_ok()
            {
                result = Err(e.into());
            }
        }
        result
    }

    fn edit_workspaces(&self, edit: impl FnOnce(&mut Vec<Workspace>) -> Result<()>) -> Result<()> {
        let mut state = lock(&self.state);
        let mut workspaces = state.workspaces.clone();
        edit(&mut workspaces)?;
        self.workspace_store.save(&workspaces)?;
        state.workspaces = workspaces;
        Ok(())
    }

    pub fn set_layout(&self, session_id: &str, layout: Option<BTreeMap<String, Pos>>) -> Result<()> {
        self.edit_session(session_id, |s, _| {
            s.set_layout(layout);
            Ok(())
        })
    }

    /// Applies `edit` to the stored session and persists it; `edit` gets
    /// the effective cage of the session, if it has one. The change is kept
    /// only if the save succeeds. A running VM keeps its applied config
    /// until the next start.
    pub(crate) fn edit_session(
        &self,
        id: &str,
        edit: impl FnOnce(&mut Session, Option<&Cage>) -> Result<()>,
    ) -> Result<()> {
        let mut state = lock(&self.state);
        let mut sessions = state.sessions.clone();
        let session = sessions.iter_mut().find(|s| s.id == id).ok_or(Error::SessionNotFound)?;
        let cage = match effective(session, &state.workspaces) {
            Some(c) => Some(state.cage(&c.id)?),
            None => None,
        };
        edit(session, cage)?;
        if let Some(cage) = cage {
            Bound { session, cage }.check()?;
        }
        self.store.save_sessions(&sessions)?;
        state.sessions = sessions;
        Ok(())
    }

    /// Applies `edit` to the stored cage and persists it; `edit` gets the
    /// sessions of the cage. The change is kept only if the save succeeds.
    pub(crate) fn edit_cage(&self, id: &str, edit: impl FnOnce(&mut Cage, &[&Session]) -> Result<()>) -> Result<()> {
        let mut state = lock(&self.state);
        let mut cages = state.cages.clone();
        let cage = cages.iter_mut().find(|c| c.id == id).ok_or(Error::CageNotFound)?;
        let sessions = state.sessions_of(id);
        edit(cage, &sessions)?;
        cage.check()?;
        for session in &sessions {
            Bound { session, cage }.check()?;
        }
        self.store.save_cages(&cages)?;
        state.cages = cages;
        Ok(())
    }

    /// The user confirms that the folders with no identity, such as a folder
    /// that was missing at the first load after the update, are theirs: the
    /// session folder and the mounts of its cage.
    pub fn confirm_paths(&self, session_id: &str) -> Result<()> {
        if let Some(cage) = lock(&self.state).effective_cage(session_id)? {
            self.edit_cage(&cage.id, |c, _| {
                c.fill_ids();
                Ok(())
            })?;
        }
        self.edit_session(session_id, |s, _| {
            s.fill_ids();
            Ok(())
        })
    }

    pub fn add_mount(&self, cage_id: &str, host: &str, guest: &str, readonly: bool) -> Result<()> {
        self.edit_cage(cage_id, |c, sessions| {
            c.add_mount(sessions, host, guest, readonly)?;
            sessions.iter().try_for_each(|session| Bound { session, cage: c }.check_app_data(&self.reserved))
        })
    }

    pub fn remove_mount(&self, cage_id: &str, guest: &str) -> Result<()> {
        self.edit_cage(cage_id, |c, sessions| c.remove_mount(sessions, guest))
    }

    pub fn set_mount_readonly(&self, cage_id: &str, guest: &str, readonly: bool) -> Result<()> {
        self.edit_cage(cage_id, |c, _| {
            c.mount_mut(guest)?.readonly = readonly;
            Ok(())
        })
    }

    /// The cage keeps its name.
    pub fn rename(&self, session_id: &str, name: &str) -> Result<()> {
        self.edit_session(session_id, |s, _| s.rename(name))
    }

    pub fn set_harness(&self, session_id: &str, harness: Option<&str>) -> Result<()> {
        self.edit_session(session_id, |s, c| s.set_harness(c.ok_or(Error::NoCage)?, harness))
    }

    pub fn list_folders(&self, session_id: &str) -> Result<Vec<String>> {
        let folder = lock(&self.state).session(session_id)?.folder.clone();
        cage::list_folders(&folder)
    }

    /// Adds the host agent to the cage, or changes it: its runtime, and
    /// its profile and host login if `used` asks for them. A login adds the
    /// agent's host-login secret; `allow_hosts` also adds an Allow rule for
    /// its provider host, which the user confirmed. Applies on the next start.
    pub async fn put_host_agent(&self, cage_id: &str, used: HostAgentUse, allow_hosts: bool) -> Result<()> {
        let kind = used.kind;
        let host = self.host.clone();
        if blocking(move || Ok(host.last().agent(kind).is_none())).await? {
            return Err(Error::HostAgentMissing(kind.label().to_owned()));
        }
        let login = match (used.login, login::api_host(kind)) {
            (true, _) if kind == AgentKind::Claude => None,
            (false, _) => None,
            (true, Some(api)) => Some(login::secret(kind, api)),
            // pi's host depends on the provider of its login.
            (true, None) => {
                let host = self.host.clone();
                let token = blocking(move || host.token(kind)).await?;
                Some(login::secret(kind, &token.host))
            }
        };
        let login = login.map(crate::secrets::Secret::normalize).transpose()?;
        let _edit = lock(&self.secret_edit);
        self.edit_cage(cage_id, |c, _| {
            if let Some(secret) = &login
                && allow_hosts
            {
                for host in &secret.hosts {
                    if !c.network.rules.iter().any(|r| r.action == Action::Allow && r.matches(host)) {
                        let target = Target::Host { host: host.to_string() };
                        c.network.put_rule(RuleDraft { id: None, target, action: Action::Allow })?;
                    }
                }
            }
            c.put_host_agent(used, login);
            Ok(())
        })?;
        self.publish(cage_id, None);
        if kind == AgentKind::Claude {
            // The login off removed the token credential; its value goes too.
            self.drop_unused(cage_id, std::iter::once(cage::CLAUDE_TOKEN))?;
        }
        Ok(())
    }

    /// Also removes its host-login secret, and for Claude its token
    /// credential and value. Applies on the next start.
    pub fn remove_host_agent(&self, cage_id: &str, kind: AgentKind) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        self.edit_cage(cage_id, |c, _| {
            c.remove_host_agent(kind);
            Ok(())
        })?;
        self.publish(cage_id, None);
        if kind == AgentKind::Claude {
            self.drop_unused(cage_id, std::iter::once(cage::CLAUDE_TOKEN))?;
        }
        Ok(())
    }

    /// Needs no restart: a running VM gets new wrappers at once.
    pub fn set_agents_unprompted(&self, cage_id: &str, on: bool) -> Result<()> {
        self.edit_cage(cage_id, |c, _| {
            c.agents_unprompted = on;
            Ok(())
        })?;
        // The stored value, not `on`: a later edit may have won the race to the lock.
        let state = lock(&self.state);
        let unprompted = state.cage(cage_id)?.agents_unprompted;
        if let Some(Vm::Running { runtimes, host, .. }) = state.vms.get(cage_id) {
            launch::write(&self.bin_dir(cage_id), &mounted(runtimes), host.found(), unprompted)?;
        }
        Ok(())
    }

    /// Stops the VM that runs for the session and saves the sessions
    /// without it. The cage stays. Then it deletes the run records of the
    /// session.
    pub async fn delete(&self, session_id: &str) -> Result<()> {
        self.stop(session_id).await.or_else(|e| match e {
            Error::NotRunning | Error::CageNotFound => Ok(()),
            e => Err(e),
        })?;
        {
            let mut state = lock(&self.state);
            state.session(session_id)?;
            // A start can claim a VM for the session between the stop above and this lock.
            match state.vm_of(session_id) {
                Some((_, Vm::Starting { .. })) => return Err(Error::Starting),
                Some((_, Vm::Stopping { .. })) => return Err(Error::Stopping),
                Some(_) => return Err(Error::AlreadyStarted),
                None => {}
            }
            let sessions: Vec<Session> = state.sessions.iter().filter(|s| s.id != session_id).cloned().collect();
            let mut workspaces = state.workspaces.clone();
            for workspace in &mut workspaces {
                workspace.sessions.retain(|s| s != session_id);
            }
            self.save_both(&state.sessions, &sessions, &workspaces)?;
            state.sessions = sessions;
            state.workspaces = workspaces;
            // Host shells of the session can still run. Each kill waits for
            // the hangup in its own thread, not in this task.
            state.terminals.retain(|t| {
                let killed = t.session_id == session_id;
                if killed {
                    t.kill();
                }
                !killed
            });
        }
        self.runs.forget(session_id, None);
        match std::fs::remove_dir_all(self.data.join("tasks").join(session_id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// The cages whose VM starts, runs or stops. A failed stop leaves the
    /// VM running.
    pub fn live(&self) -> usize {
        lock(&self.state).vms.values().filter(|vm| !matches!(vm, Vm::Failed { .. })).count()
    }

    /// The app notices, such as the VMs that the app start removed. Each
    /// notice is returned once.
    pub fn take_app_notices(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.app_notices))
    }

    pub fn add_app_notice(&self, notice: String) {
        lock(&self.app_notices).push(notice);
    }
}

/// The cage with the state of its VM.
fn cage_view<V>(state: &State<V>, cage: &Cage, vault: &dyn Vault) -> CageView {
    let (status, pending_restart) = match state.vms.get(&cage.id) {
        None => (Status::Stopped, false),
        Some(Vm::Starting { phase, .. }) => (Status::Starting { phase: *phase }, false),
        Some(Vm::Running { applied, .. }) => {
            let session = state.session(&applied.0.id).ok();
            // The session now uses a different cage: it moves at the next start.
            let moved = session.is_some_and(|s| effective(s, &state.workspaces).is_none_or(|c| c.id != cage.id));
            (Status::Running, moved || pending_restart(applied, session, cage))
        }
        Some(Vm::Stopping { .. }) => (Status::Stopping, false),
        Some(Vm::Failed { message, .. }) => (Status::Error { message: message.clone() }, false),
    };
    let (runtimes, login_expires_at, host_notices, guest_logins, start_timings) = match state.vms.get(&cage.id) {
        Some(Vm::Running { runtimes, host, proxy, .. }) => (
            runtimes.clone(),
            proxy.login_expires(),
            host.notices.clone(),
            host.guest_logins.clone(),
            Some(host.timings.clone()),
        ),
        _ => (Vec::new(), None, Vec::new(), Vec::new(), None),
    };
    CageView {
        cage: cage.clone(),
        status,
        pending_restart,
        runtimes,
        login_expires_at,
        claude_token: claude_token(cage, vault),
        host_notices,
        guest_logins,
        start_timings,
    }
}

/// Shown when the Claude host login is on or a credential has the token name.
fn claude_token(cage: &Cage, vault: &dyn Vault) -> Option<ClaudeTokenView> {
    let login = cage.host_agent(AgentKind::Claude).is_some_and(|agent| agent.login);
    if !login && !cage.network.credentials().iter().any(|c| c.name == cage::CLAUDE_TOKEN) {
        return None;
    }
    let entry = vault.entry(&cage.id, cage::CLAUDE_TOKEN).ok().flatten();
    Some(ClaudeTokenView {
        stored: (!login || cage.claude_token_binding().unwrap_or(false))
            && entry.as_ref().is_some_and(|entry| entry.saved_at.is_some()),
        expires_at: entry.and_then(|entry| entry.saved_at.map(|at| at.saturating_add(365 * 24 * 60 * 60 * 1000))),
    })
}

/// A new name, layout, task, terminal list, unprompted setting or network
/// rule is used at once, so it needs no restart. A change of the session's
/// cage is not checked here. A new credential name
/// needs its placeholder in the guest env.
fn pending_restart(applied: &(Session, Cage), session: Option<&Session>, cage: &Cage) -> bool {
    let (old_session, old_cage) = applied;
    let session_changed = session.is_some_and(|session| {
        Session {
            name: session.name.clone(),
            layout: session.layout.clone(),
            tasks: session.tasks.clone(),
            terminals: session.terminals.clone(),
            pane_layout: session.pane_layout.clone(),
            cage: session.cage.clone(),
            ..old_session.clone()
        } != *session
    });
    let cage_changed = Cage {
        name: cage.name.clone(),
        agents_unprompted: cage.agents_unprompted,
        network: cage.network.clone(),
        ..old_cage.clone()
    } != *cage;
    session_changed || cage_changed || placeholders(&old_cage.network) != placeholders(&cage.network)
}

pub(crate) fn bin_dir(data: &Path, id: &str) -> PathBuf {
    data.join("bin").join(id)
}

/// Runs file reads off the async workers.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(|e| Error::Io(std::io::Error::other(e)))?
}

#[cfg(test)]
mod tests {
    use super::{EffectiveCage, Listing, reserved_dirs, task_guest_folder};
    use crate::cage::testing::*;
    use crate::workspaces::{self, Arrangement};

    #[test]
    fn task_guest_folder_uses_the_applied_members() {
        let (manager, _, first) = manager();
        let other_folder = temp_dir();
        let second = manager.create(other_folder.to_str().unwrap(), None).unwrap();
        let members: Vec<_> = manager.list().sessions.into_iter().map(|view| view.session).collect();
        let (only, folder, shared) = task_guest_folder(&members[..1], &first).unwrap();
        assert_eq!((only.id.as_str(), folder.as_str(), shared), (first.as_str(), WORKSPACE, false));
        let (member, folder, shared) = task_guest_folder(&members, &second).unwrap();
        assert_eq!(
            (member.folder.as_path(), folder, shared),
            (other_folder.as_path(), format!("{WORKSPACE}/{}", member.slug), true)
        );
        assert!(task_guest_folder(&members[..1], &second).is_none());
    }

    #[test]
    fn task_cage_refuses_a_session_without_a_cage() {
        let (manager, _, id) = manager();
        {
            let mut state = lock(&manager.state);
            state.sessions[0].cage = None;
            state.workspaces.iter_mut().for_each(|w| w.default_cage = None);
        }
        assert!(matches!(lock(&manager.state).task_cage(&id), Err(Error::NoCage)));
    }

    #[tokio::test]
    async fn delete_refuses_a_session_that_is_starting() {
        let (manager, _runtime, id) = manager();
        lock(&manager.state).vms.insert(id.clone(), Vm::Starting { session: id.clone(), phase: Phase::Vault });
        assert!(matches!(manager.delete(&id).await, Err(Error::Starting)));
        assert_eq!(manager.list().sessions.len(), 1);
    }

    #[tokio::test]
    async fn the_unprompted_setting_rewrites_the_wrappers_at_once() {
        let (manager, _, id) = manager();
        runtimes::fake_install(&manager.data, "claude");
        manager.start(&id).await.unwrap();
        let claude = || std::fs::read_to_string(manager.bin_dir(&id).join("claude")).unwrap();
        assert!(claude().contains("--dangerously-skip-permissions \"$@\""));

        manager.set_agents_unprompted(&id, false).unwrap();
        assert!(!claude().contains("dangerously"));
        assert!(!manager.list().cages[0].cage.agents_unprompted);
        assert!(!manager.list().cages[0].pending_restart);

        // Stopped: the next start writes the stored setting.
        manager.stop(&id).await.unwrap();
        manager.set_agents_unprompted(&id, true).unwrap();
        manager.start(&id).await.unwrap();
        assert!(claude().contains("dangerously"));
        manager.stop(&id).await.unwrap();
        manager.delete(&id).await.unwrap();
        manager.delete_cage(&id).unwrap();
        assert!(!manager.bin_dir(&id).exists());
    }

    #[tokio::test]
    async fn delete_cage_removes_the_agent_state_and_the_old_chats() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        let sessions = manager.state_dir(&id).join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        std::fs::write(sessions.join("a.jsonl"), "{\"type\":\"session\",\"timestamp\":\"t\"}\n").unwrap();
        let chats = manager.data.join("chats").join(&id).join("claude");
        std::fs::create_dir_all(&chats).unwrap();
        manager.stop(&id).await.unwrap();

        manager.delete(&id).await.unwrap();
        assert!(manager.state_dir(&id).exists(), "the cage stays");
        manager.delete_cage(&id).unwrap();
        assert!(!manager.state_dir(&id).exists());
        assert!(!manager.data.join("chats").join(&id).exists());
    }

    #[test]
    fn a_workspaces_file_that_does_not_parse_gives_the_default_workspace() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        std::fs::write(dir.join(workspaces::FILE), "{not json").unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let list = reloaded.workspaces();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, workspaces::DEFAULT_NAME);
        assert_eq!(list[0].sessions, [id]);
        assert_eq!(std::fs::read(dir.join("workspaces.json.bad")).unwrap(), b"{not json");
        assert!(dir.join(workspaces::FILE).exists());
    }

    #[test]
    fn a_failed_backup_blocks_each_later_workspace_save() {
        let dir = temp_dir();
        std::fs::write(dir.join(workspaces::FILE), "{not json").unwrap();
        // A non-empty directory at the backup path makes the move fail.
        std::fs::create_dir_all(dir.join("workspaces.json.bad").join("x")).unwrap();

        let manager =
            Manager::new(FakeRuntime::default(), Store::new(dir.clone()), vault(), temp_dir(), host()).unwrap();
        assert_eq!(manager.workspaces().len(), 1);
        assert!(matches!(manager.create_workspace(), Err(Error::WorkspacesLocked(_))));
        assert!(matches!(manager.create(dir.to_str().unwrap(), None), Err(Error::WorkspacesLocked(_))));
        assert_eq!(manager.workspaces().len(), 1);
        assert!(manager.list().sessions.is_empty());
        assert_eq!(std::fs::read(dir.join(workspaces::FILE)).unwrap(), b"{not json");
    }

    #[tokio::test]
    async fn config_persists_across_managers() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        manager.add_mount(&id, temp_dir().to_str().unwrap(), "/mnt/data", true).unwrap();
        manager.set_mount_readonly(&id, "/mnt/data", false).unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let mounts = &reloaded.list().cages[0].cage.mounts;
        assert_eq!(mounts.len(), 1);
        assert!(!mounts[0].readonly);

        reloaded.delete(&id).await.unwrap();
        assert!(
            Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host())
                .unwrap()
                .list()
                .sessions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn remove_mount_used_by_harness_is_refused() {
        let (manager, _, id) = manager();
        manager.add_mount(&id, temp_dir().to_str().unwrap(), "/mnt/agent", true).unwrap();
        manager.set_harness(&id, Some("/mnt/agent/skills")).unwrap();
        let error = manager.remove_mount(&id, "/mnt/agent").unwrap_err();
        assert!(error.to_string().contains("harness uses /mnt/agent/skills"), "{error}");
        assert_eq!(manager.list().cages[0].cage.mounts.len(), 1);

        manager.set_harness(&id, None).unwrap();
        manager.remove_mount(&id, "/mnt/agent").unwrap();
        assert!(manager.list().cages[0].cage.mounts.is_empty());
    }

    #[tokio::test]
    async fn rename_while_running_needs_no_restart() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        manager.rename(&id, " api ").unwrap();
        let list = manager.list();
        assert_eq!(list.sessions[0].session.name, "api");
        assert!(!list.cages[0].pending_restart);
        assert!(matches!(manager.rename(&id, "a\nb"), Err(Error::InvalidName(_))));
        manager.rename(&id, " ").unwrap();
        let session = &manager.list().sessions[0].session;
        assert_eq!(Some(session.name.as_str()), session.folder.file_name().and_then(|n| n.to_str()));
    }

    #[tokio::test]
    async fn layout_and_order_edits_need_no_restart() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        manager.set_layout(&id, Some([("harness".to_owned(), Pos { x: 1.0, y: 2.0 })].into())).unwrap();
        let workspace = manager.workspaces()[0].id.clone();
        manager.arrange_workspaces(vec![Arrangement { id: workspace, sessions: vec![id.clone()] }]).unwrap();
        let list = manager.list();
        assert!(!list.cages[0].pending_restart);
        assert_eq!(list.sessions[0].session.layout.len(), 1);
    }

    #[tokio::test]
    async fn workspaces_persist_and_follow_create_and_delete() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let first = manager.workspaces()[0].id.clone();
        let second = manager.create_workspace().unwrap();
        let a = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        let b = manager.create(temp_dir().to_str().unwrap(), Some(&second)).unwrap();
        assert!(matches!(manager.create(temp_dir().to_str().unwrap(), Some("nope")), Err(Error::WorkspaceNotFound)));
        assert_eq!(manager.list().sessions.len(), 2);
        assert!(matches!(manager.delete_workspace(&second), Err(Error::WorkspaceNotEmpty)));
        assert!(matches!(
            manager.arrange_workspaces(vec![Arrangement { id: first.clone(), sessions: vec![a.clone()] }]),
            Err(Error::InvalidOrder)
        ));
        manager
            .arrange_workspaces(vec![
                Arrangement { id: second.clone(), sessions: vec![b.clone(), a.clone()] },
                Arrangement { id: first.clone(), sessions: Vec::new() },
            ])
            .unwrap();
        manager.rename_workspace(&second, " Work ").unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let workspaces = reloaded.workspaces();
        assert_eq!(workspaces.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), [&second, &first]);
        assert_eq!(workspaces[0].name, "Work");
        assert_eq!(workspaces[0].sessions, [b.clone(), a.clone()]);

        reloaded.delete(&a).await.unwrap();
        assert_eq!(reloaded.workspaces()[0].sessions, std::slice::from_ref(&b));
        reloaded.delete_workspace(&first).unwrap();
        reloaded.delete(&b).await.unwrap();
        assert!(matches!(reloaded.delete_workspace(&second), Err(Error::LastWorkspace)));
        reloaded.rename_workspace(&second, "").unwrap();
        assert_eq!(reloaded.workspaces()[0].name, workspaces::DEFAULT_NAME);
    }

    #[test]
    fn reserved_dirs_are_canonical_and_include_the_zwinger_home() {
        let root = temp_dir();
        std::fs::create_dir_all(root.join("home/.zwinger")).unwrap();
        std::fs::create_dir_all(root.join("home/data")).unwrap();
        std::os::unix::fs::symlink(root.join("home"), root.join("link")).unwrap();
        let dirs = reserved_dirs(&root.join("link/data"), Some(root.join("link")));
        assert_eq!(dirs, [root.join("home/data"), root.join("home/.zwinger")]);
        assert_eq!(reserved_dirs(&root, None), std::slice::from_ref(&root));
    }

    #[test]
    fn create_refuses_a_folder_that_nests_with_app_data() {
        let manager =
            Manager::new(FakeRuntime::default(), Store::new(temp_dir()), vault(), temp_dir(), host()).unwrap();
        let data = manager.data.clone();
        std::fs::create_dir(data.join("sub")).unwrap();
        for folder in [data.parent().unwrap(), &data, &data.join("sub"), Path::new("/")] {
            let error = manager.create(folder.to_str().unwrap(), None).unwrap_err();
            assert!(matches!(error, Error::AppDataOverlap(..)), "{}: {error}", folder.display());
        }
        assert!(manager.list().sessions.is_empty());
    }

    #[test]
    fn add_mount_refuses_app_data_by_its_canonical_path() {
        let (manager, _, id) = manager();
        let link = temp_dir().join("link");
        std::os::unix::fs::symlink(&manager.data, &link).unwrap();
        std::fs::create_dir(manager.data.join("sub")).unwrap();
        for host in [link.clone(), link.join("sub")] {
            let error = manager.add_mount(&id, host.to_str().unwrap(), "/mnt/data", true).unwrap_err();
            assert!(matches!(error, Error::AppDataOverlap(..)), "{}: {error}", host.display());
        }
        assert!(manager.list().cages[0].cage.mounts.is_empty());
    }

    #[test]
    fn list_folders_reads_the_project_folder() {
        let (manager, _, id) = manager();
        let folder = manager.list().sessions[0].session.folder.clone();
        std::fs::create_dir(folder.join("web")).unwrap();
        assert_eq!(manager.list_folders(&id).unwrap(), ["web"]);
        assert!(matches!(manager.list_folders("nope"), Err(Error::SessionNotFound)));
    }

    #[tokio::test]
    async fn a_session_with_an_unknown_cage_fails_clearly() {
        let (manager, runtime, id) = manager();
        lock(&manager.state).sessions[0].cage = Some("gone".into());
        assert!(matches!(manager.start(&id).await, Err(Error::CageNotFound)));
        assert!(matches!(manager.open_session(&id, TerminalKind::Shell, 24, 80).await, Err(Error::NotRunning)));
        assert!(matches!(manager.set_harness(&id, None), Err(Error::CageNotFound)));
        assert!(manager.list().sessions[0].agent_needs.is_empty());
        assert!(runtime.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_saves_a_session_and_its_cage_and_delete_keeps_the_cage() {
        let dir = temp_dir();
        let manager =
            Manager::new(FakeRuntime::default(), Store::new(dir.clone()), vault(), temp_dir(), host()).unwrap();
        let folder = temp_dir().join("My App");
        std::fs::create_dir(&folder).unwrap();
        let id = manager.create(folder.to_str().unwrap(), None).unwrap();
        let other = manager.create(folder.to_str().unwrap(), None).unwrap();
        let list = manager.list();
        let slugs: Vec<_> =
            list.sessions.iter().map(|s| (s.session.slug.as_str(), s.session.cage.as_deref())).collect();
        assert_eq!(slugs, [("my-app", Some(id.as_str())), ("my-app-2", Some(other.as_str()))]);
        assert_eq!(list.cages.iter().map(|c| c.cage.id.clone()).collect::<Vec<_>>(), [id.clone(), other.clone()]);

        manager.delete(&id).await.unwrap();
        let (sessions, cages) = Store::new(dir).load().unwrap();
        assert_eq!(sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [other.as_str()]);
        assert_eq!(cages.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_cage_save_keeps_the_cage_and_its_values() {
        let (manager, _, id, vault) = manager_with_vault();
        credential(&manager, &id, "a.io", "A", Some("1")).unwrap();
        manager.delete(&id).await.unwrap();
        break_store(&manager);
        assert!(manager.delete_cage(&id).is_err());
        assert_eq!(manager.list().cages.len(), 1);
        assert_eq!(stored(&vault, &id, "A").as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn delete_cage_removes_the_values() {
        let (manager, _, id, vault) = manager_with_vault();
        credential(&manager, &id, "a.io", "A", Some("1")).unwrap();
        credential(&manager, &id, "b.io", "B", Some("2")).unwrap();
        manager.delete(&id).await.unwrap();
        assert_eq!(vault.values.lock().unwrap().len(), 2);
        manager.delete_cage(&id).unwrap();
        assert!(vault.values.lock().unwrap().is_empty());
        assert!(manager.list().cages.is_empty());
    }

    #[test]
    fn load_prunes_the_values_of_the_vault_file_with_no_credential() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let vault: Arc<dyn Vault> = Arc::new(secrets::FileVault::new(&temp_dir()).unwrap());
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        credential(&manager, &id, "a.io", "KEY", Some("keep")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.set("deleted-project", "KEY", &SecretValue::new("y")).unwrap();
        vault.set("zwinger", "PROXY_CA_KEY", &SecretValue::new("ca")).unwrap();

        Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        assert_eq!(vault.accounts().unwrap(), [secrets::account(&id, "KEY")]);
        assert_eq!(vault.get(&id, "KEY").unwrap().unwrap().expose(), "keep");
    }

    #[test]
    fn load_fails_on_a_session_with_no_cage_and_keeps_its_credentials() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let vault: Arc<dyn Vault> = Arc::new(secrets::FileVault::new(&temp_dir()).unwrap());
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        credential(&manager, &id, "a.io", "KEY", Some("keep")).unwrap();
        std::fs::write(store().cages_path(), "[]").unwrap();

        let Err(e) = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()) else {
            panic!("the load must fail");
        };
        assert!(matches!(&e, Error::MissingCage { owner, cage } if owner.contains(&id) && *cage == id));
        assert_eq!(vault.get(&id, "KEY").unwrap().unwrap().expose(), "keep");
    }

    #[test]
    fn rename_keeps_the_cage_name() {
        let (manager, _, id) = manager();
        let old = manager.list().cages[0].cage.name.clone();
        manager.rename(&id, "New name").unwrap();
        let (sessions, cages) = manager.store.load().unwrap();
        assert_eq!((sessions[0].name.as_str(), cages[0].name.as_str()), ("New name", old.as_str()));
        manager.rename_cage(&id, " Cage ").unwrap();
        assert_eq!(manager.list().cages[0].cage.name, "Cage");
        assert!(matches!(manager.rename_cage(&id, "a\nb"), Err(Error::InvalidName(_))));
    }

    /// A workspace with a default cage `shared`, and a session of `manager`
    /// in it that inherits that cage. Returns the workspace and the cage ids.
    fn inherit(manager: &Manager<FakeRuntime>, session: &str) -> (String, String) {
        let shared = manager.create_cage("shared").unwrap();
        let workspace = manager.workspaces()[0].id.clone();
        manager.set_workspace_cage(&workspace, Some(&shared)).unwrap();
        manager.set_session_cage(session, None).unwrap();
        (workspace, shared)
    }

    #[test]
    fn create_checks_the_session_against_the_workspace_default_cage() {
        let (manager, _, id) = manager();
        let (_, shared) = inherit(&manager, &id);
        let shared_dir = temp_dir();
        manager.add_mount(&shared, shared_dir.to_str().unwrap(), "/mnt/shared", false).unwrap();
        let folder = shared_dir.join("app");
        std::fs::create_dir(&folder).unwrap();
        let dir = manager.store.dir().to_owned();
        let files = || {
            [crate::store::SESSIONS, crate::store::CAGES, workspaces::FILE].map(|f| std::fs::read(dir.join(f)).unwrap())
        };
        let before = files();
        let error = manager.create(folder.to_str().unwrap(), None).unwrap_err();
        assert!(matches!(error, Error::MountOverlap(..)), "{error}");
        assert_eq!(files(), before);
        assert_eq!(manager.list().sessions.len(), 1);
    }

    fn effective(manager: &Manager<FakeRuntime>, session: &str) -> Option<EffectiveCage> {
        manager.list().sessions.into_iter().find(|s| s.session.id == session).unwrap().effective_cage
    }

    #[test]
    fn the_effective_cage_is_the_attached_cage_else_the_workspace_default() {
        let (manager, _, id) = manager();
        assert_eq!(effective(&manager, &id), Some(EffectiveCage { id: id.clone(), inherited: false }));
        let (workspace, shared) = inherit(&manager, &id);
        assert_eq!(effective(&manager, &id), Some(EffectiveCage { id: shared.clone(), inherited: true }));
        assert_eq!(lock(&manager.state).bound(&id).unwrap().cage.id, shared);

        // A new session in a workspace with a default inherits it and gets no cage.
        let other = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        assert_eq!(effective(&manager, &other), Some(EffectiveCage { id: shared.clone(), inherited: true }));
        assert_eq!(manager.list().cages.len(), 2);

        manager.set_workspace_cage(&workspace, None).unwrap();
        assert_eq!(effective(&manager, &id), None);
        assert!(matches!(lock(&manager.state).bound(&id), Err(Error::NoCage)));
        assert!(matches!(manager.set_session_cage(&id, Some("nope")), Err(Error::CageNotFound)));
        assert!(matches!(manager.set_workspace_cage(&workspace, Some("nope")), Err(Error::CageNotFound)));

        // The choices persist.
        manager.set_workspace_cage(&workspace, Some(&shared)).unwrap();
        let reloaded = Manager::new(
            FakeRuntime::default(),
            Store::new(manager.store.dir().to_owned()),
            vault(),
            temp_dir(),
            host(),
        )
        .unwrap();
        assert_eq!(effective(&reloaded, &id), Some(EffectiveCage { id: shared, inherited: true }));
    }

    #[tokio::test]
    async fn a_session_with_no_cage_opens_mac_shells_only() {
        host_sh();
        let (manager, _, id) = manager();
        let workspace = manager.workspaces()[0].id.clone();
        manager.set_session_cage(&id, None).unwrap();
        assert!(matches!(manager.start(&id).await, Err(Error::NoCage)));
        assert!(matches!(manager.set_harness(&id, Some("/workspace")), Err(Error::NoCage)));
        manager.set_layout(&id, None).unwrap();
        manager.rename(&id, "x").unwrap();
        let terminal = manager.open_session(&id, TerminalKind::Host, 24, 80).await.unwrap();
        manager.close_terminal(&id, &terminal).unwrap();
        assert!(manager.workspaces().iter().any(|w| w.id == workspace));
    }

    #[test]
    fn delete_cage_refuses_a_cage_in_use() {
        let (manager, _, id) = manager();
        let in_use = |e: Result<()>| matches!(e, Err(Error::CageInUse(..)));
        assert!(in_use(manager.delete_cage(&id)), "attached");
        let (workspace, shared) = inherit(&manager, &id);
        assert!(in_use(manager.delete_cage(&shared)), "inherited");
        // The session no longer uses it, but the workspace default still names it.
        manager.set_session_cage(&id, Some(&id)).unwrap();
        let error = manager.delete_cage(&shared).unwrap_err();
        assert!(error.to_string().contains("default cage of workspace"), "{error}");
        manager.set_workspace_cage(&workspace, None).unwrap();
        manager.delete_cage(&shared).unwrap();
        assert!(matches!(manager.delete_cage(&shared), Err(Error::CageNotFound)));
        assert_eq!(manager.list().cages.len(), 1);
    }

    #[tokio::test]
    async fn a_cage_that_runs_for_one_session_refuses_a_second() {
        let (manager, runtime, a) = manager();
        let (_, shared) = inherit(&manager, &a);
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        manager.start(&a).await.unwrap();
        let error = manager.start(&b).await.unwrap_err();
        assert!(matches!(&error, Error::CageBusy { .. }), "{error}");
        let name = manager.list().sessions[0].session.name.clone();
        assert_eq!(error.to_string(), format!("cage shared runs for session {name}; stop it first"));
        assert!(matches!(manager.stop(&b).await, Err(Error::NotRunning)));
        let list = manager.list();
        let vm_cage = |id: &str| list.sessions.iter().find(|s| s.session.id == id).unwrap().vm_cage.clone();
        assert_eq!((vm_cage(&a), vm_cage(&b)), (Some(shared.clone()), None));

        // A cage whose VM runs is in use, also with no session on it.
        manager.set_session_cage(&a, Some(&a)).unwrap();
        manager.set_session_cage(&b, Some(&a)).unwrap();
        let workspace = manager.workspaces()[0].id.clone();
        manager.set_workspace_cage(&workspace, None).unwrap();
        assert!(matches!(manager.delete_cage(&shared), Err(Error::CageInUse(..))));

        manager.stop(&a).await.unwrap();
        manager.set_session_cage(&b, Some(&shared)).unwrap();
        manager.start(&b).await.unwrap();
        assert_eq!(runtime.log.lock().unwrap().iter().filter(|l| l.starts_with("start")).count(), 2);
    }

    #[tokio::test]
    async fn a_failed_start_shows_only_on_its_session() {
        let (manager, runtime, a) = manager();
        let (_, shared) = inherit(&manager, &a);
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        *runtime.node_version.lock().unwrap() = Some("v22.12.0\n".into());
        assert!(manager.start(&a).await.is_err());
        let list = manager.list();
        let vm_cage = |id: &str| list.sessions.iter().find(|s| s.session.id == id).unwrap().vm_cage.clone();
        assert_eq!((vm_cage(&a), vm_cage(&b)), (Some(shared), None));
    }

    #[tokio::test]
    async fn a_move_with_a_running_vm_keeps_it_until_it_stops() {
        let (manager, runtime, id) = manager();
        let first = manager.workspaces()[0].id.clone();
        let second = manager.create_workspace().unwrap();
        let shared = manager.create_cage("shared").unwrap();
        manager.set_workspace_cage(&second, Some(&shared)).unwrap();
        manager.set_workspace_cage(&first, Some(&id)).unwrap();
        manager.set_session_cage(&id, None).unwrap();
        manager.start(&id).await.unwrap();
        let cage = |list: &Listing, id: &str| list.cages.iter().find(|c| c.cage.id == id).unwrap().pending_restart;

        let moved = |to: &str| {
            let other = if to == first { &second } else { &first };
            vec![
                Arrangement { id: to.to_owned(), sessions: vec![id.clone()] },
                Arrangement { id: other.clone(), sessions: Vec::new() },
            ]
        };
        manager.arrange_workspaces(moved(&second)).unwrap();
        let list = manager.list();
        assert_eq!(list.sessions[0].effective_cage.as_ref().map(|c| c.id.as_str()), Some(shared.as_str()));
        assert_eq!(list.sessions[0].vm_cage.as_deref(), Some(id.as_str()));
        assert!(cage(&list, &id), "the running VM waits for a restart");
        // The old VM still serves the session's terminals.
        manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        assert!(matches!(manager.start(&id).await, Err(Error::AlreadyStarted)));

        // Restart: the stop finds the old VM, the start uses the new cage.
        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        let list = manager.list();
        assert_eq!(list.sessions[0].vm_cage.as_deref(), Some(shared.as_str()));
        assert!(!cage(&list, &shared));
        let log = runtime.log.lock().unwrap().clone();
        let starts: Vec<_> = log.iter().filter(|l| l.starts_with("start")).cloned().collect();
        assert_eq!(starts, [format!("start {id}"), format!("start {shared}")]);
    }

    #[test]
    fn a_move_that_breaks_the_harness_is_refused() {
        let (manager, _, id) = manager();
        manager.add_mount(&id, temp_dir().to_str().unwrap(), "/mnt/agent", true).unwrap();
        manager.set_harness(&id, Some("/mnt/agent")).unwrap();
        let first = manager.workspaces()[0].id.clone();
        let second = manager.create_workspace().unwrap();
        let shared = manager.create_cage("shared").unwrap();
        manager.set_workspace_cage(&first, Some(&id)).unwrap();
        manager.set_workspace_cage(&second, Some(&shared)).unwrap();
        manager.set_session_cage(&id, None).unwrap();
        let wanted = vec![
            Arrangement { id: second.clone(), sessions: vec![id.clone()] },
            Arrangement { id: first.clone(), sessions: Vec::new() },
        ];
        assert!(matches!(manager.arrange_workspaces(wanted), Err(Error::HarnessTarget(_))));
        assert_eq!(manager.workspaces()[0].sessions, std::slice::from_ref(&id));
        assert!(matches!(manager.set_session_cage(&id, Some(&shared)), Err(Error::HarnessTarget(_))));
    }

    #[test]
    fn failed_delete_is_an_error_but_does_not_stop_the_load() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let vault = vault();
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        *vault.writes_left.lock().unwrap() = Some(0);

        assert!(matches!(manager.set_rule_credential(&id, &rule, "KEY", None, None), Err(Error::Vault(_))));
        let reloaded = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        assert_eq!(reloaded.list().sessions.len(), 1);
        assert_eq!(stored(&vault, &id, "GONE").as_deref(), Some("x"));
    }
}
