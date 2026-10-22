use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::Poll;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::agent::{AgentKind, gemini, guest_login, launch, pi};
use crate::error::{Error, Result};
use crate::host_agents::login::{self, HostToken};
use crate::host_agents::{self, HostAgents, profile};
use crate::netlog::{self, Dedupe};
use crate::network::{self, Action, Credential, DefaultAction, Inject, NetworkPolicy, RuleDraft, Target};
use crate::proxy::ca::{self, Ca};
use crate::proxy::inject::Login;
use crate::proxy::log::Entry;
use crate::proxy::{self, Live, Proxy};
use crate::runtime::{Bind, GuestNet, PipeEvent, PipeInput, PtyEvent, PtyInput, SandboxRuntime, SessionKind};
use crate::runtimes;
use crate::secrets::{self, SecretValue, Vault};
use crate::shield::{self, AppDraft, HostAgentUse, Pos, Shield, TaskDraft, WORKSPACE};
use crate::store::Store;
use crate::task_agents::Exec;
use crate::task_artifacts::{self, Mirror, RoundWriter};
use crate::task_runs::{self, Control, RunHandle, RunState};

const NODE_LIMIT: Duration = Duration::from_secs(15);
/// The bytes of `node --version` output that are kept.
const NODE_OUT: usize = 1 << 10;
/// How long a stopped app has to exit before its restart fails.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);
/// The network log entries kept per shield.
const NET_RING: usize = 2000;
/// The output bytes kept per session, for a view that attaches later.
const RING: usize = 1 << 20;
/// A frame takes the queued output up to this many bytes, or for at most
/// [`FRAME_WAIT`].
const FRAME: usize = 64 << 10;
const FRAME_WAIT: Duration = Duration::from_millis(8);
/// A first chunk up to this size with nothing queued after it is sent at
/// once, so the echo of a key has no delay.
const ECHO: usize = 1 << 10;
/// The frames that a view may have not acknowledged. Then the session stops
/// reading its process until the view acknowledges one.
const WINDOW: usize = 8;
/// A view that acknowledges no frame for this long is gone, such as after a
/// reload, and is detached.
const ACK_LIMIT: Duration = Duration::from_secs(5);
/// The terminal size of an app until a view attaches and resizes it.
const APP_SIZE: (u16, u16) = (24, 80);
/// How long a shield stop waits for its task runs to write their manifests.
const RUN_STOP_LIMIT: Duration = Duration::from_secs(15);
/// How long the check that a host package loads in the VM may run.
const PROBE_LIMIT: Duration = Duration::from_secs(15);
/// The host pi's TUI that still runs after this long has loaded.
const TUI_PROBE: Duration = Duration::from_secs(4);
/// The bytes of a probe's stderr that a notice shows.
const PROBE_ERR: usize = 512;
/// How long the network test may run in the guest.
const NET_TEST_LIMIT: Duration = Duration::from_secs(25);
/// The bytes of the network test output that are read.
const NET_TEST_OUT: usize = 8 << 10;
/// The shields that `stop_all` stops at once.
const STOP_PARALLEL: usize = 4;
/// How often a running shield looks for agent logins that were made in the guest.
const GUEST_LOGIN_POLL: Duration = Duration::from_secs(10);

/// Gets each new network log entry with its shield id.
pub type NetSink = Box<dyn Fn(&str, &Entry) + Send + Sync>;
/// Gets the id of a shield whose guest logins changed.
pub type GuestSink = Box<dyn Fn(&str) + Send + Sync>;

/// Owns the shield configs, the running VMs and their terminal sessions.
pub struct Manager<R: SandboxRuntime> {
    runtime: R,
    store: Store,
    /// The app data dir. It holds the agent runtimes, `shields/<id>`, the
    /// agent state of each shield, and `bin/<id>`, the host-only agent
    /// wrappers of each shield.
    data: PathBuf,
    vault: Arc<dyn Vault>,
    /// The zwinger CA, loaded at the first start.
    ca: tokio::sync::OnceCell<Arc<Ca>>,
    /// Serializes the secret edits, so that a rollback cannot undo a later edit.
    /// The state lock is not held during vault calls.
    secret_edit: Mutex<()>,
    /// Per account, the value that the saved config expects (`None`: no
    /// value) after a restore that did not end. Until the vault is seen to
    /// hold it, starts and edits that use the account fail.
    unsettled: Mutex<HashMap<String, Option<SecretValue>>>,
    state: Arc<Mutex<State<R::Vm>>>,
    net_sink: Arc<OnceLock<NetSink>>,
    runs: Arc<task_runs::Registry>,
    /// Serializes run starts, so two starts cannot take the same run name.
    run_start: Mutex<()>,
    /// The stage limits of new task runs.
    run_config: task_runs::Config,
    host: Arc<HostAgents>,
    guest_sink: Arc<OnceLock<GuestSink>>,
    app_notices: Mutex<Vec<String>>,
}

struct State<V> {
    shields: Vec<Shield>,
    vms: HashMap<String, Vm<V>>,
    sessions: Vec<Session>,
    /// Per shield that starts or runs, the policy and the credential values
    /// of its proxy. An edit changes them at once.
    live: HashMap<String, Arc<Live>>,
    /// Per shield, the last network log entries. They stay after a stop, so
    /// the user can still allow a blocked host.
    net: HashMap<String, VecDeque<Entry>>,
    /// Per shield and app, counts the app stops. A start that saw an older
    /// count was stopped while its process opened.
    app_stops: HashMap<(String, String), u64>,
}

enum Vm<V> {
    Starting,
    /// `applied` is the config the VM booted with; it differs from the stored
    /// config after an edit, until the next start. `proxy` is the shield's
    /// proxy; a drop stops it. `net` follows the VM's network events. It is
    /// `None` if the runtime log could not be opened. `runtimes` are the
    /// agent runtimes that the start mounted.
    Running {
        vm: Arc<V>,
        applied: Box<Shield>,
        proxy: Arc<Proxy>,
        net: Option<AbortHandle>,
        runtimes: Vec<&'static str>,
        host: Box<HostRun>,
    },
    /// Shutdown is in progress. If it fails, the VM goes back to `Running`.
    Stopping,
    Failed(String),
}

/// What a start did for the shield's host agents.
#[derive(Default)]
struct HostRun {
    /// pi runs from the host install; else the bundled pi runs.
    pi_host: bool,
    /// The guest path of Gemini CLI's entry point, if it runs in the VM.
    gemini: Option<String>,
    /// The URL of the shield's proxy.
    proxy: Option<String>,
    /// What the user should know, such as a fallback.
    notices: Vec<String>,
    /// The host agents that do not run in the VM, with the reason.
    failed: Vec<(AgentKind, String)>,
    /// The agents with a login that was made in the guest.
    guest_logins: Vec<AgentKind>,
    /// Looks for guest logins while the VM runs. A stop aborts it.
    guest_watch: Option<AbortHandle>,
}

impl HostRun {
    fn found(&self) -> launch::Found<'_> {
        launch::Found { pi_host: self.pi_host, gemini: self.gemini.as_deref(), proxy: self.proxy.as_deref() }
    }
}

/// The backend owns each session, not a view: a view attaches to it and
/// detaches.
struct Session {
    id: String,
    shield_id: String,
    kind: SessionKind,
    input: UnboundedSender<PtyInput>,
    exit_code: Option<i32>,
    output: Arc<Mutex<Output>>,
    /// Set for a shell or agent terminal.
    terminal_id: Option<String>,
    /// Set for an app run.
    app: Option<AppRun>,
}

struct AppRun {
    app_id: String,
    /// Becomes `true` when the process has exited.
    exited: watch::Receiver<bool>,
}

/// The kept output of a session and the view that gets new output.
#[derive(Default)]
struct Output {
    ring: VecDeque<u8>,
    exit: Option<i32>,
    sink: Option<Sink>,
    attaches: u64,
    /// Wakes the pump when a view acknowledges a frame or goes.
    room: Arc<Notify>,
}

/// The view that gets new output.
struct Sink {
    /// The token of its attach.
    token: u64,
    send: PtySink,
    /// The output frames that the view has not acknowledged.
    in_flight: usize,
}

type PtySink = Box<dyn Fn(PtyEvent) + Send>;

impl Output {
    fn push(&mut self, event: PtyEvent) {
        match &event {
            PtyEvent::Output(bytes) => {
                self.ring.extend(bytes);
                let excess = self.ring.len().saturating_sub(RING);
                self.ring.drain(..excess);
            }
            PtyEvent::Exit(code) => self.exit = Some(*code),
        }
        if let Some(sink) = &mut self.sink {
            if matches!(event, PtyEvent::Output(_)) {
                sink.in_flight += 1;
            }
            (sink.send)(event);
        }
    }

    /// The view has [`WINDOW`] frames that it has not acknowledged.
    fn full(&self) -> bool {
        self.sink.as_ref().is_some_and(|s| s.in_flight >= WINDOW)
    }

    /// Sends the kept output to `send`, and the exit if there is one; then
    /// new output. Takes over from the earlier view. Returns the token.
    fn attach(&mut self, send: PtySink) -> u64 {
        self.attaches += 1;
        let token = self.attaches;
        let mut sink = Sink { token, send, in_flight: 0 };
        // One frame, so the replay takes one slot of the ack window. The ring
        // is bounded, so the frame is too.
        let kept: Vec<u8> = self.ring.iter().copied().collect();
        if !kept.is_empty() {
            sink.in_flight += 1;
            (sink.send)(PtyEvent::Output(kept));
        }
        if let Some(code) = self.exit {
            (sink.send)(PtyEvent::Exit(code));
        }
        self.sink = Some(sink);
        self.room.notify_one();
        token
    }

    /// A stale token changes nothing.
    fn detach(&mut self, token: u64) {
        if self.sink.as_ref().is_some_and(|s| s.token == token) {
            self.sink = None;
            self.room.notify_one();
        }
    }

    /// A stale token changes nothing.
    fn ack(&mut self, token: u64) {
        if let Some(sink) = self.sink.as_mut().filter(|s| s.token == token) {
            sink.in_flight = sink.in_flight.saturating_sub(1);
            self.room.notify_one();
        }
    }
}

/// The PTY events of a session, read frame by frame.
struct Frames {
    events: Receiver<PtyEvent>,
    /// The output after the end of the last frame.
    carry: Vec<u8>,
}

impl Frames {
    fn new(events: Receiver<PtyEvent>) -> Self {
        Self { events, carry: Vec::new() }
    }

    /// Reads the next frame of at most [`FRAME`] bytes: the carried output
    /// or the output of one event, and the output queued after it up to
    /// [`FRAME_WAIT`]. A small first chunk with nothing queued after it is the
    /// frame alone. The output after [`FRAME`] bytes goes to the next frame.
    /// The exit ends a frame. `None`: the events have ended.
    async fn next(&mut self) -> Option<(Vec<u8>, Option<i32>)> {
        let mut bytes = if self.carry.is_empty() {
            match self.events.recv().await? {
                PtyEvent::Output(bytes) => bytes,
                PtyEvent::Exit(code) => return Some((Vec::new(), Some(code))),
            }
        } else {
            std::mem::take(&mut self.carry)
        };
        let deadline = Instant::now() + FRAME_WAIT;
        let mut alone = true;
        let mut exit = None;
        while bytes.len() < FRAME {
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) if alone && bytes.len() <= ECHO => break,
                Err(TryRecvError::Empty) => match tokio::time::timeout_at(deadline, self.events.recv()).await {
                    Ok(Some(event)) => event,
                    Ok(None) | Err(_) => break,
                },
            };
            alone = false;
            match event {
                PtyEvent::Output(more) => bytes.extend(more),
                PtyEvent::Exit(code) => {
                    exit = Some(code);
                    break;
                }
            }
        }
        if bytes.len() > FRAME {
            self.carry = bytes.split_off(FRAME);
        }
        Some((bytes, exit))
    }
}

/// Moves the PTY output into the session's output frame by frame, until the
/// process exits. While the view is full, it does not read, so the process
/// blocks on its writes. The exit code is recorded before the `Exit` event
/// is delivered, so a listing done in reaction to it shows the exit.
async fn pump<V>(
    state: Arc<Mutex<State<V>>>,
    session_id: String,
    output: Arc<Mutex<Output>>,
    events: Receiver<PtyEvent>,
) {
    let mut frames = Frames::new(events);
    let room = lock(&output).room.clone();
    loop {
        loop {
            let notified = room.notified();
            if !lock(&output).full() {
                break;
            }
            if tokio::time::timeout(ACK_LIMIT, notified).await.is_err() {
                let mut output = lock(&output);
                if output.full() {
                    output.sink = None;
                }
            }
        }
        let Some((bytes, exit)) = frames.next().await else {
            return;
        };
        if let Some(code) = exit
            && let Some(s) = lock(&state).sessions.iter_mut().find(|s| s.id == session_id)
        {
            s.exit_code = Some(code);
        }
        let mut output = lock(&output);
        if !bytes.is_empty() {
            output.push(PtyEvent::Output(bytes));
        }
        if let Some(code) = exit {
            output.push(PtyEvent::Exit(code));
            return;
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShieldView {
    #[serde(flatten)]
    pub shield: Shield,
    pub status: Status,
    pub pending_restart: bool,
    pub sessions: Vec<SessionView>,
    /// The apps that run or have exited. An app with no entry is stopped.
    pub app_runs: Vec<AppRunView>,
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
    /// The hosts that the shield's agents need, with the proxy's decision.
    pub agent_needs: Vec<network::AgentNeed>,
}

/// One stage of the network test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetStage {
    pub ok: bool,
    pub detail: String,
}

/// The network test: the shield reaches its proxy, then the host through it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetTest {
    pub proxy: NetStage,
    /// `None` if the first stage failed.
    pub target: Option<NetStage>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "message", rename_all = "camelCase")]
