use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
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
use crate::error::{Error, Member, Result};
use crate::host_agents::HostAgents;
use crate::host_agents::login;
use crate::network::{self, Action, RuleDraft, Source, Target};
use crate::proxy::ca::Ca;
use crate::proxy::log::Entry;
use crate::proxy::{Live, Proxy};
use crate::runtime::{SandboxRuntime, TerminalKind};
use crate::secrets::{Account, Owner, Vault, VaultEntry};
use crate::session::{Pos, Session, VmChoice};
use crate::store::{self, Store};
use crate::task_runs;
use crate::terminal::{AgentState, Terminal, TerminalFolder, TerminalView};
use crate::workspaces::{self, Arrangement, Workspace, WorkspaceStore};

/// The network log entries kept per VM.
pub(crate) const NET_RING: usize = 2000;

/// The dir of the per-VM agent state. It keeps its name from before the
/// rename to projects, so the agent state of existing cages stays.
pub(crate) const LEGACY_STATE_DIR: &str = "shields";

/// Gets each new network log entry with its VM key.
pub type NetSink = Box<dyn Fn(&str, &Entry) + Send + Sync>;

/// Gets the process id and the new agent state of a terminal.
pub type AgentSink = Box<dyn Fn(&str, Option<AgentState>) + Send + Sync>;

/// Gets the process id, the order and the new folder of a live terminal.
/// `None`: unknown.
pub type FolderSink = Box<dyn Fn(&str, u64, Option<&TerminalFolder>) + Send + Sync>;

/// Gets the key of a VM whose guest logins changed.
pub type GuestSink = Box<dyn Fn(&str) + Send + Sync>;

/// Owns the sessions and cages, the running VMs and their terminals.
pub struct Manager<R: SandboxRuntime> {
    pub(crate) runtime: R,
    pub(crate) store: Store,
    pub(crate) workspace_store: WorkspaceStore,
    /// The app data dir. It holds the agent runtimes, `shields/<key>`, the
    /// agent state of each VM, and `bin/<key>`, the host-only agent
    /// wrappers of each VM.
    pub(crate) data: PathBuf,
    /// The user's home dir. [`Manager::reserved`] resolves `~/.zwinger`
    /// below it at each check, so a link that is made later is seen.
    pub(crate) home: Option<PathBuf>,
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
    pub(crate) unsettled: Mutex<HashMap<Account, Option<VaultEntry>>>,
    pub(crate) state: Arc<Mutex<State<R::Vm>>>,
    pub(crate) net_sink: Arc<OnceLock<NetSink>>,
    pub(crate) agent_sink: Arc<OnceLock<AgentSink>>,
    pub(crate) folder_sink: Arc<OnceLock<FolderSink>>,
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
    /// Per VM key: the shared VM of a cage has the cage id, an own VM of a
    /// session has its stored key. The sessions of the VM are its members. A
    /// member keeps it after its cage or its VM choice changes, until it stops.
    pub(crate) vms: HashMap<String, Vm<V>>,
    pub(crate) terminals: Vec<Terminal>,
    /// Per VM that starts or runs, the policy and the credential values
    /// of its proxy. An edit of its cage changes them at once.
    pub(crate) live: HashMap<String, Arc<Live>>,
    /// Per VM key, the last network log entries. They stay after a stop, so
    /// the user can still allow a blocked host.
    pub(crate) net: HashMap<String, VecDeque<Entry>>,
    /// The generation of the last VM that started. A stop names the
    /// generation it resolved, so a delayed stop cannot hit a later VM.
    pub(crate) generation: u64,
    /// The sessions that a restart to add joins to a VM, from the first
    /// check until the new start claims them. No other start takes them.
    pub(crate) joining: HashSet<String>,
}

/// A VM of cage `cage`. A running VM has its cage in `applied`.
pub(crate) enum Vm<V> {
    /// The phase that the start for the `members` runs now.
    Starting { cage: String, members: Vec<String>, phase: Phase },
    /// `applied` is the member sessions and the cage the VM booted with; they differ
    /// from the stored config after an edit, until the next start. `proxy`
    /// is the cage's proxy; a drop stops it. `net` follows the VM's network
    /// events. It is `None` if the runtime log could not be opened.
    /// `runtimes` are the agent runtimes that the start mounted.
    /// `generation` is unique to this start.
    Running {
        generation: u64,
        vm: Arc<V>,
        applied: Box<(Vec<Session>, Cage)>,
        proxy: Arc<Proxy>,
        net: Option<AbortHandle>,
        runtimes: Vec<&'static str>,
        host: Box<HostRun>,
    },
    /// Shutdown is in progress. If it fails, the VM goes back to `Running`.
    /// It still belongs to the `members` until the shutdown succeeds.
    Stopping { cage: String, members: Vec<String> },
    /// The start for `session` failed with `message`.
    Failed { cage: String, session: String, message: String },
}