pub enum Status {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error(String),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: String,
    pub kind: SessionKind,
    pub exit_code: Option<i32>,
    pub terminal_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRunView {
    pub app_id: String,
    pub session_id: String,
    pub exit_code: Option<i32>,
}

/// Deletes the values that no configured secret uses, such as the values of a
/// shield whose delete failed. A failure does not stop the app load.
fn prune_vault(vault: &dyn Vault, shields: &[Shield]) {
    let accounts = match vault.accounts() {
        Ok(accounts) => accounts,
        Err(e) => return eprintln!("secret prune: cannot list the vault: {e}"),
    };
    // A host login has no value in the vault, so its account is not kept.
    let mut configured: HashSet<String> = shields
        .iter()
        .flat_map(|shield| shield.network.credentials().into_iter().map(|c| secrets::account(&shield.id, &c.name)))
        .collect();
    configured.insert(secrets::account(ca::KEY_ENTRY.0, ca::KEY_ENTRY.1));
    for account in accounts.iter().filter(|a| !configured.contains(*a)) {
        let Some((shield_id, name)) = account.split_once('/') else {
            continue;
        };
        if vault.delete(shield_id, name).is_err() {
            eprintln!("secret prune: cannot delete {account}");
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<V> State<V> {
    fn shield_mut(&mut self, id: &str) -> Result<&mut Shield> {
        self.shields.iter_mut().find(|s| s.id == id).ok_or(Error::ShieldNotFound)
    }

    /// Numbers the entry and adds it to the shield's ring.
    fn push_net(&mut self, id: &str, mut event: Entry) -> Entry {
        let ring = self.net.entry(id.to_owned()).or_default();
        event.seq = ring.back().map_or(1, |last| last.seq + 1);
        if ring.len() == NET_RING {
            ring.pop_front();
        }
        ring.push_back(event.clone());
        event
    }

    /// The terminal has a session whose process runs.
    fn has_live_terminal(&self, terminal_id: &str) -> bool {
        self.sessions.iter().any(|s| s.terminal_id.as_deref() == Some(terminal_id) && s.exit_code.is_none())
    }

    fn session(&self, id: &str) -> Result<&Session> {
        self.sessions.iter().find(|s| s.id == id).ok_or(Error::SessionNotFound)
    }
}

impl<R: SandboxRuntime> Manager<R> {
    pub fn new(runtime: R, store: Store, vault: Arc<dyn Vault>, data: PathBuf, host: Arc<HostAgents>) -> Result<Self> {
        let shields = store.load()?;
        prune_vault(vault.as_ref(), &shields);
        Ok(Self {
            runtime,
            store,
            data,
            vault,
            ca: tokio::sync::OnceCell::new(),
            secret_edit: Mutex::new(()),
            unsettled: Mutex::default(),
            state: Arc::new(Mutex::new(State {
                shields,
                vms: HashMap::new(),
                sessions: Vec::new(),
                live: HashMap::new(),
                net: HashMap::new(),
                app_stops: HashMap::new(),
            })),
            net_sink: Arc::new(OnceLock::new()),
            runs: Arc::default(),
            run_start: Mutex::new(()),
            run_config: task_runs::Config::default(),
            host,
            guest_sink: Arc::new(OnceLock::new()),
            app_notices: Mutex::default(),
        })
    }

    /// Only the first sink counts.
    pub fn set_net_sink(&self, sink: NetSink) {
        let _ = self.net_sink.set(sink);
    }

    /// Only the first sink counts.
    pub fn set_guest_sink(&self, sink: GuestSink) {
        let _ = self.guest_sink.set(sink);
    }

    pub fn list_net_events(&self, id: &str) -> Result<Vec<Entry>> {
        let mut state = lock(&self.state);
        state.shield_mut(id)?;
        Ok(state.net.get(id).map(|ring| ring.iter().cloned().collect()).unwrap_or_default())
    }

    pub fn list(&self) -> Vec<ShieldView> {
        let state = lock(&self.state);
        state
            .shields
            .iter()
            .map(|shield| {
                let (status, pending_restart) = match state.vms.get(&shield.id) {
                    None => (Status::Stopped, false),
                    Some(Vm::Starting) => (Status::Starting, false),
                    // A new name, layout, app list, terminal list or network
                    // rule is used at once, so it needs no restart. A new
                    // credential name needs its placeholder in the guest env.
                    Some(Vm::Running { applied, .. }) => (
                        Status::Running,
                        Shield {
                            name: shield.name.clone(),
                            layout: shield.layout.clone(),
                            apps: shield.apps.clone(),
                            tasks: shield.tasks.clone(),
                            terminals: shield.terminals.clone(),
                            agents_unprompted: shield.agents_unprompted,
                            network: shield.network.clone(),
                            ..(**applied).clone()
                        } != *shield
                            || placeholders(&applied.network) != placeholders(&shield.network),
                    ),
                    Some(Vm::Stopping) => (Status::Stopping, false),
                    Some(Vm::Failed(message)) => (Status::Error(message.clone()), false),
                };
                let sessions = state.sessions.iter().filter(|s| s.shield_id == shield.id);
                let (runtimes, login_expires_at, host_notices, guest_logins) = match state.vms.get(&shield.id) {
                    Some(Vm::Running { runtimes, host, proxy, .. }) => {
                        (runtimes.clone(), proxy.login_expires(), host.notices.clone(), host.guest_logins.clone())
                    }
                    _ => (Vec::new(), None, Vec::new(), Vec::new()),
                };
                ShieldView {
                    agent_needs: network::agent_needs(shield),
                    shield: shield.clone(),
                    status,
                    pending_restart,
                    sessions: sessions
                        .clone()
                        .filter(|s| s.app.is_none())
                        .map(|s| SessionView {
                            id: s.id.clone(),
                            kind: s.kind,
                            exit_code: s.exit_code,
                            terminal_id: s.terminal_id.clone(),
                        })
                        .collect(),
                    app_runs: sessions
                        .filter_map(|s| {
                            s.app.as_ref().map(|run| AppRunView {
                                app_id: run.app_id.clone(),
                                session_id: s.id.clone(),
                                exit_code: s.exit_code,
                            })
                        })
                        .collect(),
                    runtimes,
                    login_expires_at,
                    host_notices,
                    guest_logins,
                }
            })
            .collect()
    }

    /// The host-only dir of the shield's agent wrappers, bound read-only at
    /// [`launch::BIN_DIR`]. It is not in the state dir: the guest can write that.
    fn bin_dir(&self, id: &str) -> PathBuf {
        bin_dir(&self.data, id)
    }

    fn state_dir(&self, id: &str) -> PathBuf {
        self.data.join("shields").join(id)
    }

    /// The host-owned profile snapshots of the shield's host agents.
    fn profile_dir(&self, id: &str) -> PathBuf {
        self.data.join("profiles").join(id)
    }

    pub fn create(&self, folder: &str) -> Result<String> {
        let shield = Shield::new(folder)?;
        let id = shield.id.clone();
        let mut state = lock(&self.state);
        state.shields.push(shield);
        self.store.save(&state.shields)?;
        Ok(id)
    }

    /// `ids` must name each shield once.
    pub fn reorder_shields(&self, ids: &[String]) -> Result<()> {
        let mut state = lock(&self.state);
        let mut shields = state.shields.clone();
        shield::reorder(&mut shields, ids, |s| &s.id)?;
        self.store.save(&shields)?;
        state.shields = shields;
        Ok(())
    }

    pub fn set_layout(&self, id: &str, layout: Option<BTreeMap<String, Pos>>) -> Result<()> {
        self.edit(id, |s| {
            s.set_layout(layout);
            Ok(())
        })
    }

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

    pub fn reorder_terminals(&self, id: &str, terminal_ids: &[String]) -> Result<()> {
        self.edit(id, |s| s.reorder_terminals(terminal_ids))
    }

    /// Applies `edit` to the stored config and persists it. The change is kept
    /// only if the save succeeds. A running VM keeps its applied config until
    /// the next start.
    fn edit(&self, id: &str, edit: impl FnOnce(&mut Shield) -> Result<()>) -> Result<()> {
        let mut state = lock(&self.state);
        let mut shields = state.shields.clone();
        let shield = shields.iter_mut().find(|s| s.id == id).ok_or(Error::ShieldNotFound)?;
        edit(shield)?;
        shield.check()?;
        self.store.save(&shields)?;
        state.shields = shields;
        Ok(())
    }

    /// The user confirms that the folders with no identity, such as a folder
    /// that was missing at the first load after the update, are theirs.
    pub fn confirm_paths(&self, id: &str) -> Result<()> {
        self.edit(id, |s| {
            s.fill_ids();
            Ok(())
        })
    }

    pub fn add_mount(&self, id: &str, host: &str, guest: &str, readonly: bool) -> Result<()> {
        self.edit(id, |s| s.add_mount(host, guest, readonly))
    }

    pub fn remove_mount(&self, id: &str, guest: &str) -> Result<()> {
        self.edit(id, |s| s.remove_mount(guest))
    }

    pub fn set_mount_readonly(&self, id: &str, guest: &str, readonly: bool) -> Result<()> {
        self.edit(id, |s| {
            s.mount_mut(guest)?.readonly = readonly;
            Ok(())
        })
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<()> {
        self.edit(id, |s| s.rename(name))
    }

    pub fn set_harness(&self, id: &str, harness: Option<&str>) -> Result<()> {
        self.edit(id, |s| s.set_harness(harness))
    }

    pub fn list_folders(&self, id: &str) -> Result<Vec<String>> {
        let folder = lock(&self.state).shield_mut(id)?.folder.clone();
        shield::list_folders(&folder)
    }

    /// Applies `edit` to the network policy and saves it. A running proxy
    /// uses the new policy from its next request.
    fn edit_network<T>(&self, id: &str, edit: impl FnOnce(&mut NetworkPolicy) -> Result<T>) -> Result<T> {
        let mut out = None;
        self.edit(id, |s| {
            out = Some(edit(&mut s.network)?);
            Ok(())
        })?;
        self.publish(id, None);
        Ok(out.expect("the edit ran"))
    }

    /// Sends the stored policy to the shield's proxy, if it has one, in one
    /// step with the new `value` of a credential.
    fn publish(&self, id: &str, value: Option<(&str, SecretValue)>) {
        let state = lock(&self.state);
        if let Some(live) = state.live.get(id)
            && let Some(shield) = state.shields.iter().find(|s| s.id == id)
        {
            live.edit(|policy, values| {
                *policy = shield.network.clone();
                if let Some((name, value)) = value {
                    values.insert(name.to_owned(), value);
                }
            });
        }
    }

    pub fn set_network_default(&self, id: &str, default: DefaultAction) -> Result<()> {
        self.edit_network(id, |n| {
            n.default = default;
            Ok(())
        })
    }

    pub fn set_offline(&self, id: &str, offline: bool) -> Result<()> {
        self.edit_network(id, |n| {
            n.offline = offline;
            Ok(())
        })
    }

    /// Adds or changes a rule. Returns its id.
    pub fn put_rule(&self, id: &str, draft: RuleDraft) -> Result<String> {
        self.edit_network(id, |n| n.put_rule(draft))
    }

    /// Saves the config first, then deletes the values that no other rule
    /// uses, so that a failure leaves a value with no credential.
    pub fn remove_rule(&self, id: &str, rule_id: &str) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        let removed = self.edit_network(id, |n| n.remove_rule(rule_id))?;
        self.drop_unused(id, removed.credentials.iter().map(|c| c.name.as_str()))
    }

    /// Deletes the values of `names` that no credential of the shield uses.
    fn drop_unused<'a>(&self, id: &str, names: impl Iterator<Item = &'a str>) -> Result<()> {
        let (used, live) = {
            let mut state = lock(&self.state);
            let used: Vec<String> =
                state.shield_mut(id)?.network.credentials().into_iter().map(|c| c.name.clone()).collect();
            (used, state.live.get(id).cloned())
        };
        let mut result = Ok(());
        for name in names.filter(|n| !used.iter().any(|u| u == n)) {
            if let Some(live) = &live {
                live.set_value(name, None);
            }
            match self.vault.delete(id, name) {
                Ok(()) => {
                    lock(&self.unsettled).remove(&secrets::account(id, name));
                }
                Err(e) => result = Err(e),
            }
        }
        result
    }

    /// Adds, changes or (with `inject` `None`) removes the credential `name`
    /// of the rule. A new name needs a value; `None` keeps the current value.
    /// The vault is written before the config, and a failed config save
    /// restores the vault. A running proxy uses the value from its next request.
    pub fn set_rule_credential(
        &self,
        id: &str,
        rule_id: &str,
        name: &str,
        inject: Option<Inject>,
        value: Option<SecretValue>,
    ) -> Result<()> {
        let Some(inject) = inject else {
            let _edit = lock(&self.secret_edit);
            if self.vault.is_pending(id, name) {
                return Err(Error::SecretPending(name.to_owned()));
            }
            self.edit_network(id, |n| {
                let rule = n.rule_mut(rule_id)?;
                let before = rule.credentials.len();
                rule.credentials.retain(|c| c.name != name);
                if rule.credentials.len() == before {
                    return Err(Error::SecretNotFound(name.to_owned()));
                }
                Ok(())
            })?;
            return self.drop_unused(id, std::iter::once(name));
        };
        let credential = Credential { name: name.to_owned(), inject }.normalize()?;
        let name = credential.name.clone();
        if value.as_ref().is_some_and(|v| v.expose().is_empty()) {
            return Err(Error::SecretValueRequired(name));
        }
        let put = |n: &mut NetworkPolicy| -> Result<()> {
            let rule = n.rule_mut(rule_id)?;
            match rule.credentials.iter_mut().find(|c| c.name == credential.name) {
                Some(current) => *current = credential.clone(),
                None => rule.credentials.push(credential.clone()),
            }
            Ok(())
        };
        let _edit = lock(&self.secret_edit);
        self.settle(id, &name)?;
        let is_new = {
            let state = lock(&self.state);
            let mut shield = state.shields.iter().find(|s| s.id == id).ok_or(Error::ShieldNotFound)?.clone();
            let is_new = !shield.network.credentials().iter().any(|c| c.name == name);
            put(&mut shield.network)?;
            shield.check()?;
            is_new
        };
        let Some(value) = value else {
            if is_new {
                return Err(Error::SecretValueRequired(name));
            }
            return self.edit_network(id, put);
        };

        let prior = self.vault.get(id, &name)?;
        if let Err(e) = self.vault.set(id, &name, &value) {
            // The late write still runs; the restore runs after it.
            if matches!(e, Error::KeychainTimeout) {
                self.restore_value(id, &name, prior.as_ref());
            }
            return Err(e);
        }
        if let Err(e) = self.edit(id, |s| put(&mut s.network)) {
            self.restore_value(id, &name, prior.as_ref());
            return Err(e);
        }
        // A request must not see the new rule with the old value.
        self.publish(id, Some((&name, value)));
        Ok(())
    }

    /// Tests the network from inside the running shield: first a TCP connect
    /// to the proxy, then a request to `host` through it.
    pub async fn test_network(&self, id: &str, host: &str) -> Result<NetTest> {
        let host = shield::domain(host)?;
        if host.starts_with("*.") {
            return Err(Error::InvalidDomain(host, "test one host, not a wildcard".into()));
        }
        let vm = match lock(&self.state).vms.get(id) {
            Some(Vm::Running { vm, .. }) => vm.clone(),
            _ => return Err(Error::NotRunning),
        };
        let argv = ["node", "-e", NET_TEST, host.as_str()].map(str::to_owned);
        let mut pipe = self.runtime.open_pipe(&vm, &argv, WORKSPACE).await?;
        let mut out = Vec::new();
        let read = async {
            while let Some(event) = pipe.events.recv().await {
                match event {
                    PipeEvent::Stdout(bytes) => {
                        let room = NET_TEST_OUT.saturating_sub(out.len());
                        out.extend(bytes.into_iter().take(room));
                    }
                    PipeEvent::Stderr(_) => {}
                    PipeEvent::Exit(_) => break,
                }
            }
        };
        if tokio::time::timeout(NET_TEST_LIMIT, read).await.is_err() {
            let _ = pipe.input.try_send(PipeInput::Kill);
        }
        Ok(net_test(&String::from_utf8_lossy(&out)))
    }

    /// Puts the prior value back after a failed edit. The old hosts must
    /// never get the new value: if the old value cannot come back, the value
    /// is removed, and the secret is then missing. If neither ends, the
    /// account stays unsettled until [`Self::settle`] sees the prior value.
    fn restore_value(&self, id: &str, name: &str, prior: Option<&SecretValue>) {
        let restored = match prior {
            Some(prior) => self.vault.set(id, name, prior),
            None => self.vault.delete(id, name),
        };
        let restored = match restored {
            // A restore past its deadline still runs, after the write.
            Err(e) if !matches!(e, Error::KeychainTimeout) => self.vault.delete(id, name),
            other => other,
        };
        if restored.is_err() {
            lock(&self.unsettled).insert(secrets::account(id, name), prior.cloned());
        }
    }

    /// Fails while a write of the account still runs, or while the vault may
    /// hold a value that the saved config does not expect. An unsettled
    /// account is checked, and restored again, here.
    fn settle(&self, id: &str, name: &str) -> Result<()> {
        if self.vault.is_pending(id, name) {
            return Err(Error::SecretPending(name.to_owned()));
        }
        let account = secrets::account(id, name);
        let Some(prior) = lock(&self.unsettled).get(&account).cloned() else {
            return Ok(());
        };
        let held = self.vault.get(id, name).map_err(|_| Error::SecretPending(name.to_owned()))?;
        let matches = held.as_ref().map(SecretValue::expose) == prior.as_ref().map(SecretValue::expose);
        let restored = matches
            || match &prior {
                Some(prior) => self.vault.set(id, name, prior),
                None => self.vault.delete(id, name),
            }
            .or_else(|_| self.vault.delete(id, name))
            .is_ok();
        if !restored {
            return Err(Error::SecretPending(name.to_owned()));
        }
        lock(&self.unsettled).remove(&account);
        Ok(())
    }

    /// Adds the host agent to the shield, or changes it: its runtime, and
    /// its profile and host login if `used` asks for them. A login adds the
    /// agent's host-login secret; `allow_hosts` also adds an Allow rule for
    /// its provider host, which the user confirmed. Applies on the next start.
    pub async fn put_host_agent(&self, id: &str, used: HostAgentUse, allow_hosts: bool) -> Result<()> {
        let kind = used.kind;
        let host = self.host.clone();
        if blocking(move || Ok(host.last().agent(kind).is_none())).await? {
            return Err(Error::HostAgentMissing(kind.label().to_owned()));
        }
        let login = match (used.login, login::api_host(kind)) {
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
        self.edit(id, |s| {
            if let Some(secret) = &login
                && allow_hosts
            {
                for host in &secret.hosts {
                    if !s.network.rules.iter().any(|r| r.action == Action::Allow && r.matches(host)) {
                        let target = Target::Host { host: host.to_string() };
                        s.network.put_rule(RuleDraft { id: None, target, action: Action::Allow })?;
                    }
                }
            }
            s.put_host_agent(used, login);
            Ok(())
        })?;
        drop(_edit);
        self.publish(id, None);
        Ok(())
    }

    /// Also removes its host-login secret. Applies on the next start.
    pub fn remove_host_agent(&self, id: &str, kind: AgentKind) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        self.edit(id, |s| {
            s.remove_host_agent(kind);
            Ok(())
        })
    }

    /// Needs no restart: a running VM gets new wrappers at once.
    pub fn set_agents_unprompted(&self, id: &str, on: bool) -> Result<()> {
        self.edit(id, |s| {
            s.agents_unprompted = on;
            Ok(())
        })?;
        // The stored value, not `on`: a later edit may have won the race to the lock.
        let mut state = lock(&self.state);
        let unprompted = state.shield_mut(id)?.agents_unprompted;
        if let Some(Vm::Running { runtimes, host, .. }) = state.vms.get(id) {
            launch::write(&self.bin_dir(id), &mounted(runtimes), host.found(), unprompted)?;
        }
        Ok(())
    }

    /// Saves the config first, then deletes the secret values, the agent
    /// state, the wrappers and the old chats. The first error is returned after all deletes ran.
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.stop(id).await.or_else(|e| match e {
            Error::NotRunning => Ok(()),
            e => Err(e),
        })?;
        let _edit = lock(&self.secret_edit);
        let names = {
            let mut state = lock(&self.state);
            let names: Vec<String> =
                state.shield_mut(id)?.network.credentials().into_iter().map(|c| c.name.clone()).collect();
            // A start can claim the shield between the stop above and this lock.
            match state.vms.get(id) {
                Some(Vm::Starting) => return Err(Error::Starting),
                Some(Vm::Running { .. }) => return Err(Error::AlreadyStarted),
                Some(Vm::Stopping) => return Err(Error::Stopping),
                None | Some(Vm::Failed(_)) => {}
            }
            let shields: Vec<Shield> = state.shields.iter().filter(|s| s.id != id).cloned().collect();
            self.store.save(&shields)?;
            state.shields = shields;
            state.vms.remove(id);
            state.live.remove(id);
            state.net.remove(id);
            names
        };
        let mut result = Ok(());
        for name in &names {
            if let Err(e) = self.vault.delete(id, name)
                && result.is_ok()
            {
                result = Err(e);
            }
        }
        self.runs.forget(id, None);
        let dirs = [
            self.state_dir(id),
            self.bin_dir(id),
            self.data.join("chats").join(id),
            self.data.join("tasks").join(id),
            self.profile_dir(id),
        ];
        for dir in dirs {
            if let Err(e) = std::fs::remove_dir_all(dir)
                && e.kind() != std::io::ErrorKind::NotFound
                && result.is_ok()
            {
                result = Err(e.into());
            }
        }
        result
    }

    /// The config snapshot, the vault reads and the registration of the
    /// proxy's live values hold `secret_edit`, so a credential edit during
    /// the start reaches the proxy. The lock is released before the host
    /// logins are read (the Keychain can wait for the macOS prompt) and
    /// before the runtime start. The proxy starts before the VM: if it
    /// cannot start, the shield does not start.
    pub async fn start(&self, id: &str) -> Result<()> {
        let (shield, live) = {
            let _edit = lock(&self.secret_edit);
            let shield = self.claim_start(id)?;
            match self.vault_values(&shield) {
                Ok(values) => {
                    let live = Live::new(shield.network.clone(), values);
                    lock(&self.state).live.insert(id.to_owned(), live.clone());
                    (shield, live)
                }
                Err(e) => {
                    lock(&self.state).vms.remove(id);
                    return Err(e);
                }
            }
        };
        let fail = |e: Error| {
            let mut state = lock(&self.state);
            state.vms.remove(id);
            state.live.remove(id);
            Err(e)
        };
        let prepared = {
            let (host, data, shield) = (self.host.clone(), self.data.clone(), shield.clone());
            blocking(move || prepare(&host, &data, &shield)).await
        };
        let Prepared { mut binds, wanted, mut run, tokens } = match prepared {
            Ok(prepared) => prepared,
            Err(e) => return fail(e),
        };
        let proxy = match self.start_proxy(id, &shield, live, tokens).await {
            Ok(proxy) => Arc::new(proxy),
            Err(e) => return fail(e),
        };
        binds.push(Bind { guest: launch::CA_DIR.to_owned(), host: Ca::dir(&self.data) });
        run.proxy = Some(proxy.url());
        let mut env = launch::net_env(&proxy.url());
        let names = shield.logins.iter().map(|l| l.name.clone());
        env.extend(names.chain(placeholders(&shield.network)).map(|n| (n.clone(), secrets::placeholder(&n))));
        let net = GuestNet { port: proxy.port, env };
        let installed = runtimes::installed(&self.data, &wanted);
        for kind in [AgentKind::Claude, AgentKind::Codex] {
            let Some(version) = wanted.get(kind.name()) else {
                continue;
            };
            let label = kind.label();
            let used = match installed.iter().find(|r| r.name == kind.name()) {
                Some(r) if r.version == *version => continue,
                Some(r) => format!(", so the shield uses {label} {}", r.version),
                None => String::new(),
            };
            run.notices.push(format!(
                "The Linux build of {label} {version} is not downloaded yet{used}. \
                 Download it in the Agents settings, then restart the shield."
            ));
        }
        // The paths again right before and after the boot, which binds them: a
        // folder that was swapped meanwhile is refused.
        let result = match shield.verify_paths() {
            Ok(()) => match self.runtime.start(&shield, &net, &self.state_dir(id), &installed, &binds).await {
                Ok(vm) => {
                    let checked = match shield.verify_paths() {
                        Ok(()) => self.check_node(&vm).await,
                        Err(e) => Err(e),
                    };
                    match checked {
                        Ok(()) => Ok(vm),
                        Err(e) => {
                            let _ = self.runtime.stop(&vm).await;
                            Err(e)
                        }
                    }
                }
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };

        let mut names: Vec<&'static str> = installed.iter().map(|r| r.name).collect();
        if let Ok(vm) = &result {
            if run.pi_host
                && let Err(e) = self.probe_tui(vm, run.proxy.as_deref()).await
            {
                run.pi_host = false;
                run.notices.push(format!(
                    "The pi of this Mac does not start in the shield ({e}), so the bundled pi runs. It can be a different version."
                ));
            }
            if let Some(entry) = run.gemini.clone() {
                let argv = ["node".to_owned(), entry, "--version".to_owned()];
                match self.probe(vm, &argv).await {
                    Ok(()) => names.push(AgentKind::Gemini.name()),
                    Err(e) => {
                        run.gemini = None;
                        run.failed.push((AgentKind::Gemini, e));
                    }
                }
            }
            for (kind, why) in &run.failed {
                run.notices.push(format!("{} from this Mac does not run in the shield: {why}", kind.label()));
            }
            // Before any session starts, so each login shell has the agent commands.
            if let Err(e) = self.probe(vm, &launch::profile_argv()).await {
                run.notices.push(format!(
                    "Login shells do not find the agent commands ({e}). Run them as {}/<agent>.",
                    launch::BIN_DIR
                ));
            }
            if let Err(e) = self.probe(vm, &launch::trust_argv()).await {
                run.notices.push(format!(
                    "Only Node trusts the network proxy of this shield, so curl, git and Python fail TLS: {e}."
                ));
            }
        }

        let net = match &result {
            Ok(vm) => match self.runtime.net_lines(vm).await {
                Ok(lines) => Some(self.follow_net(id, lines)),
                Err(e) => {
                    eprintln!("network activity of {id} is not available: {e}");
                    None
                }
            },
            Err(_) => None,
        };

        let autostart: Vec<String> = {
            let mut state = lock(&self.state);
            match result {
                Ok(vm) => {
                    // In the lock, so an edit of the setting cannot write between the read and the write.
                    let unprompted = state.shield_mut(id).is_ok_and(|s| s.agents_unprompted);
                    if let Err(e) = launch::write(&self.bin_dir(id), &mounted(&names), run.found(), unprompted) {
                        run.notices.push(format!("The agent commands could not be written: {e}"));
                    }
                    let vm = Arc::new(vm);
                    run.guest_watch = Some(self.watch_guest_logins(id, &vm));
                    state.vms.insert(
                        id.to_owned(),
                        Vm::Running { vm, applied: Box::new(shield), proxy, net, runtimes: names, host: Box::new(run) },
                    );
                    let apps = state.shield_mut(id).map(|s| s.apps.clone()).unwrap_or_default();
                    apps.into_iter().filter(|a| a.autostart).map(|a| a.id).collect()
                }
                Err(e) => {
                    state.vms.insert(id.to_owned(), Vm::Failed(e.message()));
                    state.live.remove(id);
                    return Err(e);
                }
            }
        };
        // An app that cannot start, or a stop that ran meanwhile, does not
        // fail the start. A bad command shows as an exit code.
        for app_id in autostart {
            if let Err(e) = self.start_app(id, &app_id).await {
                eprintln!("autostart of app {app_id} in {id} failed: {e}");
            }
        }
        Ok(())
    }

    /// The zwinger CA, loaded once. The Keychain read runs off the async workers.
    async fn ca(&self) -> Result<Arc<Ca>> {
        let ca = self
            .ca
            .get_or_try_init(|| {
                let (vault, dir) = (self.vault.clone(), Ca::dir(&self.data));
                blocking(move || Ok(Arc::new(Ca::load(vault.as_ref(), &dir)?)))
            })
            .await?;
        Ok(ca.clone())
    }

    /// Starts the shield's proxy. Its log entries go to the shield's ring and the sink.
    async fn start_proxy(&self, id: &str, shield: &Shield, live: Arc<Live>, tokens: Vec<HostToken>) -> Result<Proxy> {
        let ca = self.ca().await?;
        let logins = shield
            .logins
            .iter()
            .zip(tokens)
            .map(|(secret, token)| {
                Arc::new(Login::new(secret.name.clone(), secret.hosts.clone(), token, self.host.clone()))
            })
            .collect();
        let (state, sink, id) = (self.state.clone(), self.net_sink.clone(), id.to_owned());
        let config = proxy::Config {
            ca,
            live,
            logins,
            refresh: login::denied_hosts(&shield.logins),
            sink: Arc::new(move |entry| {
                let entry = lock(&state).push_net(&id, entry);
                if let Some(sink) = sink.get() {
                    sink(&id, &entry);
                }
            }),
            resolver: Arc::new(proxy::dial::System),
        };
        Proxy::start(config).await.map_err(|e| Error::Proxy(e.to_string()))
    }

    /// Looks for guest logins every `GUEST_LOGIN_POLL` while the VM runs, and
    /// calls the guest sink when they change.
    fn watch_guest_logins(&self, id: &str, vm: &Arc<R::Vm>) -> AbortHandle {
        let (state, sink, dir) = (self.state.clone(), self.guest_sink.clone(), self.state_dir(id));
        let (id, vm) = (id.to_owned(), vm.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(GUEST_LOGIN_POLL).await;
                let dir = dir.clone();
                let Ok(found) = tokio::task::spawn_blocking(move || guest_login::found(&dir)).await else {
                    continue;
                };
                let changed = {
                    let mut state = lock(&state);
                    let Some(Vm::Running { vm: running, host, .. }) = state.vms.get_mut(&id) else {
                        return;
                    };
                    if !Arc::ptr_eq(running, &vm) {
                        return;
                    }
                    found != std::mem::replace(&mut host.guest_logins, found.clone())
                };
                if changed && let Some(sink) = sink.get() {
                    sink(&id);
                }
            }
        })
        .abort_handle()
    }

    /// Deletes the agent's login that was made in the guest.
    pub fn remove_guest_login(&self, id: &str, kind: AgentKind) -> Result<()> {
        lock(&self.state).shield_mut(id)?;
        let dir = self.state_dir(id);
        guest_login::remove(&dir, kind)?;
        let found = guest_login::found(&dir);
        if let Some(Vm::Running { host, .. }) = lock(&self.state).vms.get_mut(id) {
            host.guest_logins = found;
        }
        Ok(())
    }

    /// Turns the runtime log lines into the shield's log: the direct
    /// connections that the VM blocked, because a tool ignores the proxy.
    fn follow_net(&self, id: &str, mut lines: Receiver<String>) -> AbortHandle {
        let state = self.state.clone();
        let sink = self.net_sink.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let mut dedupe = Dedupe::default();
            while let Some(line) = lines.recv().await {
                let Some(event) = netlog::parse(&line) else {
                    continue;
                };
                let Some(entry) = Entry::direct(&event).filter(|_| dedupe.keep(&event)) else {
                    continue;
                };
                let event = lock(&state).push_net(&id, entry);
                if let Some(sink) = sink.get() {
                    sink(&id, &event);
                }
            }
        })
        .abort_handle()
    }

    /// Checks the shield, marks it as starting and returns its config.
    fn claim_start(&self, id: &str) -> Result<Shield> {
        let mut state = lock(&self.state);
        let shield = state.shield_mut(id)?.clone();
        match state.vms.get(id) {
            Some(Vm::Starting) => return Err(Error::Starting),
            Some(Vm::Running { .. }) => return Err(Error::AlreadyStarted),
            Some(Vm::Stopping) => return Err(Error::Stopping),
            None | Some(Vm::Failed(_)) => {}
        }
        shield.verify_paths()?;
        shield.network.check()?;
        shield.check_app_data(&self.data)?;
        shield.check_overlap_with(state.shields.iter().filter(|s| s.id != id))?;
        state.vms.insert(id.to_owned(), Vm::Starting);
        Ok(shield)
    }

    /// pi needs a newer Node than some `node:22-slim` images have.
    async fn check_node(&self, vm: &R::Vm) -> Result<()> {
        let argv = ["node".to_owned(), "--version".to_owned()];
        let mut pipe = self.runtime.open_pipe(vm, &argv, WORKSPACE).await?;
        let mut out = Vec::new();
        let read = async {
            while let Some(event) = pipe.events.recv().await {
                match event {
                    PipeEvent::Stdout(bytes) => {
                        let room = NODE_OUT.saturating_sub(out.len());
                        out.extend(bytes.into_iter().take(room));
                    }
                    PipeEvent::Stderr(_) => {}
                    PipeEvent::Exit(_) => break,
                }
            }
        };
        if tokio::time::timeout(NODE_LIMIT, read).await.is_err() {
            let _ = pipe.input.try_send(PipeInput::Kill);
        }
        pi::check_node(&String::from_utf8_lossy(&out))
    }

    /// The vault value of each credential. A value with a write that still
    /// runs fails the start: the vault may not match the config yet.
    fn vault_values(&self, shield: &Shield) -> Result<HashMap<String, SecretValue>> {
        let mut values = HashMap::new();
        for credential in shield.network.credentials() {
            self.settle(&shield.id, &credential.name)?;
            match self.vault.get(&shield.id, &credential.name)? {
                Some(value) => values.insert(credential.name.clone(), value),
                None => return Err(Error::SecretMissing(credential.name.clone())),
            };
        }
        Ok(values)
    }

    /// Checks that `argv` runs in the VM and ends with code 0. The error is
    /// the end of its stderr.
    async fn probe(&self, vm: &R::Vm, argv: &[String]) -> std::result::Result<(), String> {
        let mut pipe = self.runtime.open_pipe(vm, argv, WORKSPACE).await.map_err(|e| e.to_string())?;
        let mut err = Vec::new();
        let read = async {
            while let Some(event) = pipe.events.recv().await {
                match event {
                    PipeEvent::Stderr(bytes) => err.extend(bytes),
                    PipeEvent::Stdout(_) => {}
                    PipeEvent::Exit(code) => return code,
                }
            }
            -1
        };
        match tokio::time::timeout(PROBE_LIMIT, read).await {
            Ok(0) => Ok(()),
            Ok(code) => Err(probe_error(&err, code)),
            Err(_) => {
                let _ = pipe.input.try_send(PipeInput::Kill);
                Err("it did not answer in time".to_owned())
            }
        }
    }

    /// Checks that the host pi starts its TUI in a terminal: it loads a
    /// native module that `--version` does not. A TUI that still runs after
    /// `TUI_PROBE` has loaded, and is killed. The error is the end of its output.
    async fn probe_tui(&self, vm: &R::Vm, proxy: Option<&str>) -> std::result::Result<(), String> {
        let found = launch::Found { pi_host: true, gemini: None, proxy };
        let mut argv = launch::command(AgentKind::Pi, found).expect("pi has a program");
        argv.push("--no-session".to_owned());
        let (rows, cols) = APP_SIZE;
        let mut pty = self.runtime.open_pty(vm, &argv, WORKSPACE, rows, cols).await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let read = async {
            while let Some(event) = pty.events.recv().await {
                match event {
                    PtyEvent::Output(bytes) => out.extend(bytes),
                    PtyEvent::Exit(code) => return code,
                }
            }
            -1
        };
        match tokio::time::timeout(TUI_PROBE, read).await {
            Ok(0) => Ok(()),
            Ok(code) => Err(probe_error(&out, code)),
            Err(_) => {
                let _ = pty.input.send(PtyInput::Kill);
                Ok(())
            }
        }
    }

    /// The VM stays tracked until the runtime confirms shutdown, so a failed
    /// stop leaves the shield running and the user can retry.
    pub async fn stop(&self, id: &str) -> Result<()> {
        // First, so each run settles its manifest before its processes die with the VM.
        self.runs.cancel_shield(id, "the shield stopped", RUN_STOP_LIMIT).await;
        let (vm, applied, proxy, net, runtimes, host) = {
            let mut state = lock(&self.state);
            state.shield_mut(id)?;
            match state.vms.get(id) {
                Some(Vm::Running { .. }) => {}
                Some(Vm::Starting) => return Err(Error::Starting),
                Some(Vm::Stopping) => return Err(Error::Stopping),
                Some(Vm::Failed(_)) | None => return Err(Error::NotRunning),
            }
            let Some(Vm::Running { vm, applied, proxy, net, runtimes, host }) =
                state.vms.insert(id.to_owned(), Vm::Stopping)
            else {
                unreachable!("checked above");
            };
            state.sessions.retain(|s| {
                if s.shield_id == id {
                    let _ = s.input.send(PtyInput::Kill);
                }
                s.shield_id != id
            });
            (vm, applied, proxy, net, runtimes, host)
        };

        let result = self.runtime.stop(&vm).await;

        let mut state = lock(&self.state);
        match result {
            // The proxy stops with the last reference to it.
            Ok(()) => {
                state.vms.remove(id);
                state.live.remove(id);
                for task in net.iter().chain(&host.guest_watch) {
                    task.abort();
                }
            }
            // The follower and the proxy stay with the VM that still runs.
            Err(_) => {
                state.vms.insert(id.to_owned(), Vm::Running { vm, applied, proxy, net, runtimes, host });
            }
        }
        result
    }

    /// Stops the running shields, `STOP_PARALLEL` at a time. `limit` bounds
    /// the total time, so app exit cannot hang on a stuck VM.
    pub async fn stop_all(&self, limit: Duration) {
        let running: Vec<String> = lock(&self.state)
            .vms
            .iter()
            .filter(|(_, vm)| matches!(vm, Vm::Running { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        let permits = Semaphore::new(STOP_PARALLEL);
        let mut stops: Vec<_> = running
            .iter()
            .map(|id| {
                Box::pin(async {
                    let _permit = permits.acquire().await;
                    let _ = self.stop(id).await;
                })
            })
            .collect();
        let all = std::future::poll_fn(|cx| {
            stops.retain_mut(|stop| stop.as_mut().poll(cx).is_pending());
            if stops.is_empty() { Poll::Ready(()) } else { Poll::Pending }
        });
        let _ = tokio::time::timeout(limit, all).await;
    }

    /// The shields with a VM that starts, runs or stops. A failed stop
    /// leaves the shield running.
    pub fn live(&self) -> usize {
        lock(&self.state).vms.values().filter(|vm| !matches!(vm, Vm::Failed(_))).count()
    }

    /// The app notices, such as the VMs that the app start removed. Each
    /// notice is returned once.
    pub fn take_app_notices(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.app_notices))
    }

    pub fn add_app_notice(&self, notice: String) {
        lock(&self.app_notices).push(notice);
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
    async fn open_terminal(
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
    fn add_terminal(&self, state: &mut State<R::Vm>, shield_id: &str, kind: SessionKind) -> Result<String> {
        let mut shields = state.shields.clone();
        let shield = shields.iter_mut().find(|s| s.id == shield_id).ok_or(Error::ShieldNotFound)?;
        let terminal_id = shield.add_terminal(kind);
        self.store.save(&shields)?;
        state.shields = shields;
        Ok(terminal_id)
    }

    /// Runs the app's saved command as `/bin/sh -lc` in a PTY that the
    /// backend owns, and returns its session id. A run that has exited is
    /// replaced. Fails if the VM is not running, or if it stopped or started
    /// again while the process opened.
    pub async fn start_app(&self, shield_id: &str, app_id: &str) -> Result<String> {
        let key = (shield_id.to_owned(), app_id.to_owned());
        let (vm, app, stops) = {
            let mut state = lock(&self.state);
            let app = state.shield_mut(shield_id)?.app(app_id)?.clone();
            let stops = state.app_stops.get(&key).copied().unwrap_or(0);
            if state
                .sessions
                .iter()
                .any(|s| s.app.as_ref().is_some_and(|r| r.app_id == app_id) && s.exit_code.is_none())
            {
                return Err(Error::AppRunning);
            }
            match state.vms.get(shield_id) {
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
                state.vms.get(shield_id),
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
            } else if state.shield_mut(shield_id).and_then(|s| s.app(app_id)).is_err() {
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
                shield_id: shield_id.to_owned(),
                kind: SessionKind::Shell,
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
    pub fn stop_app(&self, shield_id: &str, app_id: &str) -> Result<()> {
        self.kill_app(shield_id, app_id);
        Ok(())
    }

    /// Does the work of `stop_app` and returns the exit of the killed run.
    fn kill_app(&self, shield_id: &str, app_id: &str) -> Option<watch::Receiver<bool>> {
        let mut state = lock(&self.state);
        *state.app_stops.entry((shield_id.to_owned(), app_id.to_owned())).or_default() += 1;
        let mut exited = None;
        state.sessions.retain(|s| match &s.app {
            Some(run) if s.shield_id == shield_id && run.app_id == app_id => {
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
    pub async fn restart_app(&self, shield_id: &str, app_id: &str) -> Result<String> {
        if let Some(mut exited) = self.kill_app(shield_id, app_id) {
            // An error means that the run task ended, so the process is gone.
            let wait = exited.wait_for(|done| *done);
            if tokio::time::timeout(CLOSE_LIMIT, wait).await.is_err() {
                return Err(Error::AppStillRunning);
            }
        }
        self.start_app(shield_id, app_id).await
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

    fn output(&self, session_id: &str) -> Result<Arc<Mutex<Output>>> {
        Ok(lock(&self.state).session(session_id)?.output.clone())
    }

    pub fn write(&self, id: &str, data: Vec<u8>) -> Result<()> {
        self.send(id, PtyInput::Data(data))
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<()> {
        self.send(id, PtyInput::Resize { rows, cols })
    }

    fn send(&self, id: &str, input: PtyInput) -> Result<()> {
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
    fn unmounted(&self, kind: AgentKind, host: &HostRun) -> Error {
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
        let slug = lock(&self.state).shield_mut(id)?.task(task_id)?.slug.clone();
        self.edit(id, |s| s.remove_task(task_id))?;
        self.runs.forget(id, Some(task_id));
        match std::fs::remove_dir_all(self.task_dir(id, &slug)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// The host-only folder of a task's runs: the guest cannot write it.
    fn task_dir(&self, id: &str, slug: &str) -> PathBuf {
        self.data.join("tasks").join(id).join(slug)
    }

    /// The round folder of a run. Only an existing run of the task counts.
    pub fn round_dir(&self, id: &str, task_id: &str, run: &str) -> Result<PathBuf> {
        let slug = lock(&self.state).shield_mut(id)?.task(task_id)?.slug.clone();
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
            let task = state.shield_mut(id)?.task(task_id)?.clone();
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
            shield_id: id.to_owned(),
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

    /// The runs of all tasks of the shield, oldest first per task.
    pub async fn list_task_runs(&self, id: &str) -> Result<Vec<RunSummary>> {
        let tasks: Vec<(String, PathBuf)> = lock(&self.state)
            .shield_mut(id)?
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
            let task = state.shield_mut(id)?.task(task_id)?.clone();
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
fn guest_artifacts(harness: &str, slug: &str, run: &str) -> String {
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
fn run_names(dir: &std::path::Path) -> Vec<String> {
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

/// What a start needs for the shield's host agents.
struct Prepared {
    /// The token of each host login, in the order of the config.
    tokens: Vec<HostToken>,
    binds: Vec<Bind>,
    /// The host version of each native runtime that a host agent uses.
    wanted: HashMap<&'static str, String>,
    run: HostRun,
}

/// Copies the profiles and links them into the agents' state, reads the
/// host logins and writes the guest logins, and picks the host packages and
/// runtime versions. It reads the Keychain and copies files, so it runs on a
/// blocking thread. The state dir is guest-writable: each write there
/// follows no link.
fn prepare(host: &HostAgents, data: &Path, shield: &Shield) -> Result<Prepared> {
    let state = data.join("shields").join(&shield.id);
    std::fs::create_dir_all(&state)?;
    // Empty: the start writes the wrappers of the agents that run in the VM.
    let bin = bin_dir(data, &shield.id);
    if let Err(e) = std::fs::remove_dir_all(&bin)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e.into());
    }
    std::fs::create_dir_all(&bin)?;
    // Claude Code's HOME, CODEX_HOME and Gemini CLI's HOME. Codex needs its dir to exist.
    for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini] {
        host_agents::open_parts(&state, &[kind.name()])?;
    }
    let mut run = HostRun::default();

    let detection = host.last();
    let home = host.home();
    let mut binds = vec![Bind { guest: launch::BIN_DIR.to_owned(), host: bin }];
    let mut wanted = HashMap::new();
    for kind in host_agents::KINDS {
        let used = shield.host_agent(kind);
        let snapshot_dir = data.join("profiles").join(&shield.id).join(kind.name());
        let mut entries = Vec::new();
        if used.is_some_and(|u| u.profile) {
            let snapshot = profile::snapshot(kind, &home, &snapshot_dir)?;
            if !snapshot.skipped.is_empty() {
                run.notices.push(format!(
                    "The {} profile skipped {} entries: links out of their folder, denied names, or files over the size limit.",
                    kind.label(),
                    snapshot.skipped.len()
                ));
            }
            binds.push(Bind { guest: profile::guest_dir(kind), host: snapshot_dir });
            entries = snapshot.entries;
        } else if let Err(e) = std::fs::remove_dir_all(&snapshot_dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.into());
        }
        profile::link(kind, &state, &entries)?;
        if used.is_none() {
            continue;
        }
        let Some(found) = detection.agent(kind) else {
            match kind {
                AgentKind::Gemini => run.failed.push((kind, "it is not installed on this Mac".to_owned())),
                _ => run
                    .notices
                    .push(format!("{} is not installed on this Mac, so the shield uses its own copy.", kind.label())),
            }
            continue;
        };
        match (kind, found.package.clone(), found.entry.as_deref()) {
            (AgentKind::Claude | AgentKind::Codex, ..) => {
                wanted.insert(kind.name(), found.version.clone());
            }
            (AgentKind::Pi, Some(package), _) => {
                binds.push(Bind { guest: pi::HOST_DIR.to_owned(), host: package });
                run.pi_host = true;
            }
            (AgentKind::Gemini, Some(package), Some(entry)) => {
                binds.push(Bind { guest: gemini::DIR.to_owned(), host: package });
                run.gemini = Some(format!("{}/{entry}", gemini::DIR));
            }
            // Detection gives each JS agent its package, and Gemini CLI its entry point.
            (AgentKind::Pi | AgentKind::Gemini, ..) => {
                run.failed.push((kind, "detection found no package or entry point; refresh the detection".to_owned()));
            }
        }
    }

    // After the links, so no link of the profile replaces a login file.
    let now = host.now_ms();
    let mut tokens = Vec::new();
    for secret in &shield.logins {
        let crate::secrets::Origin::HostLogin { agent } = secret.origin else {
            continue;
        };
        let token = host.token(agent)?;
        if !secret.hosts.contains(&token.host) {
            return Err(Error::HostLogin(
                agent.name().to_owned(),
                format!("it now uses {}; add the agent to the shield again", token.host),
            ));
        }
        for file in login::guest_files(&token, &secret.placeholder(), now) {
            let dir = host_agents::open_parts(&state, file.dir)?;
            host_agents::write_at(&dir, file.name, &file.body)?;
        }
        if token.expires.is_none() {
            run.notices.push(format!(
                "The {} login of this Mac has no expiry that zwinger can read, so the proxy does not read it again. \
                 Restart the shield when the agent reports a login error.",
                agent.label()
            ));
        }
        tokens.push(token);
    }
    // After the host logins: a placeholder that replaced a guest login is not one.
    run.guest_logins = guest_login::found(&state);
    if login::denied_hosts(&shield.logins).contains(&login::GITHUB_API) {
        run.notices.push(format!(
            "The GitHub API ({}) is blocked in this shield: pi's Copilot login is refreshed there.",
            login::GITHUB_API
        ));
    }
    Ok(Prepared { tokens, binds, wanted, run })
}

/// The network test in the guest. It prints one JSON line per stage.
/// The target stage passes only on an answer of the host: a response of the
/// proxy itself has [`proxy::BLOCKED_HEADER`] or [`proxy::ERROR_HEADER`].
const NET_TEST: &str = r#"const net = require("net");
const host = process.argv[1];
const say = (stage, ok, detail) => console.log(JSON.stringify({ stage, ok, detail }));
let proxy;
try { proxy = new URL(process.env.HTTPS_PROXY); } catch { say("proxy", false, "the shield has no proxy setting"); process.exit(0); }
const socket = net.connect(Number(proxy.port), proxy.hostname);
socket.setTimeout(5000);
socket.on("timeout", () => { say("proxy", false, "no answer from " + proxy.host); socket.destroy(); });
socket.on("error", (e) => say("proxy", false, e.message));
socket.on("connect", async () => {
  socket.end();
  say("proxy", true, "reached " + proxy.host);
  try {
    const res = await fetch("https://" + host + "/", { method: "HEAD", redirect: "manual", signal: AbortSignal.timeout(15000) });
    const blocked = res.headers.get("x-zwinger-blocked");
    const error = res.headers.get("x-zwinger-error");
    say("target", !blocked && !error, blocked ? "blocked: " + blocked : error ? "failed: " + error : "HTTP " + res.status);
  } catch (e) {
    say("target", false, String((e.cause && e.cause.message) || e.message));
  }
});
"#;

/// Reads the lines of [`NET_TEST`].
fn net_test(out: &str) -> NetTest {
    #[derive(Deserialize)]
    struct Line {
        stage: String,
        #[serde(flatten)]
        result: NetStage,
    }
    let lines: Vec<Line> = out.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let stage = |name: &str| lines.iter().find(|l| l.stage == name).map(|l| l.result.clone());
    let proxy = stage("proxy").unwrap_or(NetStage { ok: false, detail: "the test did not answer in time".into() });
    let target = if proxy.ok {
        Some(stage("target").unwrap_or(NetStage { ok: false, detail: "the host did not answer in time".into() }))
    } else {
        None
    };
    NetTest { proxy, target }
}

/// The names of the credentials, which the guest gets as placeholder variables.
fn placeholders(policy: &NetworkPolicy) -> Vec<String> {
    let mut names: Vec<String> = policy.credentials().into_iter().map(|c| c.name.clone()).collect();
    names.sort();
    names
}

fn bin_dir(data: &Path, id: &str) -> PathBuf {
    data.join("bin").join(id)
}

/// The agents that run in the VM: pi always, the others if their runtime is mounted.
fn mounted(runtimes: &[&str]) -> Vec<AgentKind> {
    host_agents::KINDS.into_iter().filter(|k| *k == AgentKind::Pi || runtimes.contains(&k.name())).collect()
}

/// The end of a probe's output, else its exit code.
fn probe_error(out: &[u8], code: i32) -> String {
    let text = String::from_utf8_lossy(out);
    let tail: String = text.trim().chars().rev().take(PROBE_ERR).collect::<Vec<_>>().into_iter().rev().collect();
    if tail.is_empty() { format!("it ended with code {code}") } else { tail }
}

/// Runs file reads off the async workers.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(|e| Error::Io(std::io::Error::other(e)))?
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::sync::mpsc;

    use super::*;
    use crate::host_agents::fake::{FakeClock, FakeFs};
    use crate::host_agents::login::fake::{FakeLogins, claude_doc};
    use crate::runtime::PTY_QUEUE;
    use crate::runtime::fake::{FakeRuntime, PtyGate};
    use crate::secrets::fake::MemoryVault;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn vault() -> Arc<MemoryVault> {
        Arc::new(MemoryVault::default())
    }

    /// No host agents.
    fn host() -> Arc<HostAgents> {
        host_with(FakeFs::default(), FakeLogins::default(), 0)
    }

    fn host_with(fs: FakeFs, logins: FakeLogins, now: i64) -> Arc<HostAgents> {
        Arc::new(HostAgents::new(Arc::new(fs), Arc::new(logins), Arc::new(FakeClock(now.into())), temp_dir()))
    }

    fn manager() -> (Manager<FakeRuntime>, FakeRuntime, String) {
        let (manager, runtime, id, _) = manager_with_vault();
        (manager, runtime, id)
    }

    fn manager_with_vault() -> (Manager<FakeRuntime>, FakeRuntime, String, Arc<MemoryVault>) {
        let dir = temp_dir();
        let runtime = FakeRuntime::default();
        let vault = vault();
        let manager =
            Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir(), host())
                .unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        (manager, runtime, id, vault)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<PtyEvent>) -> PtyEvent {
        tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap()
    }

    fn output(text: &str) -> PtyEvent {
        PtyEvent::Output(text.as_bytes().to_vec())
    }

    fn attach(manager: &Manager<FakeRuntime>, session: &str) -> (u64, mpsc::UnboundedReceiver<PtyEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = manager
            .attach_session(session, move |e| {
                let _ = tx.send(e);
            })
            .unwrap();
        (token, rx)
    }

    fn host_rule(host: &str, action: Action) -> RuleDraft {
        RuleDraft { id: None, target: Target::Host { host: host.into() }, action }
    }

    /// An Allow rule for `host`. Returns its id.
    fn allow<R: SandboxRuntime>(manager: &Manager<R>, id: &str, host: &str) -> String {
        manager.put_rule(id, host_rule(host, Action::Allow)).unwrap()
    }

    fn bearer() -> Option<Inject> {
        Some(Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() })
    }

    /// Sets the credential `name` on an Allow rule for `host`.
    fn credential<R: SandboxRuntime>(
        manager: &Manager<R>,
        id: &str,
        host: &str,
        name: &str,
        value: Option<&str>,
    ) -> Result<()> {
        let rule = allow(manager, id, host);
        manager.set_rule_credential(id, &rule, name, bearer(), value.map(SecretValue::new))
    }

    fn live(manager: &Manager<FakeRuntime>, id: &str) -> Arc<Live> {
        lock(&manager.state).live.get(id).cloned().expect("the shield has a proxy")
    }

    #[tokio::test]
    async fn delete_refuses_a_shield_that_is_starting() {
        let (manager, _runtime, id, vault) = manager_with_vault();
        credential(&manager, &id, "api.example.com", "TOKEN", Some("v")).unwrap();
        lock(&manager.state).vms.insert(id.clone(), Vm::Starting);
        assert!(matches!(manager.delete(&id).await, Err(Error::Starting)));
        assert_eq!(manager.list().len(), 1);
        assert!(vault.get(&id, "TOKEN").unwrap().is_some());
    }

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

    /// Stops the shield (and in the second case starts it again) while
    /// `open_pty` is held, then releases it.
    async fn open_during(restart: bool) {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.pty_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let opening = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.open_session(&id, SessionKind::Shell, 24, 80).await }
        });
        gate.entered.notified().await;
        manager.stop(&id).await.unwrap();
        if restart {
            *runtime.pty_gate.lock().unwrap() = None;
            manager.start(&id).await.unwrap();
        }
        gate.release.notify_one();

        assert!(matches!(opening.await.unwrap(), Err(Error::NotRunning)));
        assert!(manager.list()[0].sessions.is_empty());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !runtime.log.lock().unwrap().contains(&"kill".to_owned()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the new process was not killed");
    }

    #[tokio::test]
    async fn session_opened_during_stop_is_killed() {
        open_during(false).await;
    }

    #[tokio::test]
    async fn session_opened_during_restart_is_killed() {
        open_during(true).await;
    }

    fn agent(agent: AgentKind, resume: bool) -> SessionKind {
        SessionKind::Agent { agent, resume }
    }

    fn wrappers(manager: &Manager<FakeRuntime>, id: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(manager.bin_dir(id))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_network_test_knows_the_responses_of_the_proxy() {
        for header in [proxy::BLOCKED_HEADER, proxy::ERROR_HEADER] {
            assert!(NET_TEST.contains(&format!("res.headers.get(\"{header}\")")), "{header}");
        }
        let out = r#"{"stage":"proxy","ok":true,"detail":"reached p"}
{"stage":"target","ok":false,"detail":"failed: cannot connect to a.io in time"}"#;
        let target = net_test(out).target.unwrap();
        assert_eq!((target.ok, target.detail.as_str()), (false, "failed: cannot connect to a.io in time"));
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

    #[tokio::test]
    async fn the_unprompted_setting_rewrites_the_wrappers_at_once() {
        let (manager, _, id) = manager();
        runtimes::fake_install(&manager.data, "claude");
        manager.start(&id).await.unwrap();
        let claude = || std::fs::read_to_string(manager.bin_dir(&id).join("claude")).unwrap();
        assert!(claude().contains("--dangerously-skip-permissions \"$@\""));

        manager.set_agents_unprompted(&id, false).unwrap();
        assert!(!claude().contains("dangerously"));
        assert!(!manager.list()[0].shield.agents_unprompted);
        assert!(!manager.list()[0].pending_restart);

        // Stopped: the next start writes the stored setting.
        manager.stop(&id).await.unwrap();
        manager.set_agents_unprompted(&id, true).unwrap();
        manager.start(&id).await.unwrap();
        assert!(claude().contains("dangerously"));
        manager.stop(&id).await.unwrap();
        manager.delete(&id).await.unwrap();
        assert!(!manager.bin_dir(&id).exists());
    }

    #[tokio::test]
    async fn start_refuses_old_node() {
        let (manager, runtime, id) = manager();
        *runtime.node_version.lock().unwrap() = Some("v22.12.0\n".into());
        assert!(matches!(manager.start(&id).await, Err(Error::NodeTooOld(v)) if v == "v22.12.0"));
        assert!(matches!(manager.list()[0].status, Status::Error(_)));
        assert_eq!(*runtime.log.lock().unwrap(), [format!("start {id}"), format!("stop {id}")]);
    }

    #[tokio::test]
    async fn start_refuses_a_mount_of_the_app_data_dir() {
        let (manager, runtime, id) = manager();
        manager.add_mount(&id, manager.data.to_str().unwrap(), "/mnt/data", true).unwrap();
        assert!(matches!(manager.start(&id).await, Err(Error::AppDataOverlap(..))));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(runtime.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_removes_the_agent_state_and_the_old_chats() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        let sessions = manager.state_dir(&id).join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        std::fs::write(sessions.join("a.jsonl"), "{\"type\":\"session\",\"timestamp\":\"t\"}\n").unwrap();
        let chats = manager.data.join("chats").join(&id).join("claude");
        std::fs::create_dir_all(&chats).unwrap();
        manager.stop(&id).await.unwrap();

        manager.delete(&id).await.unwrap();
        assert!(!manager.state_dir(&id).exists());
        assert!(!manager.data.join("chats").join(&id).exists());
    }

    #[tokio::test]
    async fn start_refuses_changed_path() {
        let (manager, runtime, id) = manager();
        let folder = manager.list()[0].shield.folder.clone();
        std::fs::remove_dir_all(&folder).unwrap();
        assert!(matches!(manager.start(&id).await, Err(Error::MissingPath(_))));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(runtime.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stop_all_stops_running_shields() {
        let dir = temp_dir();
        let runtime = FakeRuntime::default();
        let manager =
            Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault(), temp_dir(), host()).unwrap();
        let a = manager.create(temp_dir().to_str().unwrap()).unwrap();
        let b = manager.create(temp_dir().to_str().unwrap()).unwrap();
        manager.create(temp_dir().to_str().unwrap()).unwrap();
        manager.start(&a).await.unwrap();
        manager.start(&b).await.unwrap();

        manager.stop_all(Duration::from_secs(2)).await;
        assert!(manager.list().iter().all(|v| v.status == Status::Stopped));
        let log = runtime.log.lock().unwrap();
        assert!(log.contains(&format!("stop {a}")) && log.contains(&format!("stop {b}")));
    }

    #[tokio::test]
    async fn the_update_restarts_only_when_no_shield_is_live() {
        let (manager, runtime, id) = manager();
        assert!(crate::updates::restart_guard(manager.live()).is_ok());
        manager.start(&id).await.unwrap();
        assert!(matches!(crate::updates::restart_guard(manager.live()), Err(Error::ShieldsRunning(1))));

        *runtime.fail_stop.lock().unwrap() = true;
        manager.stop_all(Duration::from_secs(2)).await;
        assert!(matches!(crate::updates::restart_guard(manager.live()), Err(Error::ShieldsRunning(1))));

        *runtime.fail_stop.lock().unwrap() = false;
        manager.stop_all(Duration::from_secs(2)).await;
        assert!(crate::updates::restart_guard(manager.live()).is_ok());
    }

    #[tokio::test]
    async fn stop_kills_sessions() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        let session = manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80).await.unwrap();
        let (_, mut rx) = attach(&manager, &session);
        next(&mut rx).await;

        manager.stop(&id).await.unwrap();
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
        assert!(manager.list()[0].sessions.is_empty());
        // The terminal stays as a dormant row.
        assert_eq!(manager.list()[0].shield.terminals.len(), 1);
    }

    #[tokio::test]
    async fn failed_stop_keeps_vm_running() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();

        *runtime.fail_stop.lock().unwrap() = true;
        assert!(manager.stop(&id).await.is_err());
        assert_eq!(manager.list()[0].status, Status::Running);

        *runtime.fail_stop.lock().unwrap() = false;
        manager.stop(&id).await.unwrap();
        assert_eq!(manager.list()[0].status, Status::Stopped);
    }

    fn dns_denied(at: &str, host: &str) -> String {
        format!(
            "{at} DEBUG microsandbox_network::engine::dns::forwarder: DNS query denied by network policy domain={host}"
        )
    }

    fn net_sender(runtime: &FakeRuntime, index: usize) -> mpsc::Sender<String> {
        runtime.net_lines.lock().unwrap()[index].clone()
    }

    async fn net_closed(runtime: &FakeRuntime, index: usize) {
        tokio::time::timeout(Duration::from_secs(2), net_sender(runtime, index).closed())
            .await
            .expect("the follow task still runs");
    }

    fn hosts(events: &[Entry]) -> Vec<(u64, &str)> {
        events.iter().map(|e| (e.seq, e.host.as_str())).collect()
    }

    #[tokio::test]
    async fn network_events_reach_the_ring_and_the_sink() {
        let (manager, runtime, id) = manager();
        // A pair that dedupe drops, and junk.
        *runtime.net_script.lock().unwrap() = vec![
            dns_denied("2999-01-01T00:00:00Z", "google.com"),
            dns_denied("2999-01-01T00:00:00.1Z", "google.com"),
            "junk".into(),
            dns_denied("2999-01-01T00:00:00.2Z", "example.com"),
        ];
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.set_net_sink(Box::new(move |shield, event| {
            let _ = tx.send((shield.to_owned(), event.clone()));
        }));
        manager.start(&id).await.unwrap();

        let mut sent = Vec::new();
        for _ in 0..2 {
            let (shield, event) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
            assert_eq!(shield, id);
            sent.push(event);
        }
        assert_eq!(hosts(&sent), [(1, "google.com"), (2, "example.com")]);
        assert!(sent.iter().all(|e| e.reason == crate::proxy::log::DIRECT && !e.allowed));
        assert_eq!(manager.list_net_events(&id).unwrap(), sent);

        // The ring stays after a stop; a restart continues the numbers.
        manager.stop(&id).await.unwrap();
        net_closed(&runtime, 0).await;
        assert_eq!(manager.list_net_events(&id).unwrap().len(), 2);
        *runtime.net_script.lock().unwrap() = vec![dns_denied("2999-01-01T00:00:01Z", "google.com")];
        manager.start(&id).await.unwrap();
        let (_, event) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        assert_eq!(event.seq, 3);

        manager.delete(&id).await.unwrap();
        net_closed(&runtime, 1).await;
        assert!(lock(&manager.state).net.is_empty());
    }

    #[tokio::test]
    async fn failed_stop_keeps_the_follow_task() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        *runtime.fail_stop.lock().unwrap() = true;
        assert!(manager.stop(&id).await.is_err());

        net_sender(&runtime, 0).send(dns_denied("2999-01-01T00:00:00Z", "google.com")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while manager.list_net_events(&id).unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the event did not arrive");

        *runtime.fail_stop.lock().unwrap() = false;
        manager.stop(&id).await.unwrap();
        net_closed(&runtime, 0).await;
    }

    #[tokio::test]
    async fn network_edits_reach_the_running_proxy_with_no_restart() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let net = runtime.net.lock().unwrap().clone().unwrap();
        let env: BTreeMap<_, _> = net.env.iter().cloned().collect();
        assert!(env["HTTPS_PROXY"].ends_with(&format!("@host.microsandbox.internal:{}", net.port)));
        assert_eq!(env["NODE_USE_ENV_PROXY"], "1");

        let rule = allow(&manager, &id, "Registry.npmjs.org");
        manager.set_network_default(&id, DefaultAction::Allow).unwrap();
        manager.set_offline(&id, true).unwrap();
        let policy = live(&manager, &id).snapshot().policy;
        assert_eq!(
            (policy.default, policy.offline, policy.rules[0].id.as_str()),
            (DefaultAction::Allow, true, rule.as_str())
        );
        assert_eq!(policy.rules[0].target, Target::Host { host: "registry.npmjs.org".into() });
        assert!(!manager.list()[0].pending_restart);

        // A new credential name needs its placeholder variable in the guest.
        manager.set_rule_credential(&id, &rule, "NPM_TOKEN", bearer(), Some(SecretValue::new("t"))).unwrap();
        assert!(manager.list()[0].pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values["NPM_TOKEN"], SecretValue::new("t"));
        manager.stop(&id).await.unwrap();
        assert!(lock(&manager.state).live.is_empty());
        manager.start(&id).await.unwrap();
        assert!(!manager.list()[0].pending_restart);
        let net = runtime.net.lock().unwrap().clone().unwrap();
        assert!(net.env.contains(&("NPM_TOKEN".to_owned(), "$MSB_NPM_TOKEN$".to_owned())));
        assert!(!net.env.iter().any(|(_, v)| v == "t"));
    }

    #[test]
    fn rule_edits_dedupe_and_validate() {
        let (manager, _, id) = manager();
        let first = allow(&manager, &id, "registry.npmjs.org");
        assert_eq!(manager.put_rule(&id, host_rule("Registry.NPMjs.org", Action::Block)).unwrap(), first);
        allow(&manager, &id, "*.anthropic.com");
        assert!(matches!(
            manager.put_rule(&id, host_rule("https://example.com", Action::Allow)),
            Err(Error::InvalidDomain(..))
        ));
        assert!(matches!(manager.put_rule("nope", host_rule("a.io", Action::Allow)), Err(Error::ShieldNotFound)));
        manager.remove_rule(&id, &first).unwrap();
        assert!(matches!(manager.remove_rule(&id, &first), Err(Error::RuleNotFound)));
        let rules = manager.list()[0].shield.network.rules.clone();
        assert_eq!(rules.iter().map(|r| r.label()).collect::<Vec<_>>(), ["*.anthropic.com"]);
    }

    #[test]
    fn close_unknown_terminal_is_ok() {
        let (manager, _, id) = manager();
        manager.close_terminal(&id, "gone").unwrap();
    }

    #[tokio::test]
    async fn config_persists_across_managers() {
        let dir = temp_dir();
        let store = || Store::new(dir.join("shields.json"));
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.add_mount(&id, temp_dir().to_str().unwrap(), "/mnt/data", true).unwrap();
        manager.set_mount_readonly(&id, "/mnt/data", false).unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let mounts = &reloaded.list()[0].shield.mounts;
        assert_eq!(mounts.len(), 1);
        assert!(!mounts[0].readonly);

        reloaded.delete(&id).await.unwrap();
        assert!(Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap().list().is_empty());
    }

    #[tokio::test]
    async fn remove_mount_used_by_harness_is_refused() {
        let (manager, _, id) = manager();
        manager.add_mount(&id, temp_dir().to_str().unwrap(), "/mnt/agent", true).unwrap();
        manager.set_harness(&id, Some("/mnt/agent/skills")).unwrap();
        let error = manager.remove_mount(&id, "/mnt/agent").unwrap_err();
        assert!(error.to_string().contains("harness uses /mnt/agent/skills"), "{error}");
        assert_eq!(manager.list()[0].shield.mounts.len(), 1);

        manager.set_harness(&id, None).unwrap();
        manager.remove_mount(&id, "/mnt/agent").unwrap();
        assert!(manager.list()[0].shield.mounts.is_empty());
    }

    #[tokio::test]
    async fn rename_while_running_needs_no_restart() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        manager.rename(&id, " api ").unwrap();
        let view = &manager.list()[0];
        assert_eq!(view.shield.name, "api");
        assert!(!view.pending_restart);
        assert!(matches!(manager.rename(&id, ""), Err(Error::InvalidName(_))));
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

    fn task_draft(id: Option<String>, title: &str, kinds: [AgentKind; 2]) -> TaskDraft {
        TaskDraft {
            id,
            title: title.into(),
            request: "  Fix the map.  ".into(),
            agents: kinds.map(|kind| shield::TaskAgent { kind, model: None }),
            gates: vec!["pnpm build".into(), "  ".into()],
            workdir: "/workspace".into(),
        }
    }

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
        let tasks = manager.list()[0].shield.tasks.clone();
        assert_eq!(tasks.iter().map(|t| t.slug.as_str()).collect::<Vec<_>>(), ["fix-the-map", "fix-the-map-2"]);
        assert_eq!(
            (tasks[0].request.as_str(), tasks[0].gates.as_slice()),
            ("Fix the map.", ["pnpm build".to_owned()].as_slice())
        );
        // A rename keeps the folder of the runs.
        manager.put_task(&id, task_draft(Some(task_id.clone()), "Renamed", kinds)).unwrap();
        assert_eq!(manager.list()[0].shield.tasks[0].slug, "fix-the-map");
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
        assert_eq!(manager.list()[0].shield.tasks.len(), 1);
    }

    fn app(manager: &Manager<FakeRuntime>, id: &str, command: &str, autostart: bool) -> String {
        manager
            .put_app(
                id,
                AppDraft { id: None, name: command.into(), command: command.into(), cwd: WORKSPACE.into(), autostart },
            )
            .unwrap()
    }

    fn runs(manager: &Manager<FakeRuntime>) -> Vec<(String, Option<i32>)> {
        manager.list()[0].app_runs.iter().map(|r| (r.app_id.clone(), r.exit_code)).collect()
    }

    async fn until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !done() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition not met");
    }

    #[tokio::test]
    async fn layout_app_and_order_edits_need_no_restart() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        manager.set_layout(&id, Some([("harness".to_owned(), Pos { x: 1.0, y: 2.0 })].into())).unwrap();
        let web = app(&manager, &id, "pnpm dev", false);
        app(&manager, &id, "cargo run", true);
        let mut ids: Vec<String> = manager.list()[0].shield.apps.iter().map(|a| a.id.clone()).collect();
        ids.reverse();
        manager.reorder_apps(&id, &ids).unwrap();
        manager.remove_app(&id, &web).unwrap();
        manager.reorder_shields(std::slice::from_ref(&id)).unwrap();
        let view = &manager.list()[0];
        assert!(!view.pending_restart);
        assert_eq!(view.shield.apps.len(), 1);
        assert_eq!(view.shield.layout.len(), 1);
    }

    #[tokio::test]
    async fn reorder_shields_persists() {
        let dir = temp_dir();
        let store = || Store::new(dir.join("shields.json"));
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let a = manager.create(temp_dir().to_str().unwrap()).unwrap();
        let b = manager.create(temp_dir().to_str().unwrap()).unwrap();
        assert!(matches!(manager.reorder_shields(std::slice::from_ref(&b)), Err(Error::InvalidOrder)));
        manager.reorder_shields(&[b.clone(), a.clone()]).unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir(), host()).unwrap();
        let order: Vec<String> = reloaded.list().into_iter().map(|v| v.shield.id).collect();
        assert_eq!(order, [b, a]);
    }

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
    async fn a_stale_detach_keeps_the_new_sink() {
        let (manager, _, id) = manager();
        let web = app(&manager, &id, "pnpm dev", false);
        manager.start(&id).await.unwrap();
        let session = manager.start_app(&id, &web).await.unwrap();
        let (old, _old_rx) = attach(&manager, &session);
        let (new, mut rx) = attach(&manager, &session);
        assert_ne!(old, new);
        manager.detach_session(&session, old);
        let _ = next(&mut rx).await;
        manager.write(&session, b"x".to_vec()).unwrap();
        while next(&mut rx).await != output("x") {}

        manager.detach_session(&session, new);
        manager.write(&session, b"y".to_vec()).unwrap();
        manager.send(&session, PtyInput::Kill).unwrap();
        until(|| runs(&manager) == [(web.clone(), Some(137))]).await;
        assert!(rx.try_recv().is_err());
        manager.detach_session("gone", new);
    }

    #[test]
    fn the_ring_keeps_the_last_bytes_only() {
        let mut output = Output::default();
        output.push(PtyEvent::Output(vec![b'a'; RING - 1]));
        output.push(PtyEvent::Output(b"bcd".to_vec()));
        assert_eq!(output.ring.len(), RING);
        assert!(output.ring.iter().rev().take(3).eq(b"dcb"));
        assert_eq!(output.ring[0], b'a');
        output.push(PtyEvent::Output(vec![b'z'; 2 * RING]));
        assert_eq!(output.ring.len(), RING);
        assert!(output.ring.iter().all(|b| *b == b'z'));
    }

    #[tokio::test]
    async fn a_reattach_replays_the_output_in_order_and_a_stale_token_gets_nothing() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        let session = manager.open_session(&id, SessionKind::Shell, 24, 80).await.unwrap();
        for text in ["one ", "two ", "three"] {
            manager.write(&session, text.as_bytes().to_vec()).unwrap();
        }
        until(|| lock(&manager.output(&session).unwrap()).ring.len() == "24x80one two three".len()).await;

        let (old, mut old_rx) = attach(&manager, &session);
        assert_eq!(next(&mut old_rx).await, output("24x80one two three"));
        let (new, mut rx) = attach(&manager, &session);
        assert_eq!(next(&mut rx).await, output("24x80one two three"));
        manager.write(&session, b"four".to_vec()).unwrap();
        assert_eq!(next(&mut rx).await, output("four"));
        // The stale view gets no new output, and its detach and ack change nothing.
        manager.detach_session(&session, old);
        manager.ack_session(&session, old);
        assert_eq!(
            lock(&manager.output(&session).unwrap()).sink.as_ref().map(|s| (s.token, s.in_flight)),
            Some((new, 2))
        );
        manager.send(&session, PtyInput::Kill).unwrap();
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
        assert!(old_rx.try_recv().is_err());

        // After the exit, an attach gets the output and then the exit.
        let (_, mut rx) = attach(&manager, &session);
        assert_eq!(next(&mut rx).await, output("24x80one two threefour"));
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_coalesces_up_to_its_size_and_its_wait() {
        let (tx, events) = mpsc::channel(PTY_QUEUE);
        let mut frames = Frames::new(events);
        let start = Instant::now();

        // A small chunk with nothing queued goes at once.
        tx.send(output("k")).await.unwrap();
        assert_eq!(frames.next().await, Some((b"k".to_vec(), None)));
        assert_eq!(start.elapsed(), Duration::ZERO);

        // Queued chunks merge up to the frame size.
        for _ in 0..6 {
            tx.send(PtyEvent::Output(vec![b'x'; 16 << 10])).await.unwrap();
        }
        assert_eq!(frames.next().await.unwrap().0.len(), FRAME);
        assert_eq!(start.elapsed(), Duration::ZERO);

        // The rest waits for more until the frame wait ends.
        assert_eq!(frames.next().await.unwrap().0.len(), 32 << 10);
        assert_eq!(start.elapsed(), FRAME_WAIT);

        // Output that comes during the wait joins the frame; the exit ends it.
        let sender = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(3)).await;
            sender.send(output("b")).await.unwrap();
            sender.send(PtyEvent::Exit(0)).await.unwrap();
        });
        tx.send(PtyEvent::Output(vec![b'a'; ECHO + 1])).await.unwrap();
        let (bytes, exit) = frames.next().await.unwrap();
        assert_eq!((bytes.len(), bytes.last(), exit), (ECHO + 2, Some(&b'b'), Some(0)));
        drop(tx);
        assert_eq!(frames.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn output_past_the_frame_size_goes_to_the_next_frame() {
        let (tx, events) = mpsc::channel(PTY_QUEUE);
        let mut frames = Frames::new(events);
        tx.send(PtyEvent::Output(vec![b'a'; 48 << 10])).await.unwrap();
        tx.send(PtyEvent::Output(vec![b'b'; 48 << 10])).await.unwrap();
        tx.send(PtyEvent::Output(vec![b'c'; 3 * FRAME])).await.unwrap();
        tx.send(PtyEvent::Exit(0)).await.unwrap();
        drop(tx);
        let mut all = Vec::new();
        let exit = loop {
            let (bytes, exit) = frames.next().await.unwrap();
            assert!(bytes.len() <= FRAME);
            all.extend(bytes);
            if exit.is_some() {
                break exit;
            }
        };
        assert_eq!(exit, Some(0));
        assert_eq!(all.len(), (96 << 10) + 3 * FRAME);
        assert!(all[..48 << 10].iter().all(|b| *b == b'a'));
        assert!(all[48 << 10..96 << 10].iter().all(|b| *b == b'b'));
        assert!(all[96 << 10..].iter().all(|b| *b == b'c'));
        assert_eq!(frames.next().await, None);
    }

    #[test]
    fn an_attach_replays_the_ring_in_one_frame() {
        let mut output = Output::default();
        output.push(PtyEvent::Output((0..RING).map(|i| i as u8).collect()));
        let sent = Arc::new(Mutex::new(Vec::<PtyEvent>::new()));
        let into = sent.clone();
        output.attach(Box::new(move |event| lock(&into).push(event)));
        let sent = lock(&sent);
        assert_eq!(sent.len(), 1);
        assert_eq!(output.sink.as_ref().unwrap().in_flight, 1);
        let PtyEvent::Output(bytes) = &sent[0] else { panic!("{:?}", sent[0]) };
        assert!(bytes.iter().eq(output.ring.iter()));
    }

    #[tokio::test]
    async fn a_burst_stays_bounded_and_a_full_view_holds_the_process() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let burst = 50 << 20;
        *runtime.burst.lock().unwrap() = burst;
        let sent = runtime.burst_sent.clone();
        let session = manager.open_session(&id, SessionKind::Shell, 24, 80).await.unwrap();
        let (token, mut rx) = attach(&manager, &session);

        // A view that acknowledges nothing holds the process after its window.
        let mut frames = 0;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            frames += 1;
        }
        assert!(frames <= WINDOW + 1, "{frames} frames");
        // The fake prints chunks of one frame each: the window, the queue and the frame that waits.
        let held = sent.load(std::sync::atomic::Ordering::SeqCst);
        assert!(held <= (WINDOW + PTY_QUEUE + 2) * FRAME, "{held} bytes read");

        // Each acknowledged frame lets one more frame through.
        manager.ack_session(&session, token);
        assert!(next(&mut rx).await != PtyEvent::Exit(0));

        // A detached session drains into the ring.
        manager.detach_session(&session, token);
        let output = manager.output(&session).unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while sent.load(std::sync::atomic::Ordering::SeqCst) < burst || lock(&output).ring.back() != Some(&b'x') {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the burst did not drain");
        assert_eq!(lock(&output).ring.len(), RING);
    }

    fn terminal_ids(manager: &Manager<FakeRuntime>) -> Vec<String> {
        manager.list()[0].shield.terminals.iter().map(|t| t.id.clone()).collect()
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

    async fn app_start_during(remove: bool) {
        let (manager, runtime, id) = manager();
        let web = app(&manager, &id, "pnpm dev", false);
        manager.start(&id).await.unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.pty_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let starting = tokio::spawn({
            let (manager, id, web) = (manager.clone(), id.clone(), web.clone());
            async move { manager.start_app(&id, &web).await }
        });
        gate.entered.notified().await;
        if remove {
            manager.remove_app(&id, &web).unwrap();
        } else {
            manager.stop_app(&id, &web).unwrap();
        }
        gate.release.notify_one();

        let result = starting.await.unwrap();
        if remove {
            assert!(matches!(result, Err(Error::AppNotFound)), "{result:?}");
        } else {
            assert!(matches!(result, Err(Error::AppStopped)), "{result:?}");
        }
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

    #[test]
    fn list_folders_reads_the_shield_folder() {
        let (manager, _, id) = manager();
        let folder = manager.list()[0].shield.folder.clone();
        std::fs::create_dir(folder.join("web")).unwrap();
        assert_eq!(manager.list_folders(&id).unwrap(), ["web"]);
        assert!(matches!(manager.list_folders("nope"), Err(Error::ShieldNotFound)));
    }

    fn value(text: &str) -> Option<SecretValue> {
        Some(SecretValue::new(text))
    }

    fn stored(vault: &MemoryVault, id: &str, name: &str) -> Option<String> {
        vault.get(id, name).unwrap().map(|v| v.expose().to_owned())
    }

    /// Makes the next config save fail: the store renames onto a non-empty folder.
    fn break_store(manager: &Manager<FakeRuntime>) {
        let path = manager.store.path().to_owned();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(path.join("blocker")).unwrap();
    }

    #[tokio::test]
    async fn credential_values_stay_out_of_the_config() {
        let (manager, runtime, id, vault) = manager_with_vault();
        credential(&manager, &id, "api.openai.com", " OPENAI_API_KEY ", Some("sk-hidden")).unwrap();
        assert_eq!(stored(&vault, &id, "OPENAI_API_KEY").as_deref(), Some("sk-hidden"));
        let rules = manager.list()[0].shield.network.rules.clone();
        assert_eq!(rules[0].credentials[0].name, "OPENAI_API_KEY");

        let config = std::fs::read_to_string(manager.store.path()).unwrap();
        let shields = serde_json::to_string(&manager.list()).unwrap();
        for text in [config, shields] {
            assert!(text.contains("OPENAI_API_KEY") && !text.contains("sk-hidden"), "{text}");
        }

        manager.start(&id).await.unwrap();
        let net = runtime.net.lock().unwrap().clone().unwrap();
        assert!(net.env.contains(&("OPENAI_API_KEY".to_owned(), "$MSB_OPENAI_API_KEY$".to_owned())));
        assert!(!format!("{net:?}").contains("sk-hidden"));
        assert_eq!(live(&manager, &id).snapshot().values["OPENAI_API_KEY"], SecretValue::new("sk-hidden"));
    }

    #[test]
    fn a_credential_needs_a_value_only_when_new() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        let set = |name: &str, inject: Option<Inject>, value: Option<SecretValue>| {
            manager.set_rule_credential(&id, &rule, name, inject, value)
        };
        assert!(matches!(set("KEY", bearer(), None), Err(Error::SecretValueRequired(_))));
        assert!(matches!(set("KEY", bearer(), value("")), Err(Error::SecretValueRequired(_))));
        assert!(manager.list()[0].shield.network.rules[0].credentials.is_empty());

        set("KEY", bearer(), value("v1")).unwrap();
        let replace = Inject::Replace {
            places: secrets::Places { header_fields: vec!["x-api-key".into()], query: false, body: false },
        };
        set("KEY", Some(replace.clone()), None).unwrap();
        assert_eq!(manager.list()[0].shield.network.rules[0].credentials[0].inject, replace);
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v1"));

        assert!(matches!(set("PATH", bearer(), value("x")), Err(Error::InvalidSecret(..))));
        assert!(matches!(
            manager.set_rule_credential("nope", &rule, "KEY", bearer(), value("x")),
            Err(Error::ShieldNotFound)
        ));
        let wildcard = manager.put_rule(&id, host_rule("*.b.io", Action::Allow)).unwrap();
        assert!(matches!(
            manager.set_rule_credential(&id, &wildcard, "OTHER", bearer(), value("x")),
            Err(Error::InvalidSecret(..))
        ));
        assert_eq!(vault.values.lock().unwrap().len(), 1);
        // A rule with a credential cannot become a wildcard; it can become a Block rule.
        let moved =
            RuleDraft { id: Some(rule.clone()), target: Target::Host { host: "*.a.io".into() }, action: Action::Allow };
        assert!(matches!(manager.put_rule(&id, moved), Err(Error::InvalidSecret(..))));
        manager
            .put_rule(
                &id,
                RuleDraft { id: Some(rule), target: Target::Host { host: "a.io".into() }, action: Action::Block },
            )
            .unwrap();
        assert_eq!(manager.list()[0].shield.network.rules[0].credentials.len(), 1);
    }

    #[test]
    fn failed_save_restores_the_vault() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "OLD", bearer(), value("old")).unwrap();
        break_store(&manager);

        assert!(manager.set_rule_credential(&id, &rule, "OLD", bearer(), value("new")).is_err());
        assert_eq!(stored(&vault, &id, "OLD").as_deref(), Some("old"));

        let error = manager.set_rule_credential(&id, &rule, "NEW", bearer(), value("fresh")).unwrap_err();
        assert!(!error.to_string().contains("fresh"));
        assert_eq!(stored(&vault, &id, "NEW"), None);
        let names: Vec<_> = manager.list()[0].shield.network.credentials().iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, ["OLD"]);
    }

    #[test]
    fn a_removed_credential_saves_the_config_first_and_keeps_a_shared_value() {
        let (manager, _, id, vault) = manager_with_vault();
        let a = allow(&manager, &id, "a.io");
        let b = allow(&manager, &id, "b.io");
        manager.set_rule_credential(&id, &a, "KEY", bearer(), value("v")).unwrap();
        // The same name on a second rule shares the value.
        manager.set_rule_credential(&id, &b, "KEY", bearer(), None).unwrap();
        assert!(matches!(manager.set_rule_credential(&id, &a, "OTHER", None, None), Err(Error::SecretNotFound(_))));

        manager.set_rule_credential(&id, &a, "KEY", None, None).unwrap();
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));

        break_store(&manager);
        assert!(manager.remove_rule(&id, &b).is_err());
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));
        assert_eq!(manager.list()[0].shield.network.credentials().len(), 1);

        let path = manager.store.path().to_owned();
        std::fs::remove_dir_all(&path).unwrap();
        manager.remove_rule(&id, &b).unwrap();
        assert_eq!(stored(&vault, &id, "KEY"), None);
        assert!(manager.list()[0].shield.network.credentials().is_empty());
    }

    #[tokio::test]
    async fn delete_removes_the_values() {
        let (manager, _, id, vault) = manager_with_vault();
        credential(&manager, &id, "a.io", "A", Some("1")).unwrap();
        credential(&manager, &id, "b.io", "B", Some("2")).unwrap();
        manager.delete(&id).await.unwrap();
        assert!(vault.values.lock().unwrap().is_empty());
        assert!(manager.list().is_empty());
    }

    #[tokio::test]
    async fn start_refuses_a_missing_value() {
        let (manager, runtime, id, vault) = manager_with_vault();
        credential(&manager, &id, "a.io", "KEY", Some("v")).unwrap();
        vault.delete(&id, "KEY").unwrap();

        assert!(matches!(manager.start(&id).await, Err(Error::SecretMissing(n)) if n == "KEY"));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(runtime.log.lock().unwrap().is_empty());
        assert!(lock(&manager.state).live.is_empty());
    }

    /// `shields.json` can be edited by hand. A rule that the checks do not
    /// accept must refuse the start.
    #[tokio::test]
    async fn start_refuses_an_invalid_stored_host() {
        let (manager, runtime, id) = manager();
        allow(&manager, &id, "a.io");
        lock(&manager.state).shields[0].network.rules[0].target = Target::Host { host: "*".into() };

        assert!(matches!(manager.start(&id).await, Err(Error::InvalidDomain(..))));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(runtime.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn value_changes_while_running_reach_the_proxy_at_once() {
        let (manager, _, id) = manager();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v1")).unwrap();
        manager.start(&id).await.unwrap();

        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v2")).unwrap();
        assert!(!manager.list()[0].pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values["KEY"], SecretValue::new("v2"));

        // The guest still has the placeholder variable of a removed name.
        manager.set_rule_credential(&id, &rule, "KEY", None, None).unwrap();
        assert!(!live(&manager, &id).snapshot().values.contains_key("KEY"));
        assert!(manager.list()[0].pending_restart);
    }

    #[tokio::test]
    async fn a_value_change_during_start_reaches_the_proxy() {
        let (manager, runtime, id) = manager();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v1")).unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.start_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let starting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.start(&id).await }
        });
        gate.entered.notified().await;
        assert_eq!(manager.list()[0].status, Status::Starting);
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v2")).unwrap();
        gate.release.notify_one();

        starting.await.unwrap().unwrap();
        let view = &manager.list()[0];
        assert_eq!(view.status, Status::Running);
        assert!(!view.pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values["KEY"], SecretValue::new("v2"));
    }

    #[test]
    fn load_prunes_values_with_no_credential_and_keeps_the_ca_key() {
        let dir = temp_dir();
        let store = || Store::new(dir.join("shields.json"));
        let vault = vault();
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        credential(&manager, &id, "a.io", "KEY", Some("keep")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.set("deleted-shield", "KEY", &SecretValue::new("y")).unwrap();
        vault.set(ca::KEY_ENTRY.0, ca::KEY_ENTRY.1, &SecretValue::new("ca")).unwrap();

        Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let mut accounts = vault.accounts().unwrap();
        accounts.sort();
        let mut kept = [secrets::account(&id, "KEY"), secrets::account(ca::KEY_ENTRY.0, ca::KEY_ENTRY.1)];
        kept.sort();
        assert_eq!(accounts, kept);
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("keep"));
    }

    #[test]
    fn failed_delete_is_an_error_but_does_not_stop_the_load() {
        let dir = temp_dir();
        let store = || Store::new(dir.join("shields.json"));
        let vault = vault();
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.fail_delete.store(true, std::sync::atomic::Ordering::SeqCst);

        assert!(matches!(manager.set_rule_credential(&id, &rule, "KEY", None, None), Err(Error::Keychain(_))));
        let reloaded = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(stored(&vault, &id, "GONE").as_deref(), Some("x"));
    }

    /// Holds each `set` before the write until the test releases it.
    #[derive(Default)]
    struct GatedVault {
        inner: MemoryVault,
        gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    }

    impl Vault for GatedVault {
        fn set(&self, shield_id: &str, name: &str, value: &SecretValue) -> Result<()> {
            if let Some((entered, release)) = self.gate.lock().unwrap().take() {
                entered.send(()).unwrap();
                release.recv().unwrap();
            }
            self.inner.set(shield_id, name, value)
        }

        fn get(&self, shield_id: &str, name: &str) -> Result<Option<SecretValue>> {
            self.inner.get(shield_id, name)
        }

        fn delete(&self, shield_id: &str, name: &str) -> Result<()> {
            self.inner.delete(shield_id, name)
        }

        fn accounts(&self) -> Result<Vec<String>> {
            self.inner.accounts()
        }
    }

    /// A start during a value edit waits for the edit, so its proxy gets the new value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_during_a_credential_edit_uses_the_new_value() {
        let dir = temp_dir();
        let vault = Arc::new(GatedVault::default());
        let runtime = FakeRuntime::default();
        let manager = Arc::new(
            Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir(), host())
                .unwrap(),
        );
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v1")).unwrap();

        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *vault.gate.lock().unwrap() = Some((entered_tx, release_rx));
        let putting = std::thread::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            move || manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v2"))
        });
        entered.recv().unwrap();

        let starting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.start(&id).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(runtime.log.lock().unwrap().is_empty());
        release.send(()).unwrap();
        putting.join().unwrap().unwrap();
        starting.await.unwrap().unwrap();

        assert_eq!(live(&manager, &id).snapshot().values["KEY"], SecretValue::new("v2"));
        let view = &manager.list()[0];
        assert_eq!(view.status, Status::Running);
        assert!(!view.pending_restart);
    }

    const NOW: i64 = 1_800_000_000_000;

    /// A Mac with Claude Code 2.1.310, a `~/.claude` with a skill, Codex,
    /// pi and Gemini CLI. `home` is a real folder, as the profile copies it.
    fn mac(home: &Path) -> FakeFs {
        let mut fs = FakeFs { home: home.to_owned(), ..FakeFs::default() };
        let bin = home.join(".local/share/claude/versions/2.1.310");
        fs.links.insert(home.join(".local/bin/claude"), bin.clone());
        fs.versions.insert(bin, "2.1.310 (Claude Code)".into());
        fs.dirs.insert(home.join(".claude"));
        std::fs::create_dir_all(home.join(".claude/skills/review")).unwrap();
        std::fs::write(home.join(".claude/skills/review/SKILL.md"), "review").unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "secret").unwrap();
        fs.file("/usr/local/lib/node_modules/@openai/codex/package.json", r#"{"version":"0.163.0"}"#);
        fs.versions.insert("/usr/local/bin/codex".into(), "codex-cli 0.163.0".into());
        for (name, entry) in
            [("@earendil-works/pi-coding-agent", "dist/cli.js"), ("@google/gemini-cli", "bundle/gemini.js")]
        {
            let manifest = format!(r#"{{"version":"1.0.0","bin":{{"gemini":"{entry}"}}}}"#);
            fs.file(&format!("/usr/local/lib/node_modules/{name}/package.json"), &manifest);
            fs.file(&format!("/usr/local/lib/node_modules/{name}/{entry}"), "");
        }
        fs
    }

    fn host_manager(logins: FakeLogins, now: i64) -> (Manager<FakeRuntime>, FakeRuntime, String, Arc<MemoryVault>) {
        let (dir, home) = (temp_dir(), temp_dir());
        let runtime = FakeRuntime::default();
        let vault = vault();
        let host = host_with(mac(&home), logins, now);
        let manager =
            Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir(), host)
                .unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        (manager, runtime, id, vault)
    }

    fn uses(kind: AgentKind, profile: bool, login: bool) -> HostAgentUse {
        HostAgentUse { kind, profile, login }
    }

    #[tokio::test]
    async fn a_host_agent_gets_its_login_and_profile_at_start() {
        let logins = FakeLogins::default();
        logins.docs.lock().unwrap().insert(AgentKind::Claude, claude_doc("sk-ant-oat-real", NOW + 3_600_000));
        let (manager, runtime, id, vault) = host_manager(logins, NOW);
        manager.put_host_agent(&id, uses(AgentKind::Claude, true, true), true).await.unwrap();

        let shield = manager.list().remove(0).shield;
        let rules: Vec<_> = shield.network.rules.iter().map(|r| (r.target.clone(), r.action)).collect();
        assert_eq!(rules, [(Target::Host { host: "api.anthropic.com".into() }, Action::Allow)]);
        assert_eq!(shield.logins[0].name, "CLAUDE_CODE_OAUTH_TOKEN");
        assert_eq!(shield.logins[0].origin, crate::secrets::Origin::HostLogin { agent: AgentKind::Claude });
        // A credential cannot take its name, and the vault never holds it.
        let rule = shield.network.rules[0].id.clone();
        assert!(matches!(
            manager.set_rule_credential(&id, &rule, "CLAUDE_CODE_OAUTH_TOKEN", bearer(), value("x")),
            Err(Error::DuplicateSecret(_))
        ));

        manager.start(&id).await.unwrap();
        let accounts: Vec<String> = vault.values.lock().unwrap().keys().cloned().collect();
        assert_eq!(accounts, [secrets::account(ca::KEY_ENTRY.0, ca::KEY_ENTRY.1)], "only the CA key");
        let net = runtime.net.lock().unwrap().clone().unwrap();
        assert!(net.env.contains(&("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "$MSB_CLAUDE_CODE_OAUTH_TOKEN$".to_owned())));
        assert!(!format!("{net:?}").contains("sk-ant-oat-real"));
        let snapshot = manager.profile_dir(&id).join("claude");
        let ca = Bind { guest: launch::CA_DIR.into(), host: Ca::dir(&manager.data) };
        assert_eq!(
            runtime.binds.lock().unwrap()[1..],
            [Bind { guest: "/opt/zwinger/profile/claude".into(), host: snapshot.clone() }, ca]
        );
        assert!(snapshot.join("skills/review/SKILL.md").is_file());
        assert!(!snapshot.join(".credentials.json").exists());
        let skills = manager.state_dir(&id).join("claude/.claude/skills");
        assert_eq!(std::fs::read_link(skills).unwrap(), Path::new("/opt/zwinger/profile/claude/skills"));

        let view = manager.list().remove(0);
        assert_eq!(view.login_expires_at, Some(NOW + 3_600_000));
        // Neither the host version nor the pinned Claude Code is downloaded.
        assert!(
            view.host_notices.iter().any(|n| n.contains("Claude Code 2.1.310 is not downloaded")),
            "{:?}",
            view.host_notices
        );
        assert!(!view.pending_restart);

        // The profile off: its links and its snapshot go at the next start.
        manager.stop(&id).await.unwrap();
        manager.put_host_agent(&id, uses(AgentKind::Claude, false, true), false).await.unwrap();
        manager.start(&id).await.unwrap();
        assert_eq!(runtime.binds.lock().unwrap().len(), 2, "only the bin dir and the CA");
        assert!(!manager.state_dir(&id).join("claude/.claude/skills").exists());
        assert!(!snapshot.exists());
        manager.stop(&id).await.unwrap();
        manager.delete(&id).await.unwrap();
        assert!(!manager.profile_dir(&id).exists());
    }

    #[tokio::test]
    async fn the_guest_login_has_placeholders_only() {
        let logins = FakeLogins::default();
        let doc = serde_json::json!({
            "tokens": { "id_token": "id.real", "access_token": "at-real", "refresh_token": "rt-real", "account_id": "acc" },
        });
        logins.docs.lock().unwrap().insert(AgentKind::Codex, doc.to_string());
        let (manager, runtime, id, _) = host_manager(logins, NOW);
        manager.put_host_agent(&id, uses(AgentKind::Codex, false, true), false).await.unwrap();
        // A link that the guest planted at the login file is replaced, not written through.
        let outside = temp_dir().join("precious");
        std::fs::write(&outside, "host").unwrap();
        std::fs::create_dir_all(manager.state_dir(&id).join("codex")).unwrap();
        std::os::unix::fs::symlink(&outside, manager.state_dir(&id).join("codex/auth.json")).unwrap();

        manager.start(&id).await.unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "host");
        let auth = std::fs::read_to_string(manager.state_dir(&id).join("codex/auth.json")).unwrap();
        assert!(
            auth.contains("$MSB_CODEX_HOST_LOGIN$") && auth.contains(login::INERT_REFRESH) && auth.contains("id.real")
        );
        assert!(!auth.contains("at-real") && !auth.contains("rt-real"));
        assert_eq!(manager.list()[0].shield.logins[0].hosts, ["chatgpt.com"]);
        assert!(!format!("{:?}", runtime.net.lock().unwrap()).contains("at-real"));
        // The access token here is no JWT, so its expiry is unknown.
        let notices = manager.list().remove(0).host_notices;
        assert_eq!(
            notices,
            [
                "The Codex login of this Mac has no expiry that zwinger can read, so the proxy does not read it again. Restart the shield when the agent reports a login error.",
                "The Linux build of Codex 0.163.0 is not downloaded yet. Download it in the Agents settings, then restart the shield.",
            ]
        );
        // Installed: no download notice.
        manager.stop(&id).await.unwrap();
        runtimes::fake_install(&manager.data, "codex");
        manager.start(&id).await.unwrap();
        assert!(manager.list()[0].host_notices.iter().all(|n| !n.contains("not downloaded")));
        assert_eq!(*runtime.runtimes.lock().unwrap(), ["codex"]);
    }

    #[tokio::test]
    async fn an_expired_or_missing_host_login_fails_the_start() {
        let logins = FakeLogins::default();
        logins.docs.lock().unwrap().insert(AgentKind::Claude, claude_doc("t", NOW - 1));
        let (manager, runtime, id, _) = host_manager(logins, NOW);
        manager.put_host_agent(&id, uses(AgentKind::Claude, false, true), false).await.unwrap();
        assert!(matches!(manager.start(&id).await, Err(Error::HostLoginExpired(_))));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(runtime.log.lock().unwrap().is_empty());
        // pi's login names its provider, so pi needs one to be added with a login.
        assert!(matches!(
            manager.put_host_agent(&id, uses(AgentKind::Pi, false, true), false).await,
            Err(Error::HostLoginMissing(_))
        ));
        // An agent that this Mac does not have.
        let (empty, _, empty_id) = self::manager();
        assert!(matches!(
            empty.put_host_agent(&empty_id, uses(AgentKind::Codex, false, false), false).await,
            Err(Error::HostAgentMissing(_))
        ));
    }

    #[tokio::test]
    async fn pi_copilot_login_blocks_the_github_api_and_gemini_gets_a_placeholder_login() {
        let logins = FakeLogins::default();
        let pi = serde_json::json!({ "github-copilot": {
            "type": "oauth", "access": "tid=1;proxy-ep=proxy.business.githubcopilot.com;", "refresh": "r", "expires": NOW + 3_600_000,
        } });
        logins.docs.lock().unwrap().insert(AgentKind::Pi, pi.to_string());
        let gemini = serde_json::json!({ "access_token": "ya29.real", "refresh_token": "1//real", "expiry_date": NOW + 3_600_000, "scope": "openid" });
        logins.docs.lock().unwrap().insert(AgentKind::Gemini, gemini.to_string());
        let (manager, _, id, _) = host_manager(logins, NOW);

        manager.put_host_agent(&id, uses(AgentKind::Gemini, false, true), true).await.unwrap();
        // A GitHub token on the GitHub API conflicts with pi's Copilot login.
        credential(&manager, &id, "api.github.com", "GITHUB_TOKEN", Some("ghp")).unwrap();
        assert!(matches!(
            manager.put_host_agent(&id, uses(AgentKind::Pi, false, true), true).await,
            Err(Error::RefreshHostConflict(..))
        ));
        let github = manager.list()[0].shield.network.rules[1].id.clone();
        manager.remove_rule(&id, &github).unwrap();
        manager.put_host_agent(&id, uses(AgentKind::Pi, false, true), true).await.unwrap();
        let shield = manager.list().remove(0).shield;
        let hosts: Vec<_> = shield.network.rules.iter().map(|r| r.label()).collect();
        assert_eq!(hosts, ["cloudcode-pa.googleapis.com", "api.business.githubcopilot.com"]);

        manager.start(&id).await.unwrap();
        let creds = std::fs::read_to_string(manager.state_dir(&id).join("gemini/.gemini/oauth_creds.json")).unwrap();
        assert!(
            creds.contains("$MSB_GEMINI_HOST_LOGIN$") && !creds.contains("ya29.real") && !creds.contains("1//real")
        );
        let view = manager.list().remove(0);
        assert!(view.guest_logins.is_empty(), "the placeholder login is not a guest login");
        assert!(
            view.host_notices.iter().any(|n| n.contains("GitHub API (api.github.com) is blocked")),
            "{:?}",
            view.host_notices
        );
    }

    #[tokio::test]
    async fn a_login_with_no_expiry_is_not_read_again() {
        let logins = FakeLogins::default();
        let doc = serde_json::json!({ "claudeAiOauth": { "accessToken": "t" } }).to_string();
        logins.docs.lock().unwrap().insert(AgentKind::Claude, doc);
        let (manager, _, id, _) = host_manager(logins, NOW);
        manager.put_host_agent(&id, uses(AgentKind::Claude, false, true), false).await.unwrap();
        manager.start(&id).await.unwrap();
        let view = manager.list().remove(0);
        assert_eq!(view.login_expires_at, None);
        assert!(view.host_notices.iter().any(|n| n.contains("does not read it again")), "{:?}", view.host_notices);
    }

    #[tokio::test]
    async fn host_js_agents_run_from_the_mac_with_their_fallbacks() {
        let (manager, runtime, id, _) = host_manager(FakeLogins::default(), NOW);
        manager.put_host_agent(&id, uses(AgentKind::Pi, false, false), false).await.unwrap();
        manager.put_host_agent(&id, uses(AgentKind::Gemini, false, false), false).await.unwrap();
        manager.start(&id).await.unwrap();
        let binds: Vec<String> = runtime.binds.lock().unwrap().iter().map(|b| b.guest.clone()).collect();
        assert_eq!(binds, ["/opt/zwinger/bin", "/opt/zwinger/host/pi", "/opt/zwinger/host/gemini", "/opt/zwinger/ca"]);
        assert!(manager.list()[0].runtimes.contains(&"gemini"));
        // The TUI probe: a terminal, with no session file.
        let probe = runtime.argvs.lock().unwrap()[0].clone();
        assert_eq!(probe[0], "env");
        assert!(probe.iter().any(|a| a.starts_with("HTTPS_PROXY=http://zw:")), "{probe:?}");
        assert!(probe.contains(&"NODE_USE_ENV_PROXY=1".to_owned()), "{probe:?}");
        assert_eq!(
            probe[probe.len() - 4..],
            [
                "PI_CODING_AGENT_DIR=/opt/zwinger/state/agent",
                "node",
                "/opt/zwinger/host/pi/dist/cli.js",
                "--no-session"
            ]
        );
        let wrapper = |kind: &str| std::fs::read_to_string(manager.bin_dir(&id).join(kind)).unwrap();
        assert!(wrapper("pi").contains("exec node /opt/zwinger/host/pi/dist/cli.js "));
        assert!(wrapper("gemini").contains("exec node /opt/zwinger/host/gemini/bundle/gemini.js "));
        manager.stop(&id).await.unwrap();

        // A darwin-only native module: pi falls back to the bundle, Gemini CLI has none.
        *runtime.probe_fail.lock().unwrap() =
            vec!["/opt/zwinger/host/pi/dist/cli.js".into(), "/opt/zwinger/host/gemini/bundle/gemini.js".into()];
        manager.start(&id).await.unwrap();
        let view = manager.list().remove(0);
        assert!(!view.runtimes.contains(&"gemini"));
        assert!(view.host_notices.iter().any(|n| n.contains("bundled pi")), "{:?}", view.host_notices);
        assert!(view.host_notices.iter().any(|n| n.contains("darwin-arm64.node")), "{:?}", view.host_notices);
        let gemini = agent(AgentKind::Gemini, false);
        assert!(matches!(manager.open_session(&id, gemini, 24, 80).await, Err(Error::HostAgentFailed(..))));
        assert_eq!(wrappers(&manager, &id), ["pi"]);
        assert!(wrapper("pi").contains("exec node /opt/zwinger/pi/cli.js "));

        // Gemini CLI that is no host agent of the shield.
        let (plain, _, plain_id) = manager_with_host();
        plain.start(&plain_id).await.unwrap();
        assert!(matches!(plain.open_session(&plain_id, gemini, 24, 80).await, Err(Error::NotHostAgent(_))));
    }

    fn manager_with_host() -> (Manager<FakeRuntime>, FakeRuntime, String) {
        let (manager, runtime, id, _) = host_manager(FakeLogins::default(), NOW);
        (manager, runtime, id)
    }

    #[tokio::test]
    async fn each_shield_trusts_the_zwinger_ca_or_says_why_not() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        assert_eq!(*runtime.trusts.lock().unwrap(), 1);
        assert!(manager.list()[0].host_notices.iter().all(|n| !n.contains("Only Node")));
        manager.stop(&id).await.unwrap();

        *runtime.trust_fail.lock().unwrap() = true;
        manager.start(&id).await.unwrap();
        let notices = &manager.list()[0].host_notices;
        assert!(
            notices.iter().any(|n| n.contains("Only Node trusts") && n.contains("no system CA bundle")),
            "{notices:?}"
        );
    }

    /// The hosts of the rules with a credential.
    fn holders(manager: &Manager<FakeRuntime>) -> Vec<String> {
        let rules = manager.list().remove(0).shield.network.rules;
        rules.iter().filter(|r| !r.credentials.is_empty()).map(|r| r.label()).collect()
    }

    #[tokio::test]
    async fn a_keychain_write_past_its_deadline_blocks_the_account_until_it_ends() {
        let dir = temp_dir();
        let slow = Arc::new(secrets::fake::SlowVault::default());
        let vault = Arc::new(secrets::Ordered::alone(slow.clone(), Duration::from_millis(50)));
        let manager = Manager::new(
            FakeRuntime::default(),
            Store::new(dir.join("shields.json")),
            vault.clone(),
            temp_dir(),
            host(),
        )
        .unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        credential(&manager, &id, "old.io", "KEY", Some("v1")).unwrap();
        let new = allow(&manager, &id, "new.io");

        // A new host and a new value: the set passes its deadline.
        slow.hold();
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), value("v2")),
            Err(Error::KeychainTimeout)
        ));
        assert!(matches!(manager.start(&id).await, Err(Error::SecretPending(n)) if n == "KEY"));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), value("v3")),
            Err(Error::SecretPending(_))
        ));
        assert!(matches!(manager.set_rule_credential(&id, &new, "KEY", None, None), Err(Error::SecretPending(_))));

        // The late set runs, then the restore: the old hosts keep the old value.
        slow.release();
        secrets::tests::until_settled(vault.as_ref(), &id, "KEY");
        let account = secrets::account(&id, "KEY");
        let writes: Vec<_> =
            slow.writes.lock().unwrap().iter().map(|(a, v)| (a == &account, v.clone().unwrap())).collect();
        assert_eq!(writes, [(true, "v1".to_owned()), (true, "v2".to_owned()), (true, "v1".to_owned())]);
        assert_eq!(holders(&manager), ["old.io"]);
        manager.start(&id).await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_restore_blocks_the_account_until_the_vault_holds_the_prior_value() {
        let dir = temp_dir();
        let slow = Arc::new(secrets::fake::SlowVault::default());
        let vault = Arc::new(secrets::Ordered::alone(slow.clone(), Duration::from_millis(50)));
        let manager = Manager::new(
            FakeRuntime::default(),
            Store::new(dir.join("shields.json")),
            vault.clone(),
            temp_dir(),
            host(),
        )
        .unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        credential(&manager, &id, "old.io", "KEY", Some("v1")).unwrap();
        let new = allow(&manager, &id, "new.io");

        // The late set of v2 runs; the restore and its fallback delete fail.
        slow.hold();
        *slow.fail_after.lock().unwrap() = Some(2);
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), value("v2")),
            Err(Error::KeychainTimeout)
        ));
        slow.release();
        secrets::tests::until_settled(vault.as_ref(), &id, "KEY");
        assert_eq!(vault.get(&id, "KEY").unwrap().unwrap().expose(), "v2");

        assert!(matches!(manager.start(&id).await, Err(Error::SecretPending(n)) if n == "KEY"));
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), value("v3")),
            Err(Error::SecretPending(_))
        ));

        // The vault works again: the next start restores v1 for the old hosts.
        *slow.fail_after.lock().unwrap() = None;
        manager.start(&id).await.unwrap();
        assert_eq!(vault.get(&id, "KEY").unwrap().unwrap().expose(), "v1");
        assert_eq!(holders(&manager), ["old.io"]);
    }

    #[tokio::test]
    async fn start_refuses_a_folder_swapped_at_the_same_path() {
        let (manager, runtime, id) = manager();
        let folder = manager.list()[0].shield.folder.clone();
        std::fs::rename(&folder, folder.with_extension("was")).unwrap();
        std::fs::create_dir(&folder).unwrap();
        assert!(matches!(manager.start(&id).await, Err(Error::PathChanged(_))));
        assert!(runtime.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_folder_swapped_during_the_boot_stops_the_vm() {
        let (manager, runtime, id) = manager();
        let folder = manager.list()[0].shield.folder.clone();
        let gate = Arc::new(PtyGate::default());
        *runtime.start_gate.lock().unwrap() = Some(gate.clone());
        let manager = Arc::new(manager);
        let starting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.start(&id).await }
        });
        gate.entered.notified().await;
        std::fs::rename(&folder, folder.with_extension("was")).unwrap();
        std::fs::create_dir(&folder).unwrap();
        gate.release.notify_one();
        assert!(matches!(starting.await.unwrap(), Err(Error::PathChanged(_))));
        assert_eq!(*runtime.log.lock().unwrap(), [format!("start {id}"), format!("stop {id}")]);
        assert!(matches!(manager.list()[0].status, Status::Error(_)));
    }

    #[tokio::test]
    async fn stop_all_stops_four_shields_at_a_time() {
        let runtime = FakeRuntime::default();
        let manager =
            Manager::new(runtime.clone(), Store::new(temp_dir().join("shields.json")), vault(), temp_dir(), host())
                .unwrap();
        let ids: Vec<String> = (0..6).map(|_| manager.create(temp_dir().to_str().unwrap()).unwrap()).collect();
        for id in &ids {
            manager.start(id).await.unwrap();
        }
        *runtime.stop_delay.lock().unwrap() = Duration::from_secs(1);
        tokio::time::pause();
        let started = Instant::now();
        manager.stop_all(Duration::from_secs(20)).await;
        assert_eq!(started.elapsed().as_secs(), 2, "two rounds of four and two");
        assert_eq!(runtime.most_stopping.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert!(manager.list().iter().all(|v| v.status == Status::Stopped));
    }

    fn put_codex_login(manager: &Manager<FakeRuntime>, id: &str) -> PathBuf {
        let codex = manager.state_dir(id).join("codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(codex.join("auth.json"), r#"{"tokens":{"access_token":"eyJ"}}"#).unwrap();
        codex.join("auth.json")
    }

    #[tokio::test]
    async fn guest_logins_show_in_the_view_and_can_be_removed() {
        let (manager, _, id) = manager();
        let file = put_codex_login(&manager, &id);
        manager.start(&id).await.unwrap();
        assert_eq!(manager.list()[0].guest_logins, [AgentKind::Codex]);
        manager.remove_guest_login(&id, AgentKind::Codex).unwrap();
        assert!(manager.list()[0].guest_logins.is_empty());
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn the_guest_login_watch_reports_a_new_login() {
        let (manager, _, id) = manager();
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.set_guest_sink(Box::new(move |id| {
            let _ = tx.send(id.to_owned());
        }));
        manager.start(&id).await.unwrap();
        assert!(manager.list()[0].guest_logins.is_empty());
        tokio::time::pause();
        put_codex_login(&manager, &id);
        let reported = tokio::time::timeout(Duration::from_secs(60), rx.recv()).await.unwrap();
        assert_eq!(reported, Some(id.clone()));
        assert_eq!(manager.list()[0].guest_logins, [AgentKind::Codex]);
    }

    /// Runs `script` with `sh -c` in the VM. Returns the exit code and the
    /// stdout; after a failure also the stderr.
    async fn sh<R: SandboxRuntime>(manager: &Manager<R>, vm: &R::Vm, script: &str) -> (i32, String) {
        let argv = ["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()];
        let mut pipe = manager.runtime.open_pipe(vm, &argv, WORKSPACE).await.unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut code = -1;
        while let Some(event) = pipe.events.recv().await {
            match event {
                PipeEvent::Stdout(bytes) => out.extend(bytes),
                PipeEvent::Stderr(bytes) => err.extend(bytes),
                PipeEvent::Exit(exit) => {
                    code = exit;
                    break;
                }
            }
        }
        if code != 0 {
            out.extend(err);
        }
        (code, String::from_utf8_lossy(&out).trim().to_owned())
    }

    /// Boots a real shield. Needs microsandbox and network:
    /// `cargo test -- --ignored live_`.
    #[tokio::test]
    #[ignore]
    async fn live_shield_boots() {
        use crate::runtime::Microsandbox;

        let (folder, readonly, pi, data) = (temp_dir(), temp_dir(), temp_dir(), temp_dir());
        std::fs::write(folder.join("hello.txt"), "from the host").unwrap();
        let store = Store::new(data.join("shields.json"));
        let manager = Manager::new(Microsandbox { pi }, store, vault(), data.clone(), host()).unwrap();
        let id = manager.create(folder.to_str().unwrap()).unwrap();
        manager.add_mount(&id, readonly.to_str().unwrap(), "/mnt/ro", true).unwrap();
        // A credential host, a host with no credential, and the Debian mirror:
        // the image has no curl, so the test installs it.
        credential(&manager, &id, "api.github.com", "ZWINGER_LIVE", Some("live-value")).unwrap();
        allow(&manager, &id, "example.com");
        allow(&manager, &id, "deb.debian.org");
        manager.start(&id).await.unwrap();
        let vm = match lock(&manager.state).vms.get(&id) {
            Some(Vm::Running { vm, .. }) => vm.clone(),
            _ => panic!("the shield does not run"),
        };
        let run = |script: &'static str| sh(&manager, &vm, script);

        assert_eq!(run("cat /workspace/hello.txt").await, (0, "from the host".to_owned()));
        assert_eq!(run("test ! -e /Users").await.0, 0, "/Users is visible");
        assert_ne!(run("echo x > /mnt/ro/f").await.0, 0, "a read-only mount took a write");
        assert!(!readonly.join("f").exists());
        assert_ne!(run("getent hosts example.com").await.0, 0, "a name resolved in the guest");
        let connect = "node -e \"const s = require('net').connect(443, '1.1.1.1'); \
            s.on('connect', () => process.exit(0)); s.on('error', () => process.exit(1)); \
            setTimeout(() => process.exit(1), 5000)\"";
        assert_ne!(run(connect).await.0, 0, "direct egress passed");
        assert_eq!(run("printenv ZWINGER_LIVE").await, (0, "$MSB_ZWINGER_LIVE$".to_owned()));
        assert_eq!(run("bash -lc 'command -v pi'").await, (0, "/opt/zwinger/bin/pi".to_owned()));

        // Node goes through the proxy and trusts the zwinger CA.
        for url in ["https://api.github.com/zen", "https://example.com/"] {
            let script = format!(
                "node -e \"fetch('{url}').then(() => process.exit(0), (e) => {{ console.error(e.cause ?? e); process.exit(1); }})\""
            );
            let (code, out) = sh(&manager, &vm, &script).await;
            assert_eq!(code, 0, "{url}: {out}");
        }
        // The start made a bundle with the proxy CA, although the image has none.
        let trusted = format!("grep -qF \"$(sed -n 2p {})\" {}", launch::CA_CERT, launch::CA_BUNDLE);
        assert_eq!(sh(&manager, &vm, &trusted).await.0, 0, "the bundle has no proxy CA");
        // curl only, without ca-certificates: it must work with that bundle.
        let install = "apt-get update -qq && DEBIAN_FRONTEND=noninteractive \
            apt-get install -qq -y --no-install-recommends curl >/dev/null";
        let (code, out) = run(install).await;
        assert_eq!(code, 0, "install curl: {out}");
        let (code, out) = run("curl -fsS -o /dev/null https://example.com/").await;
        assert_eq!(code, 0, "curl https://example.com/: {out}");
        // The proxy sends the fake credential, so GitHub answers 401 itself.
        let (code, out) = run("curl -sS -o /dev/null -w '%{http_code}' https://api.github.com/zen").await;
        assert_eq!((code, out.as_str()), (0, "401"), "curl https://api.github.com/zen");
        let events = manager.list_net_events(&id).unwrap();
        assert!(events.iter().any(|e| e.host == "api.github.com" && e.credentials == ["ZWINGER_LIVE"]), "{events:?}");
        // The default blocks the rest, with the zwinger reason.
        let (_, out) = run("curl -sS https://www.iana.org/ 2>&1").await;
        assert!(out.contains("403"), "{out}");
        assert!(
            manager.list_net_events(&id).unwrap().iter().any(|e| e.host == "www.iana.org" && e.reason == "default")
        );
        let test = manager.test_network(&id, "example.com").await.unwrap();
        assert!(test.proxy.ok && test.target.is_some_and(|t| t.ok));
        manager.stop(&id).await.unwrap();
    }
}