impl<V> Vm<V> {
    pub(crate) fn cage(&self) -> &str {
        match self {
            Vm::Starting { cage, .. } | Vm::Stopping { cage, .. } | Vm::Failed { cage, .. } => cage,
            Vm::Running { applied, .. } => &applied.1.id,
        }
    }
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
    /// The key of that VM. With no such VM, the key of the VM that the
    /// session uses next, if it has one: its network log stays after a stop.
    pub vm_key: Option<String>,
    /// The session's folder in the running VM, if the session is a member.
    pub guest_folder: Option<String>,
    /// The folder where the session's agents start in the running VM, if
    /// the session is a member.
    pub guest_harness: Option<String>,
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
    pub claude_token: Option<ClaudeTokenView>,
    /// The shared VM of the cage, also when it is stopped, then by key the
    /// own VMs that a session stores, also when they are stopped, and the
    /// own VMs that start, run, stop or failed.
    pub vms: Vec<VmView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VmView {
    pub key: String,
    /// The VM of one session alone, not the shared VM of the cage.
    pub own: bool,
    pub status: Status,
    /// The names of the member sessions of the VM that starts, runs or stops.
    pub members: Vec<String>,
    pub pending_restart: bool,
    /// The agent runtimes that the running VM has mounted. A host Gemini CLI
    /// that loads counts as one.
    pub runtimes: Vec<&'static str>,
    /// The earliest expiry of the host logins that the proxy of the running
    /// VM has now, in ms. The proxy reads a newer login when it is due.
    pub login_expires_at: Option<i64>,
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
    /// The token comes from the credential library.
    pub library: bool,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    Stopped,
    Starting { phase: Phase },
    Running,
    Stopping,
    Error { message: String },
}

/// Deletes the local values that no configured credential uses, such as the
/// values of a cage whose delete failed. A library value stays, also with no
/// cage that uses it. A failure does not stop the app load.
pub(crate) fn prune_vault(vault: &dyn Vault, cages: &[Cage]) {
    let accounts = match vault.accounts() {
        Ok(accounts) => accounts,
        Err(e) => return eprintln!("secret prune: cannot list the vault: {e}"),
    };
    // A host login has no value in the vault, so its account is not kept.
    let configured: HashSet<Account> = cages
        .iter()
        .flat_map(|cage| cage.network.credentials().into_iter().map(|c| c.key().account(&cage.id)))
        .collect();
    for account in accounts.iter().filter(|a| a.0 != Owner::Library && !configured.contains(*a)) {
        let (owner, name) = account;
        if vault.delete(owner, name).is_err() {
            eprintln!("secret prune: cannot delete {name} of {owner:?}");
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
        let (member, folder, shared) = task_guest_folder(&applied.0, id).ok_or(Error::NotRunning)?;
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

    /// The key and the VM that starts, runs or stops with the session as a
    /// member. A session is a member of at most one VM: after a change of
    /// its cage or VM choice it stays a member of the old VM until that stops.
    pub(crate) fn vm_of(&self, session_id: &str) -> Option<(&str, &Vm<V>)> {
        self.vms
            .iter()
            .find(|(_, vm)| match vm {
                Vm::Starting { members, .. } | Vm::Stopping { members, .. } => members.iter().any(|m| m == session_id),
                Vm::Running { applied, .. } => applied.0.iter().any(|m| m.id == session_id),
                Vm::Failed { .. } => false,
            })
            .map(|(id, vm)| (id.as_str(), vm))
    }

    /// The key of the VM that the session uses in its effective cage.
    /// `None`: no cage, or an own key of a different cage.
    pub(crate) fn planned_key(&self, session_id: &str) -> Option<String> {
        self.bound(session_id).ok().and_then(|b| cage::vm_key(b.session, b.cage).map(str::to_owned))
    }

    /// The proxy values of each VM of the cage that starts or runs.
    pub(crate) fn lives_of(&self, cage_id: &str) -> Vec<Arc<Live>> {
        let of_cage = |key: &String| self.vms.get(key).is_some_and(|vm| vm.cage() == cage_id);
        self.live.iter().filter(|(key, _)| of_cage(key)).map(|(_, live)| live.clone()).collect()
    }

    /// A key is taken while a session stores it, a VM has it, or a cage has
    /// it as its id.
    pub(crate) fn key_taken(&self, key: &str) -> bool {
        self.vms.contains_key(key)
            || self.cages.iter().any(|c| c.id == key)
            || self.sessions.iter().any(|s| matches!(&s.vm, VmChoice::Own { key: k, .. } if k == key))
    }

    /// Each member id with the current name of its session.
    pub(crate) fn members(&self, ids: impl IntoIterator<Item = String>) -> Vec<Member> {
        ids.into_iter()
            .map(|id| Member { name: self.session(&id).map_or_else(|_| id.clone(), |s| s.name.clone()), id })
            .collect()
    }

    /// Numbers the entry and adds it to the VM's ring.
    pub(crate) fn push_net(&mut self, key: &str, mut event: Entry) -> Entry {
        let ring = self.net.entry(key.to_owned()).or_default();
        event.seq = ring.back().map_or(1, |last| last.seq + 1);
        if ring.len() == NET_RING {
            ring.pop_front();
        }
        ring.push_back(event.clone());
        event
    }
}

/// The guest folder of member `id` of a VM: `/workspace` for the only
/// member, else `/workspace/<slug>` (ADR 0007). The flag is true when the VM
/// has more members. Each guest path of a session folder comes from here.
pub(crate) fn task_guest_folder<'a>(members: &'a [Session], id: &str) -> Option<(&'a Session, String, bool)> {
    let member = members.iter().find(|s| s.id == id)?;
    let shared = members.len() > 1;
    let folder = if shared { format!("{WORKSPACE}/{}", member.slug) } else { WORKSPACE.to_owned() };
    Some((member, folder, shared))
}

/// A stored guest path of a session, such as its harness or a task workdir,
/// is relative to `/workspace`. This puts it below the session's guest
/// `folder`. A path below a mount stays.
pub(crate) fn in_guest_folder(folder: &str, stored: &str) -> String {
    match stored.strip_prefix(WORKSPACE) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{folder}{rest}"),
        _ => stored.to_owned(),
    }
}

pub(crate) fn ids(sessions: &[Session]) -> Vec<String> {
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
    /// The canonical dirs that no session folder or mount may nest with:
    /// the app data dir and `~/.zwinger`.
    pub(crate) fn reserved(&self) -> Vec<PathBuf> {
        reserved_dirs(&self.data, self.home.clone())
    }

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
            home: dirs::home_dir(),
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
                joining: HashSet::new(),
            })),
            net_sink: Arc::new(OnceLock::new()),
            agent_sink: Arc::new(OnceLock::new()),
            folder_sink: Arc::new(OnceLock::new()),
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
    pub fn set_folder_sink(&self, sink: FolderSink) {
        let _ = self.folder_sink.set(sink);
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
            .map(|session| {
                // A failed start shows on the session that started it.
                let vm = state.vm_of(&session.id).or_else(|| {
                    state
                        .vms
                        .iter()
                        .find(|(_, vm)| matches!(vm, Vm::Failed { session: s, .. } if *s == session.id))
                        .map(|(key, vm)| (key.as_str(), vm))
                });
                let guest = match vm {
                    Some((_, Vm::Running { applied, .. })) => task_guest_folder(&applied.0, &session.id),
                    _ => None,
                };
                let (guest_folder, guest_harness) = guest.map_or((None, None), |(member, folder, _)| {
                    let harness =
                        member.harness.as_deref().map_or_else(|| folder.clone(), |h| in_guest_folder(&folder, h));
                    (Some(folder), Some(harness))
                });
                SessionView {
                    guest_folder,
                    guest_harness,
                    agent_needs: state.bound(&session.id).map_or_else(|_| Vec::new(), network::agent_needs),
                    effective_cage: effective(session, &state.workspaces),
                    vm_cage: vm.map(|(_, vm)| vm.cage().to_owned()),
                    vm_key: vm.map(|(key, _)| key.to_owned()).or_else(|| state.planned_key(&session.id)),
                    processes: state
                        .terminals
                        .iter()
                        .filter(|t| t.session_id == session.id)
                        .map(|t| {
                            let output = lock(&t.output);
                            let live = t.exit_code.is_none();
                            TerminalView {
                                id: t.id.clone(),
                                kind: t.kind,
                                exit_code: t.exit_code,
                                terminal_id: t.terminal_id.clone(),
                                agent: output
                                    .osc
                                    .agent
                                    .filter(|_| live && matches!(t.kind, TerminalKind::Agent { .. })),
                                folder: output.folder.clone().filter(|_| live),
                                folder_seq: output.folder_seq,
                            }
                        })
                        .collect(),
                    session: session.clone(),
                }
            })
            .collect();
        let cages = state.cages.iter().map(|cage| cage_view(&state, cage, self.vault.as_ref())).collect();
        Listing { sessions, cages }
    }

    /// The host-only dir of the VM's agent wrappers, bound read-only at
    /// [`launch::BIN_DIR`]. It is not in the state dir: the guest can write that.
    pub(crate) fn bin_dir(&self, key: &str) -> PathBuf {
        bin_dir(&self.data, key)
    }

    pub(crate) fn state_dir(&self, key: &str) -> PathBuf {
        self.data.join(LEGACY_STATE_DIR).join(key)
    }

    /// The host-owned profile snapshots of the VM's host agents.
    pub(crate) fn profile_dir(&self, key: &str) -> PathBuf {
        self.data.join("profiles").join(key)
    }

    /// Deletes all that is left of the VM `key`: its agent state, its
    /// wrappers, its profiles, its network log and a failed start. Under the
    /// state lock, so no start can take the key meanwhile. The VM must not
    /// start, run or stop.
    pub(crate) fn wipe_vm(&self, state: &mut State<R::Vm>, key: &str) -> Result<()> {
        if state.vms.get(key).is_some_and(|vm| !matches!(vm, Vm::Failed { .. })) {
            return Err(Error::AlreadyStarted);
        }
        state.vms.remove(key);
        state.live.remove(key);
        state.net.remove(key);
        self.remove_vm_dirs(key)
    }

    /// Deletes the agent state, the wrappers and the profiles of VM `key`.
    /// It tries each dir and returns the first error.
    pub(crate) fn remove_vm_dirs(&self, key: &str) -> Result<()> {
        let mut result = Ok(());
        for dir in [self.state_dir(key), self.bin_dir(key), self.profile_dir(key)] {
            if let Err(source) = std::fs::remove_dir_all(&dir)
                && source.kind() != std::io::ErrorKind::NotFound
                && result.is_ok()
            {
                result = Err(Error::VmStateLeft { key: key.to_owned(), path: dir.display().to_string(), source });
            }
        }
        result
    }

    /// Reserves a new own-VM key of cage `cage_id` with an empty state. A
    /// key whose old state cannot be deleted is not handed out.
    pub(crate) fn new_vm_key(&self, state: &mut State<R::Vm>, cage_id: &str) -> Result<String> {
        let key = cage::free_vm_key(cage_id, |key| state.key_taken(key));
        self.wipe_vm(state, &key)?;
        Ok(key)
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
        bound.check_app_data(&self.reserved())
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
    /// whose VMs do not run. Also deletes its secret values, its old chats,
    /// and the agent state, the wrappers and the profiles of its VMs. A
    /// session that kept an own VM of the cage after a cage change goes
    /// back to the shared choice. The first error is returned after all
    /// deletes ran.
    pub fn delete_cage(&self, id: &str) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        let (cage, own_keys) = {
            let mut state = lock(&self.state);
            let cage = state.cage(id)?.clone();
            let in_use = |reason: String| Err(Error::CageInUse(cage.name.clone(), reason));
            if let Some(session) = state.sessions_of(id).first() {
                return in_use(format!("session {} uses it", session.name));
            }
            if let Some(w) = state.workspaces.iter().find(|w| w.default_cage.as_deref() == Some(id)) {
                return in_use(format!("it is the default cage of workspace {}", w.name));
            }
            if state.vms.values().any(|vm| vm.cage() == id && !matches!(vm, Vm::Failed { .. })) {
                return in_use("its VM runs".into());
            }
            // The sessions are saved first: a failed cage save then leaves only free keys.
            let mut sessions = state.sessions.clone();
            let mut own_keys = Vec::new();
            for session in &mut sessions {
                if let VmChoice::Own { key, cage } = &session.vm
                    && cage == id
                {
                    own_keys.push(key.clone());
                    session.vm = VmChoice::Shared;
                }
            }
            if !own_keys.is_empty() {
                self.store.save_sessions(&sessions)?;
                state.sessions = sessions;
            }
            let cages: Vec<Cage> = state.cages.iter().filter(|c| c.id != id).cloned().collect();
            self.store.save_cages(&cages)?;
            state.cages = cages;
            let mut keys: Vec<String> =
                state.vms.iter().filter(|(_, vm)| vm.cage() == id).map(|(k, _)| k.clone()).collect();
            keys.push(id.to_owned());
            keys.extend(own_keys.iter().cloned());
            for key in keys {
                state.vms.remove(&key);
                state.live.remove(&key);
                state.net.remove(&key);
            }
            (cage, own_keys)
        };
        let mut result = Ok(());
        // The library values stay for the other cages and for later use.
        for credential in cage.network.credentials().into_iter().filter(|c| c.source.is_local()) {
            if let Err(e) = self.vault.delete(&Owner::cage(id), &credential.name)
                && result.is_ok()
            {
                result = Err(e);
            }
        }
        if let Err(e) = std::fs::remove_dir_all(self.data.join("chats").join(id))
            && e.kind() != std::io::ErrorKind::NotFound
            && result.is_ok()
        {
            result = Err(e.into());
        }
        for key in std::iter::once(id).chain(own_keys.iter().map(String::as_str)) {
            if let Err(e) = self.remove_vm_dirs(key)
                && result.is_ok()
            {
                result = Err(e);
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
        let reserved = self.reserved();
        self.edit_cage(cage_id, |c, sessions| {
            c.add_mount(sessions, host, guest, readonly)?;
            // The new mount first: an old mount must not hide its own error.
            let new = c.mounts.last().expect("add_mount pushed the mount");
            if let Some(dir) = cage::reserved_overlap(&new.host, &reserved) {
                return Err(Error::AppDataOverlap(new.host.display().to_string(), dir.display().to_string()));
            }
            c.check_app_data(&reserved)?;
            sessions.iter().try_for_each(|session| Bound { session, cage: c }.check_app_data(&reserved))
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

    pub fn set_editor(&self, session_id: &str, editor: Option<&str>) -> Result<()> {
        self.edit_session(session_id, |s, _| s.set_editor(editor))
    }

    /// The editor override of the session.
    pub fn session_editor(&self, session_id: &str) -> Result<Option<String>> {
        Ok(lock(&self.state).session(session_id)?.editor.clone())
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
            // The login off removed the token credential; its local value goes too.
            self.drop_unused(cage_id, cage::claude_token_refs())?;
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
            self.drop_unused(cage_id, cage::claude_token_refs())?;
        }
        Ok(())
    }

    /// Needs no restart: each running VM of the cage gets new wrappers at once.
    pub fn set_agents_unprompted(&self, cage_id: &str, on: bool) -> Result<()> {
        self.edit_cage(cage_id, |c, _| {
            c.agents_unprompted = on;
            Ok(())
        })?;
        // The stored value, not `on`: a later edit may have won the race to the lock.
        let state = lock(&self.state);
        let unprompted = state.cage(cage_id)?.agents_unprompted;
        for (key, vm) in &state.vms {
            if let Vm::Running { runtimes, host, applied, .. } = vm
                && applied.1.id == cage_id
            {
                launch::write(&self.bin_dir(key), &mounted(runtimes), host.found(), unprompted)?;
            }
        }
        Ok(())
    }

    /// Stops the VM that runs for the session and saves the sessions
    /// without it. The cage stays. Then it deletes the run records of the
    /// session. A VM with other members is not stopped: the delete fails
    /// with [`Error::StopShared`], and the user stops that VM first. An own
    /// VM of the session loses its state; if that fails, the delete still
    /// completes and returns [`Error::VmStateLeft`].
    pub async fn delete(&self, session_id: &str) -> Result<()> {
        self.stop(session_id, None).await.or_else(|e| match e {
            Error::NotRunning | Error::CageNotFound => Ok(()),
            e => Err(e),
        })?;
        let wiped = {
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
            let released = match &state.session(session_id)?.vm {
                VmChoice::Own { key, .. } => Some(key.clone()),
                VmChoice::Shared => None,
            };
            self.save_both(&state.sessions, &sessions, &workspaces)?;
            state.sessions = sessions;
            state.workspaces = workspaces;
            let wiped = match released {
                Some(key) => self.wipe_vm(&mut state, &key),
                None => Ok(()),
            };
            // Host shells of the session can still run. Each kill waits for
            // the hangup in its own thread, not in this task.
            state.terminals.retain(|t| {
                let killed = t.session_id == session_id;
                if killed {
                    t.kill();
                }
                !killed
            });
            wiped
        };
        self.runs.forget(session_id, None);
        match std::fs::remove_dir_all(self.data.join("tasks").join(session_id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => wiped,
        }
    }

    /// The VMs that start, run or stop. A failed stop leaves the
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

/// The cage with the state of its VMs.
fn cage_view<V>(state: &State<V>, cage: &Cage, vault: &dyn Vault) -> CageView {
    let stored = state.sessions.iter().filter_map(|s| match &s.vm {
        VmChoice::Own { key, cage: of } if *of == cage.id => Some(key),
        _ => None,
    });
    let tracked = state.vms.iter().filter(|(key, vm)| **key != cage.id && vm.cage() == cage.id).map(|(key, _)| key);
    let own: BTreeSet<&String> = stored.chain(tracked).collect();
    let shared = vm_view(state, cage, &cage.id, state.vms.get(&cage.id));
    let vms = std::iter::once(shared).chain(own.into_iter().map(|key| vm_view(state, cage, key, state.vms.get(key))));
    CageView { cage: cage.clone(), claude_token: claude_token(cage, vault), vms: vms.collect() }
}

fn vm_view<V>(state: &State<V>, cage: &Cage, key: &str, vm: Option<&Vm<V>>) -> VmView {
    let (status, pending_restart, members) = match vm {
        None => (Status::Stopped, false, Vec::new()),
        Some(Vm::Starting { phase, members, .. }) => (Status::Starting { phase: *phase }, false, members.clone()),
        Some(Vm::Running { applied, .. }) => {
            let (old_members, old_cage) = applied.as_ref();
            // A member that now uses a different cage or VM, or was deleted, leaves at the next start.
            let changed = old_members.iter().any(|old| match state.session(&old.id) {
                Ok(session) => state.planned_key(&session.id).is_none_or(|k| k != key) || session_changed(old, session),
                Err(_) => true,
            });
            (Status::Running, changed || cage_changed(old_cage, cage), ids(old_members))
        }
        Some(Vm::Stopping { members, .. }) => (Status::Stopping, false, members.clone()),
        Some(Vm::Failed { message, .. }) => (Status::Error { message: message.clone() }, false, Vec::new()),
    };
    let members = state.members(members).into_iter().map(|m| m.name).collect();
    let (runtimes, login_expires_at, host_notices, guest_logins, start_timings) = match vm {
        Some(Vm::Running { runtimes, host, proxy, .. }) => (
            runtimes.clone(),
            proxy.login_expires(),
            host.notices.clone(),
            host.guest_logins.clone(),
            Some(host.timings.clone()),
        ),
        _ => (Vec::new(), None, Vec::new(), Vec::new(), None),
    };
    VmView {
        key: key.to_owned(),
        own: key != cage.id,
        status,
        members,
        pending_restart,
        runtimes,
        login_expires_at,
        host_notices,
        guest_logins,
        start_timings,
    }
}

#[cfg(test)]
impl CageView {
    /// The view of the shared VM.
    pub(crate) fn shared(&self) -> VmView {
        self.vms[0].clone()
    }
}

/// Shown when the Claude host login is on or a credential has the token name.
fn claude_token(cage: &Cage, vault: &dyn Vault) -> Option<ClaudeTokenView> {
    let login = cage.host_agent(AgentKind::Claude).is_some_and(|agent| agent.login);
    if !login && !cage.network.credentials().iter().any(|c| c.name == cage::CLAUDE_TOKEN) {
        return None;
    }
    let key = cage.claude_token_ref();
    let entry = vault.entry(&key.vault_key(&cage.id), &key.name).ok().flatten();
    Some(ClaudeTokenView {
        library: key.source == Source::Library,
        stored: (!login || cage.claude_token_binding().unwrap_or(false))
            && entry.as_ref().is_some_and(|entry| entry.saved_at.is_some()),
        expires_at: entry.and_then(|entry| entry.saved_at.map(|at| at.saturating_add(365 * 24 * 60 * 60 * 1000))),
    })
}

/// A new name, layout, task, terminal list, editor, unprompted setting or
/// network rule is used at once, so it needs no restart. A change of the session's
/// cage or VM choice is not checked here.
fn session_changed(old: &Session, session: &Session) -> bool {
    Session {
        name: session.name.clone(),
        layout: session.layout.clone(),
        tasks: session.tasks.clone(),
        terminals: session.terminals.clone(),
        pane_layout: session.pane_layout.clone(),
        editor: session.editor.clone(),
        cage: session.cage.clone(),
        vm: session.vm.clone(),
        ..old.clone()
    } != *session
}

/// As [`session_changed`]. A new credential name needs its placeholder in the guest env.
fn cage_changed(old: &Cage, cage: &Cage) -> bool {
    Cage {
        name: cage.name.clone(),
        agents_unprompted: cage.agents_unprompted,
        network: cage.network.clone(),
        ..old.clone()
    } != *cage
        || placeholders(&old.network) != placeholders(&cage.network)
}

pub(crate) fn bin_dir(data: &Path, key: &str) -> PathBuf {
    data.join("bin").join(key)
}

/// Runs file reads off the async workers.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(|e| Error::Io(std::io::Error::other(e)))?
}

#[cfg(test)]
mod tests {
    use super::{EffectiveCage, Listing, in_guest_folder, reserved_dirs, task_guest_folder};
    use crate::cage::lifecycle::Confirmed;
    use crate::cage::testing::*;
    use crate::error::Member;
    use crate::session::VmChoice;
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
        // The stored paths of a session move with its guest folder; a mount path stays.
        let shared = format!("{WORKSPACE}/{}", member.slug);
        assert_eq!(in_guest_folder(WORKSPACE, "/workspace/src"), "/workspace/src");
        assert_eq!(in_guest_folder(&shared, WORKSPACE), shared);
        assert_eq!(in_guest_folder(&shared, "/workspace/src/app"), format!("{shared}/src/app"));
        assert_eq!(in_guest_folder(&shared, "/workspaces"), "/workspaces");
        assert_eq!(in_guest_folder(&shared, "/mnt/data"), "/mnt/data");
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
        lock(&manager.state)
            .vms
            .insert(id.clone(), Vm::Starting { cage: id.clone(), members: vec![id.clone()], phase: Phase::Vault });
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
        assert!(!manager.list().cages[0].shared().pending_restart);

        // Stopped: the next start writes the stored setting.
        manager.stop(&id, None).await.unwrap();
        manager.set_agents_unprompted(&id, true).unwrap();
        manager.start(&id).await.unwrap();
        assert!(claude().contains("dangerously"));
        manager.stop(&id, None).await.unwrap();
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
        manager.stop(&id, None).await.unwrap();

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
    async fn a_project_editor_is_set_changed_cleared_and_kept_without_a_restart() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        assert_eq!(manager.session_editor(&id).unwrap(), None);
        manager.start(&id).await.unwrap();
        manager.set_editor(&id, Some("cursor")).unwrap();
        assert!(!manager.list().cages[0].shared().pending_restart);
        assert_eq!(manager.list().sessions[0].session.editor.as_deref(), Some("cursor"));
        manager.set_editor(&id, Some("zed")).unwrap();
        assert!(matches!(manager.set_editor(&id, Some("notepad")), Err(Error::UnknownEditor(_))));
        assert!(matches!(manager.set_editor("gone", None), Err(Error::SessionNotFound)));

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        assert_eq!(reloaded.session_editor(&id).unwrap().as_deref(), Some("zed"));
        reloaded.set_editor(&id, None).unwrap();
        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        assert_eq!(reloaded.session_editor(&id).unwrap(), None);
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
        assert!(!list.cages[0].shared().pending_restart);
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
        assert!(!list.cages[0].shared().pending_restart);
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

    /// An old config can hold a mount of the app data: the add check is newer.
    fn put_old_mount(manager: &Manager<FakeRuntime>, cage_id: &str, host: &Path) {
        let host = host.to_str().unwrap();
        manager.edit_cage(cage_id, |c, sessions| c.add_mount(sessions, host, "/mnt/data", true)).unwrap();
    }

    fn cage_of(manager: &Manager<FakeRuntime>, id: &str) -> crate::cage::Cage {
        manager.list().cages.into_iter().map(|c| c.cage).find(|c| c.id == id).unwrap()
    }

    #[test]
    fn an_old_app_data_mount_blocks_a_new_mount_until_it_is_removed() {
        let (manager, _, id) = manager();
        put_old_mount(&manager, &id, &manager.data);
        let before = cage_of(&manager, &id);
        let other = temp_dir();
        let other = other.to_str().unwrap();

        let error = manager.add_mount(&id, other, "/mnt/other", true).unwrap_err();
        assert!(
            matches!(&error, Error::MountAppData { cage, guest, .. } if *cage == before.name && guest == "/mnt/data"),
            "{error}"
        );
        assert_eq!(cage_of(&manager, &id), before);

        // The new mount is invalid too: its own error comes first.
        std::fs::create_dir(manager.data.join("sub")).unwrap();
        let error = manager.add_mount(&id, manager.data.join("sub").to_str().unwrap(), "/mnt/sub", true).unwrap_err();
        assert!(matches!(error, Error::AppDataOverlap(..)), "{error}");

        manager.remove_mount(&id, "/mnt/data").unwrap();
        manager.add_mount(&id, other, "/mnt/other", true).unwrap();
        assert_eq!(cage_of(&manager, &id).mounts.len(), 1);
    }

    #[test]
    fn add_mount_checks_a_cage_with_no_session() {
        let (manager, _, _) = manager();
        let cage_id = manager.create_cage("spare").unwrap();
        put_old_mount(&manager, &cage_id, &manager.data);
        let error = manager.add_mount(&cage_id, temp_dir().to_str().unwrap(), "/mnt/other", true).unwrap_err();
        assert!(
            matches!(&error, Error::MountAppData { cage, guest, .. } if cage == "spare" && guest == "/mnt/data"),
            "{error}"
        );
        assert_eq!(cage_of(&manager, &cage_id).mounts.len(), 1);
    }

    #[test]
    fn add_mount_sees_a_zwinger_home_link_made_after_the_load() {
        let (mut manager, _, id) = manager();
        let home = temp_dir();
        manager.home = Some(home.clone());
        let target = temp_dir();
        std::os::unix::fs::symlink(&target, home.join(".zwinger")).unwrap();
        let error = manager.add_mount(&id, target.to_str().unwrap(), "/mnt/data", true).unwrap_err();
        assert!(matches!(error, Error::AppDataOverlap(..)), "{error}");
        assert!(cage_of(&manager, &id).mounts.is_empty());
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
        vault.set(&Owner::cage(&id), "GONE", &SecretValue::new("x")).unwrap();
        vault.set(&Owner::cage("deleted-project"), "KEY", &SecretValue::new("y")).unwrap();
        vault.set(&Owner::cage("zwinger"), "PROXY_CA_KEY", &SecretValue::new("ca")).unwrap();
        // A cage value of a cage with the old id `@library` is a cage value.
        vault.set(&Owner::cage("@library"), "SHARED", &SecretValue::new("legacy")).unwrap();
        // A library value with no cage that attaches it stays.
        vault.set(&Owner::Library, "SHARED", &SecretValue::new("s")).unwrap();

        Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let accounts: std::collections::HashSet<_> = vault.accounts().unwrap().into_iter().collect();
        assert_eq!(
            accounts,
            std::collections::HashSet::from([
                (Owner::Library, "SHARED".to_owned()),
                (Owner::Cage(id.clone().into()), "KEY".to_owned())
            ])
        );
        assert_eq!(vault.get(&Owner::cage(&id), "KEY").unwrap().unwrap().expose(), "keep");
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
        assert_eq!(vault.get(&Owner::cage(&id), "KEY").unwrap().unwrap().expose(), "keep");
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
    async fn a_second_session_joins_the_running_vm_by_a_confirmed_restart() {
        let (manager, runtime, a) = manager();
        let (_, shared) = inherit(&manager, &a);
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        let session = |id: &str| manager.list().sessions.into_iter().find(|s| s.session.id == id).unwrap().session;
        let (sa, sb) = (session(&a), session(&b));
        std::fs::create_dir(sa.folder.join("src")).unwrap();
        manager.set_harness(&a, Some("/workspace/src")).unwrap();
        manager.start(&a).await.unwrap();
        assert_eq!(*runtime.folders.lock().unwrap(), [(WORKSPACE.to_owned(), sa.folder.clone())]);
        let guest = |id: &str| {
            let view = manager.list().sessions.into_iter().find(|s| s.session.id == id).unwrap();
            (view.guest_folder, view.guest_harness)
        };
        assert_eq!(guest(&a), (Some(WORKSPACE.to_owned()), Some(format!("{WORKSPACE}/src"))));
        assert_eq!(guest(&b), (None, None));
        let shell = manager.open_session(&a, TerminalKind::Shell, 24, 80).await.unwrap();

        let Err(Error::RestartToAdd { members, generation, .. }) = manager.start(&b).await else {
            panic!("a running VM without the session asks for a restart");
        };
        assert_eq!(members, [Member { id: a.clone(), name: sa.name.clone() }]);
        let expected = [a.clone()];
        let stale = manager.restart_to_add(&b, &expected, generation + 1).await;
        assert!(matches!(stale, Err(Error::StaleRestart)), "{stale:?}");
        let stale = manager.restart_to_add(&b, std::slice::from_ref(&b), generation).await;
        assert!(matches!(stale, Err(Error::StaleRestart)), "{stale:?}");
        assert!(matches!(manager.stop(&b, None).await, Err(Error::NotRunning)));

        manager.restart_to_add(&b, &expected, generation).await.unwrap();
        let folders = runtime.folders.lock().unwrap().clone();
        assert_eq!(
            folders,
            [
                (format!("{WORKSPACE}/{}", sa.slug), sa.folder.clone()),
                (format!("{WORKSPACE}/{}", sb.slug), sb.folder.clone())
            ]
        );
        let list = manager.list();
        let vm_cage = |id: &str| list.sessions.iter().find(|s| s.session.id == id).unwrap().vm_cage.clone();
        assert_eq!((vm_cage(&a), vm_cage(&b)), (Some(shared.clone()), Some(shared.clone())));
        let view = list.cages.iter().find(|c| c.cage.id == shared).unwrap();
        assert_eq!(view.shared().members, [sa.name.clone(), sb.name.clone()]);
        assert!(!view.shared().pending_restart);
        assert!(matches!(manager.start(&b).await, Err(Error::AlreadyStarted)));
        assert!(lock(&manager.state).task_cage(&b).unwrap().4);
        let (folder_a, folder_b) = (format!("{WORKSPACE}/{}", sa.slug), format!("{WORKSPACE}/{}", sb.slug));
        assert_eq!(guest(&a), (Some(folder_a.clone()), Some(format!("{folder_a}/src"))));
        assert_eq!(guest(&b), (Some(folder_b.clone()), Some(folder_b)));

        // The VM shell of `a` opened again in the new VM, at its new folder.
        let processes = &list.sessions.iter().find(|s| s.session.id == a).unwrap().processes;
        assert_eq!(processes.len(), 1);
        assert_ne!(processes[0].id, shell);
        assert_eq!(processes[0].exit_code, None);
        assert_eq!(runtime.cwds.lock().unwrap().last(), Some(&folder_a));
        assert!(manager.take_app_notices().is_empty());

        // Each terminal starts in its own session's guest folder.
        manager.open_session(&b, TerminalKind::Shell, 24, 80).await.unwrap();
        assert_eq!(runtime.cwds.lock().unwrap().last(), Some(&format!("{WORKSPACE}/{}", sb.slug)));

        // A stop or a delete that also ends the other member asks first.
        let Err(Error::StopShared { members, generation, .. }) = manager.stop(&a, None).await else {
            panic!("a stop of a shared VM asks first");
        };
        assert_eq!(members, [Member { id: b.clone(), name: sb.name.clone() }]);
        let Err(Error::StopShared { generation: deleted, .. }) = manager.delete(&b).await else {
            panic!("a delete of a member of a shared VM asks first");
        };
        assert_eq!(deleted, generation);
        manager.stop(&a, Some(&Confirmed { members: vec![b.clone()], generation })).await.unwrap();
        let list = manager.list();
        assert!(list.sessions.iter().all(|s| s.vm_cage.is_none()));
        assert!(list.sessions.iter().all(|s| s.processes.is_empty()));

        // A stopped VM starts with only the session that starts it, at `/workspace`.
        manager.start(&b).await.unwrap();
        assert_eq!(*runtime.folders.lock().unwrap(), [(WORKSPACE.to_owned(), sb.folder.clone())]);
        assert_eq!(runtime.log.lock().unwrap().iter().filter(|l| l.starts_with("start")).count(), 3);
    }

    /// Two sessions of a cage in one VM. Returns the second session.
    async fn two_members(manager: &Manager<FakeRuntime>, a: &str) -> String {
        inherit(manager, a);
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        manager.start(a).await.unwrap();
        let Err(Error::RestartToAdd { generation, .. }) = manager.start(&b).await else { panic!("restart to add") };
        manager.restart_to_add(&b, &[a.to_owned()], generation).await.unwrap();
        b
    }

    fn running_generation(manager: &Manager<FakeRuntime>, session: &str) -> u64 {
        match lock(&manager.state).vm_of(session) {
            Some((_, Vm::Running { generation, .. })) => *generation,
            _ => panic!("the VM of the session runs"),
        }
    }

    #[tokio::test]
    async fn a_stale_confirmed_stop_is_refused() {
        let (manager, _, a) = manager();
        let b = two_members(&manager, &a).await;
        let generation = running_generation(&manager, &b);
        for stale in [
            Confirmed { members: vec![b.clone()], generation: generation + 1 },
            Confirmed { members: vec![], generation },
            Confirmed { members: vec![a.clone(), b.clone()], generation },
        ] {
            let result = manager.stop(&a, Some(&stale)).await;
            assert!(matches!(result, Err(Error::StaleRestart)), "{result:?}");
        }
        assert_eq!(running_generation(&manager, &a), generation);
        // The order of the members does not matter.
        manager.stop(&b, Some(&Confirmed { members: vec![a.clone()], generation })).await.unwrap();
    }

    #[tokio::test]
    async fn a_restart_keeps_all_members_and_their_terminals() {
        let (manager, runtime, a) = manager();
        let b = two_members(&manager, &a).await;
        manager.open_session(&b, TerminalKind::Shell, 24, 80).await.unwrap();
        let before = runtime.folders.lock().unwrap().clone();
        let Err(Error::StopShared { members, generation, .. }) = manager.restart(&a, None).await else {
            panic!("a restart of a shared VM asks first");
        };
        assert_eq!(members.len(), 1);
        manager.restart(&a, Some(&Confirmed { members: vec![b.clone()], generation })).await.unwrap();
        assert_eq!(*runtime.folders.lock().unwrap(), before);
        assert!(running_generation(&manager, &a) > generation);
        let list = manager.list();
        let view = |id: &str| list.sessions.iter().find(|s| s.session.id == id).unwrap();
        assert_eq!(view(&a).vm_cage, view(&b).vm_cage);
        assert!(view(&b).processes.iter().all(|p| p.exit_code.is_none()));
        assert_eq!(view(&b).processes.len(), 1);
    }

    #[tokio::test]
    async fn a_reopen_after_a_second_stop_does_not_register_in_a_dead_vm() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.pty_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let restarting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.restart(&id, None).await }
        });
        // The new VM runs and the reopen waits in `open_pty`.
        gate.entered.notified().await;
        manager.stop(&id, None).await.unwrap();
        *runtime.pty_gate.lock().unwrap() = None;
        gate.release.notify_one();

        restarting.await.unwrap().unwrap();
        assert!(manager.list().sessions[0].processes.is_empty());
        let notices = manager.take_app_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].starts_with("The VM shell of "), "{notices:?}");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !runtime.log.lock().unwrap().contains(&"kill".to_owned()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reopened process was not killed");
    }

    #[tokio::test]
    async fn a_restart_to_add_drops_members_that_left_the_cage() {
        let (manager, runtime, a) = manager();
        let (_, shared) = inherit(&manager, &a);
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        let c = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        manager.start(&a).await.unwrap();
        let Err(Error::RestartToAdd { generation, .. }) = manager.start(&b).await else { panic!("restart to add") };
        manager.restart_to_add(&b, std::slice::from_ref(&a), generation).await.unwrap();

        // `a` moves to its own cage: the VM waits for a restart, and the restart drops it.
        manager.set_session_cage(&a, Some(&a)).unwrap();
        let pending = |m: &Manager<FakeRuntime>| m.list().cages.into_iter().find(|v| v.cage.id == shared).unwrap();
        assert!(pending(&manager).shared().pending_restart);
        let Err(Error::RestartToAdd { members, generation, .. }) = manager.start(&c).await else {
            panic!("restart to add")
        };
        let members: Vec<String> = members.into_iter().map(|m| m.id).collect();
        assert_eq!(members, [a.clone(), b.clone()]);
        manager.restart_to_add(&c, &members, generation).await.unwrap();
        let folders: Vec<String> = runtime.folders.lock().unwrap().iter().map(|(guest, _)| guest.clone()).collect();
        let slug = |id: &str| manager.list().sessions.into_iter().find(|s| s.session.id == id).unwrap().session.slug;
        assert_eq!(folders, [format!("{WORKSPACE}/{}", slug(&b)), format!("{WORKSPACE}/{}", slug(&c))]);
        assert!(!pending(&manager).shared().pending_restart);
        assert_eq!(manager.list().sessions.iter().find(|s| s.session.id == a).unwrap().vm_cage, None);
    }

    #[tokio::test]
    async fn a_restart_to_add_with_nested_folders_keeps_the_vm() {
        let (manager, _, a) = manager();
        inherit(&manager, &a);
        let folder = manager.list().sessions[0].session.folder.join("inner");
        std::fs::create_dir(&folder).unwrap();
        let b = manager.create(folder.to_str().unwrap(), None).unwrap();
        manager.start(&a).await.unwrap();
        let Err(Error::RestartToAdd { generation, .. }) = manager.start(&b).await else { panic!("restart to add") };
        let error = manager.restart_to_add(&b, std::slice::from_ref(&a), generation).await.unwrap_err();
        assert!(matches!(error, Error::MountOverlap(..)), "{error}");
        assert!(matches!(manager.start(&a).await, Err(Error::AlreadyStarted)), "the old VM still runs");
    }

    /// The joining session stays reserved while the old VM stops: it cannot
    /// start in a different cage and then also join this VM.
    #[tokio::test]
    async fn a_restart_to_add_reserves_the_joining_session_during_the_stop() {
        let (manager, runtime, a) = manager();
        let (_, shared) = inherit(&manager, &a);
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        manager.start(&a).await.unwrap();
        let Err(Error::RestartToAdd { generation, .. }) = manager.start(&b).await else { panic!("restart to add") };

        let gate = Arc::new(PtyGate::default());
        *runtime.stop_gate.lock().unwrap() = Some(gate.clone());
        let manager = Arc::new(manager);
        let restart = tokio::spawn({
            let (manager, a, b) = (manager.clone(), a.clone(), b.clone());
            async move { manager.restart_to_add(&b, std::slice::from_ref(&a), generation).await }
        });
        gate.entered.notified().await;
        let other = manager.create_cage("other").unwrap();
        manager.set_session_cage(&b, Some(&other)).unwrap();
        assert!(matches!(manager.start(&b).await, Err(Error::Starting)));
        manager.set_session_cage(&b, None).unwrap();

        *runtime.stop_gate.lock().unwrap() = None;
        gate.release.notify_one();
        restart.await.unwrap().unwrap();
        let list = manager.list();
        let vm_cage = |id: &str| list.sessions.iter().find(|s| s.session.id == id).unwrap().vm_cage.clone();
        assert_eq!((vm_cage(&a), vm_cage(&b)), (Some(shared.clone()), Some(shared)));
        assert!(lock(&manager.state).joining.is_empty());
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
        let cage =
            |list: &Listing, id: &str| list.cages.iter().find(|c| c.cage.id == id).unwrap().shared().pending_restart;

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
        manager.stop(&id, None).await.unwrap();
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

    /// A second session `b` of the cage of `a`, in its own VM.
    fn own_member(manager: &Manager<FakeRuntime>, a: &str, folder: &Path) -> String {
        let b = manager.create(folder.to_str().unwrap(), None).unwrap();
        manager.set_session_cage(&b, Some(a)).unwrap();
        b
    }

    #[tokio::test]
    async fn an_own_vm_gets_the_lowest_free_key_and_the_shared_vm_keeps_the_cage_id() {
        let (manager, _, a) = manager();
        let b = own_member(&manager, &a, &temp_dir());
        assert_eq!((vm_key(&manager, &a), vm_key(&manager, &b)), (Some(a.clone()), Some(a.clone())));
        manager.set_session_vm(&a, true, None).await.unwrap();
        manager.set_session_vm(&b, true, None).await.unwrap();
        let (ka, kb) = (format!("{a}-2"), format!("{a}-3"));
        assert_eq!((vm_key(&manager, &a), vm_key(&manager, &b)), (Some(ka.clone()), Some(kb.clone())));
        let stored = |id: &str| lock(&manager.state).session(id).unwrap().vm.clone();
        assert_eq!(stored(&a), VmChoice::Own { key: ka.clone(), cage: a.clone() });
        // A free key is used again; a choice that does not change is no error.
        manager.set_session_vm(&a, false, None).await.unwrap();
        manager.set_session_vm(&a, false, None).await.unwrap();
        assert_eq!(stored(&a), VmChoice::Shared);
        manager.set_session_vm(&a, true, None).await.unwrap();
        assert_eq!(vm_key(&manager, &a), Some(ka));
        // The choice persists.
        let reloaded = Manager::new(
            FakeRuntime::default(),
            Store::new(manager.store.dir().to_owned()),
            vault(),
            temp_dir(),
            host(),
        )
        .unwrap();
        assert_eq!(vm_key(&reloaded, &b), Some(kb));
    }

    #[tokio::test]
    async fn two_vms_of_one_cage_run_at_once() {
        let (manager, runtime, a) = manager();
        let b = own_member(&manager, &a, &temp_dir());
        manager.set_session_vm(&b, true, None).await.unwrap();
        let own = format!("{a}-2");
        manager.start(&a).await.unwrap();
        manager.start(&b).await.unwrap();
        assert_eq!(vm_states(&manager, &a), [(a.clone(), Status::Running), (own.clone(), Status::Running)]);
        let starts: Vec<String> = runtime.log.lock().unwrap().clone();
        assert_eq!(starts, [format!("start {a}"), format!("start {own}")]);
        assert!(manager.state_dir(&a).is_dir() && manager.state_dir(&own).is_dir());
        assert_ne!(manager.state_dir(&a), manager.state_dir(&own));
        let list = manager.list();
        let vm_cage = |id: &str| list.sessions.iter().find(|s| s.session.id == id).unwrap().vm_cage.clone();
        assert_eq!((vm_cage(&a), vm_cage(&b)), (Some(a.clone()), Some(a.clone())));
        assert_eq!(list.cages[0].vms[1].members, [list.sessions[1].session.name.clone()]);
        assert!(list.cages[0].vms[1].own && !list.cages[0].vms[0].own);

        // A policy edit reaches both proxies; a mount edit waits for both restarts.
        let rule = allow(&manager, &a, "a.io");
        for key in [&a, &own] {
            assert_eq!(live(&manager, key).snapshot().policy.rules[0].id, rule);
        }
        manager.add_mount(&a, temp_dir().to_str().unwrap(), "/mnt/data", true).unwrap();
        assert!(manager.list().cages[0].vms.iter().all(|v| v.pending_restart));

        // A stop of one VM keeps the other.
        manager.stop(&a, None).await.unwrap();
        assert_eq!(vm_states(&manager, &a), [(a.clone(), Status::Stopped), (own.clone(), Status::Running)]);
        manager.stop(&b, None).await.unwrap();
        assert!(matches!(manager.delete_cage(&a), Err(Error::CageInUse(..))));
    }

    #[tokio::test]
    async fn the_folders_of_a_cage_may_not_nest_across_its_vms() {
        let (manager, _, a) = manager();
        let inner = manager.list().sessions[0].session.folder.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let b = own_member(&manager, &a, &inner);
        manager.set_session_vm(&b, true, None).await.unwrap();
        assert!(matches!(manager.start(&a).await, Err(Error::MountOverlap(..))));
        assert!(matches!(manager.start(&b).await, Err(Error::MountOverlap(..))));
    }

    #[tokio::test]
    async fn a_switch_of_a_running_session_leaves_its_vm() {
        let (manager, runtime, a) = manager();
        let b = two_members(&manager, &a).await;
        let shared = vm_key(&manager, &a).unwrap();
        manager.open_session(&a, TerminalKind::Shell, 24, 80).await.unwrap();
        manager.open_session(&b, TerminalKind::Shell, 24, 80).await.unwrap();
        let Err(Error::StopShared { members, generation, .. }) = manager.set_session_vm(&b, true, None).await else {
            panic!("a switch out of a shared VM asks first");
        };
        assert_eq!(members.into_iter().map(|m| m.id).collect::<Vec<_>>(), std::slice::from_ref(&a));
        let stale = Confirmed { members: vec![a.clone()], generation: generation + 1 };
        assert!(matches!(manager.set_session_vm(&b, true, Some(&stale)).await, Err(Error::StaleRestart)));
        let confirmed = Confirmed { members: vec![a.clone()], generation };
        manager.set_session_vm(&b, true, Some(&confirmed)).await.unwrap();

        // The shared VM runs again with `a` alone and its shell; `b` runs no VM.
        let own = format!("{shared}-2");
        let list = manager.list();
        let view = |id: &str| list.sessions.iter().find(|s| s.session.id == id).unwrap();
        assert_eq!((view(&a).vm_key.clone(), view(&b).vm_key.clone()), (Some(shared.clone()), Some(own.clone())));
        assert_eq!(view(&b).vm_cage, None);
        assert!(view(&b).processes.is_empty());
        assert_eq!(view(&a).processes.len(), 1);
        assert_eq!(*runtime.folders.lock().unwrap(), [(WORKSPACE.to_owned(), view(&a).session.folder.clone())]);
        assert!(manager.take_app_notices().is_empty());

        // Back to the shared VM: the own VM stops and its state goes.
        manager.start(&b).await.unwrap();
        assert!(manager.state_dir(&own).is_dir());
        manager.set_session_vm(&b, false, None).await.unwrap();
        assert!(!manager.state_dir(&own).exists());
        assert_eq!(vm_key(&manager, &b), Some(shared.clone()));
        assert_eq!(vm_states(&manager, &shared), [(shared, Status::Running)]);
    }

    #[tokio::test]
    async fn a_reused_key_gets_an_empty_state() {
        let (manager, _, a) = manager();
        manager.set_session_vm(&a, true, None).await.unwrap();
        let own = format!("{a}-2");
        manager.start(&a).await.unwrap();
        let login = put_codex_login(&manager, &own);
        lock(&manager.state).push_net(
            &own,
            Entry::direct(&crate::netlog::parse(&dns_denied("2999-01-01T00:00:00Z", "a.io")).unwrap()).unwrap(),
        );
        manager.stop(&a, None).await.unwrap();
        assert!(login.exists() && !manager.list_net_events(&own).unwrap().is_empty());
        manager.set_session_vm(&a, false, None).await.unwrap();
        assert!(!manager.state_dir(&own).exists());
        assert!(manager.list_net_events(&own).unwrap().is_empty());

        // A folder that a failed delete left is gone before the key is handed out again.
        put_codex_login(&manager, &own);
        manager.set_session_vm(&a, true, None).await.unwrap();
        assert_eq!(vm_key(&manager, &a), Some(own.clone()));
        assert!(!manager.state_dir(&own).exists());
    }

    #[tokio::test]
    async fn an_own_key_of_an_old_cage_is_replaced_at_the_start() {
        let (manager, runtime, a) = manager();
        manager.set_session_vm(&a, true, None).await.unwrap();
        let old = format!("{a}-2");
        manager.start(&a).await.unwrap();
        let login = put_codex_login(&manager, &old);
        manager.stop(&a, None).await.unwrap();
        let other = manager.create_cage("other").unwrap();
        manager.set_session_cage(&a, Some(&other)).unwrap();
        // No key of the new cage yet: the session has no planned VM.
        assert_eq!(vm_key(&manager, &a), None);
        manager.start(&a).await.unwrap();
        let new = format!("{other}-2");
        assert_eq!(vm_key(&manager, &a), Some(new.clone()));
        assert!(!login.exists() && !manager.state_dir(&old).exists());
        assert_eq!(runtime.log.lock().unwrap().last(), Some(&format!("start {new}")));
    }

    #[tokio::test]
    async fn delete_removes_the_state_of_the_own_vm() {
        let (manager, _, a) = manager();
        manager.set_session_vm(&a, true, None).await.unwrap();
        let own = format!("{a}-2");
        manager.start(&a).await.unwrap();
        assert!(manager.state_dir(&own).is_dir());
        manager.delete(&a).await.unwrap();
        assert!(!manager.state_dir(&own).exists() && !manager.bin_dir(&own).exists());
    }

    #[tokio::test]
    async fn a_stopped_own_vm_is_listed_with_its_key() {
        let (manager, _, a) = manager();
        manager.set_session_vm(&a, true, None).await.unwrap();
        let own = format!("{a}-2");
        assert_eq!(vm_states(&manager, &a), [(a.clone(), Status::Stopped), (own.clone(), Status::Stopped)]);
        manager.start(&a).await.unwrap();
        manager.stop(&a, None).await.unwrap();
        assert_eq!(vm_states(&manager, &a), [(a.clone(), Status::Stopped), (own.clone(), Status::Stopped)]);
        assert!(manager.list().cages[0].vms[1].own);
    }

    /// Puts a file where the agent state dir of VM `key` goes, so its delete fails.
    fn block_state_delete(manager: &Manager<FakeRuntime>, key: &str) {
        let dir = manager.state_dir(key);
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(dir, "x").unwrap();
    }

    #[tokio::test]
    async fn a_failed_state_delete_is_an_error_and_the_key_is_not_handed_out() {
        let (manager, _, a) = manager();
        manager.set_session_vm(&a, true, None).await.unwrap();
        let own = format!("{a}-2");
        block_state_delete(&manager, &own);
        let Err(e @ Error::VmStateLeft { .. }) = manager.set_session_vm(&a, false, None).await else {
            panic!("a failed delete is an error");
        };
        let path = manager.state_dir(&own).display().to_string();
        assert_eq!(
            e.message(),
            format!(
                "Could not delete the agent state of VM {own} at {path} (Not a directory (os error 20)); delete it by hand."
            )
        );
        assert_eq!(vm_key(&manager, &a), Some(a.clone()));
        assert!(matches!(manager.set_session_vm(&a, true, None).await, Err(Error::VmStateLeft { .. })));
        assert_eq!(vm_key(&manager, &a), Some(a.clone()));
    }

    #[tokio::test]
    async fn a_failed_state_delete_of_a_deleted_session_is_an_error() {
        let (manager, _, a) = manager();
        manager.set_session_vm(&a, true, None).await.unwrap();
        block_state_delete(&manager, &format!("{a}-2"));
        assert!(matches!(manager.delete(&a).await, Err(Error::VmStateLeft { .. })));
        assert!(manager.list().sessions.is_empty());
    }

    #[tokio::test]
    async fn a_cage_delete_removes_the_state_of_own_vms_of_moved_sessions() {
        let (manager, _, a) = manager();
        manager.set_session_vm(&a, true, None).await.unwrap();
        let own = format!("{a}-2");
        manager.start(&a).await.unwrap();
        put_codex_login(&manager, &own);
        manager.stop(&a, None).await.unwrap();
        let other = manager.create_cage("other").unwrap();
        manager.set_session_cage(&a, Some(&other)).unwrap();
        manager.delete_cage(&a).unwrap();
        for dir in [manager.state_dir(&own), manager.bin_dir(&own), manager.profile_dir(&own)] {
            assert!(!dir.exists(), "{} is left", dir.display());
        }
        assert_eq!(lock(&manager.state).session(&a).unwrap().vm, VmChoice::Shared);
    }

    #[test]
    fn failed_delete_is_an_error_but_does_not_stop_the_load() {
        let dir = temp_dir();
        let store = || Store::new(dir.clone());
        let vault = vault();
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v")).unwrap();
        vault.set(&Owner::cage(&id), "GONE", &SecretValue::new("x")).unwrap();
        *vault.writes_left.lock().unwrap() = Some(0);

        assert!(matches!(
            manager.set_rule_credential(&id, &rule, "KEY", None, Source::Local, None),
            Err(Error::Vault(_))
        ));
        let reloaded = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        assert_eq!(reloaded.list().sessions.len(), 1);
        assert_eq!(stored(&vault, &id, "GONE").as_deref(), Some("x"));
    }
}
