use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::agent::{AgentKind, gemini, launch, pi};
use crate::error::{Error, Result};
use crate::host_agents::login::{self, Step};
use crate::host_agents::{self, HostAgents, profile};
use crate::netlog::{self, Dedupe, NetEvent};
use crate::runtime::{
    Bind, PipeEvent, PipeInput, PtyEvent, PtyInput, SandboxRuntime, SessionKind, sandbox_policy, secret_entries,
};
use crate::runtimes;
use crate::secrets::{self, Origin, Places, Secret, SecretValue, Vault};
use crate::shield::{self, AppDraft, HostAgentUse, NetworkMode, NetworkPolicy, Pos, Shield, TaskDraft, WORKSPACE};
use crate::store::Store;
use crate::task_agents::Exec;
use crate::task_artifacts::{self, Mirror, RoundWriter};
use crate::task_runs::{self, Control, RunHandle, RunState};

const NODE_LIMIT: Duration = Duration::from_secs(15);
/// The bytes of `node --version` output that are kept.
const NODE_OUT: usize = 1 << 10;
/// How long a stopped app has to exit before its restart fails.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);
/// The network events kept per shield.
const NET_RING: usize = 500;
/// The output bytes kept per app run, for a view that attaches later.
const APP_RING: usize = 256 << 10;
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
/// The first and the longest wait before a due host login is checked again.
const RENEW_RETRY: Duration = Duration::from_secs(60);
const RENEW_RETRY_MAX: Duration = Duration::from_secs(10 * 60);

/// Gets each new network event with its shield id.
pub type NetSink = Box<dyn Fn(&str, &NetEvent) + Send + Sync>;
/// Gets the id of a shield whose host login is due for renewal.
pub type RenewSink = Box<dyn Fn(&str) + Send + Sync>;

/// Owns the shield configs, the running VMs and their terminal sessions.
pub struct Manager<R: SandboxRuntime> {
    runtime: R,
    store: Store,
    /// The app data dir. It holds the agent runtimes, `shields/<id>`, the
    /// agent state of each shield, and `bin/<id>`, the host-only agent
    /// wrappers of each shield.
    data: PathBuf,
    vault: Arc<dyn Vault>,
    /// Serializes the secret edits, so that a rollback cannot undo a later edit.
    /// The state lock is not held during vault calls.
    secret_edit: Mutex<()>,
    state: Arc<Mutex<State<R::Vm>>>,
    net_sink: Arc<OnceLock<NetSink>>,
    runs: Arc<task_runs::Registry>,
    /// Serializes run starts, so two starts cannot take the same run name.
    run_start: Mutex<()>,
    /// The stage limits of new task runs.
    run_config: task_runs::Config,
    host: Arc<HostAgents>,
    renew_sink: Arc<OnceLock<RenewSink>>,
}

struct State<V> {
    shields: Vec<Shield>,
    vms: HashMap<String, Vm<V>>,
    sessions: Vec<Session>,
    /// Per shield, counts the secret value changes. The config does not hold
    /// the values, so a running VM compares this count instead.
    secrets_rev: HashMap<String, u64>,
    /// Per shield, the last network events. They stay after a stop, so the
    /// user can still allow a blocked host.
    net: HashMap<String, VecDeque<NetEvent>>,
    /// Per shield and app, counts the app stops. A start that saw an older
    /// count was stopped while its process opened.
    app_stops: HashMap<(String, String), u64>,
}

enum Vm<V> {
    Starting,
    /// `applied` is the config the VM booted with; it differs from the stored
    /// config after an edit, until the next start. `applied_rev` is the
    /// `secrets_rev` that the start read before it read the values.
    /// `net` follows the VM's network events. It is `None` if the runtime
    /// log could not be opened. `runtimes` are the agent runtimes that the
    /// start mounted.
    Running {
        vm: Arc<V>,
        applied: Box<Shield>,
        applied_rev: u64,
        net: Option<AbortHandle>,
        runtimes: Vec<&'static str>,
        host: HostRun,
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
    /// The expiry of each host login at the start, in ms since the epoch.
    /// A login with no expiry is not here: it is not renewed.
    logins: Vec<(AgentKind, i64)>,
    /// The earliest expiry of the host logins, in ms since the epoch.
    login_expires: Option<i64>,
    /// The renewal of each login in `logins`. A stop aborts them.
    renew: Vec<AbortHandle>,
    /// The Mac has a newer host login: a restart uses it.
    renew_due: bool,
    /// What the user should know, such as a fallback.
    notices: Vec<String>,
    /// The host agents that do not run in the VM, with the reason.
    failed: Vec<(AgentKind, String)>,
}

impl HostRun {
    fn found(&self) -> launch::Found<'_> {
        launch::Found {
            pi_host: self.pi_host,
            gemini: self.gemini.as_deref(),
        }
    }
}

struct Session {
    id: String,
    shield_id: String,
    kind: SessionKind,
    input: UnboundedSender<PtyInput>,
    exit_code: Option<i32>,
    /// Set for an app run. The backend owns it, not a view.
    app: Option<AppRun>,
}

struct AppRun {
    app_id: String,
    output: Arc<Mutex<AppOutput>>,
    /// Becomes `true` when the process has exited.
    exited: watch::Receiver<bool>,
}

/// The kept output of an app run and the view that gets new output.
#[derive(Default)]
struct AppOutput {
    ring: VecDeque<u8>,
    exit: Option<i32>,
    /// The view that gets new output, with the token of its attach.
    sink: Option<(u64, PtySink)>,
    attaches: u64,
}

type PtySink = Box<dyn Fn(PtyEvent) + Send>;

impl AppOutput {
    fn push(&mut self, event: PtyEvent) {
        match &event {
            PtyEvent::Output(bytes) => {
                self.ring.extend(bytes);
                let excess = self.ring.len().saturating_sub(APP_RING);
                self.ring.drain(..excess);
            }
            PtyEvent::Exit(code) => self.exit = Some(*code),
        }
        if let Some((_, sink)) = &self.sink {
            sink(event);
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
    /// The earliest expiry of the host logins of the running VM, in ms.
    pub login_expires_at: Option<i64>,
    /// A host login is due for renewal: a restart reads the new token.
    pub renew_due: bool,
    /// What the start of the running VM did differently for the host
    /// agents, such as a fallback.
    pub host_notices: Vec<String>,
}

/// A secret as the UI sees it. It has no value, only if the vault has one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretView {
    pub name: String,
    pub hosts: Vec<String>,
    pub places: Places,
    pub placeholder: String,
    pub set: bool,
    pub origin: Origin,
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
    let configured: HashSet<String> = shields
        .iter()
        .flat_map(|shield| {
            let user = shield.secrets.iter().filter(|s| s.origin.is_user());
            user.map(|s| secrets::account(&shield.id, &s.name))
        })
        .collect();
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
    /// An agent session or an app runs in the shield. The task runs are in
    /// the run registry.
    fn is_busy(&self, id: &str) -> bool {
        self.sessions.iter().any(|s| {
            s.shield_id == id && s.exit_code.is_none() && (s.app.is_some() || matches!(s.kind, SessionKind::Agent { .. }))
        })
    }

    fn secrets_rev(&self, id: &str) -> u64 {
        self.secrets_rev.get(id).copied().unwrap_or(0)
    }

    fn bump_secrets_rev(&mut self, id: &str) {
        *self.secrets_rev.entry(id.to_owned()).or_default() += 1;
    }

    fn shield_mut(&mut self, id: &str) -> Result<&mut Shield> {
        self.shields
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(Error::ShieldNotFound)
    }

    /// Numbers the event and adds it to the shield's ring.
    fn push_net(&mut self, id: &str, mut event: NetEvent) -> NetEvent {
        let ring = self.net.entry(id.to_owned()).or_default();
        event.seq = ring.back().map_or(1, |last| last.seq + 1);
        if ring.len() == NET_RING {
            ring.pop_front();
        }
        ring.push_back(event.clone());
        event
    }

    fn session(&self, id: &str) -> Result<&Session> {
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .ok_or(Error::SessionNotFound)
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
            secret_edit: Mutex::new(()),
            state: Arc::new(Mutex::new(State {
                shields,
                vms: HashMap::new(),
                sessions: Vec::new(),
                secrets_rev: HashMap::new(),
                net: HashMap::new(),
                app_stops: HashMap::new(),
            })),
            net_sink: Arc::new(OnceLock::new()),
            runs: Arc::default(),
            run_start: Mutex::new(()),
            run_config: task_runs::Config::default(),
            host,
            renew_sink: Arc::new(OnceLock::new()),
        })
    }

    /// Only the first sink counts.
    pub fn set_net_sink(&self, sink: NetSink) {
        let _ = self.net_sink.set(sink);
    }

    /// Only the first sink counts.
    pub fn set_renew_sink(&self, sink: RenewSink) {
        let _ = self.renew_sink.set(sink);
    }

    pub fn list_net_events(&self, id: &str) -> Result<Vec<NetEvent>> {
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
                    // A new name, layout or app list is used at once, so it
                    // needs no restart.
                    Some(Vm::Running {
                        applied, applied_rev, ..
                    }) => (
                        Status::Running,
                        Shield {
                            name: shield.name.clone(),
                            layout: shield.layout.clone(),
                            apps: shield.apps.clone(),
                            tasks: shield.tasks.clone(),
                            auto_renew: shield.auto_renew,
                            agents_unprompted: shield.agents_unprompted,
                            ..(**applied).clone()
                        } != *shield
                            || *applied_rev != state.secrets_rev(&shield.id),
                    ),
                    Some(Vm::Stopping) => (Status::Stopping, false),
                    Some(Vm::Failed(message)) => (Status::Error(message.clone()), false),
                };
                let sessions = state.sessions.iter().filter(|s| s.shield_id == shield.id);
                let (runtimes, login_expires_at, renew_due, host_notices) = match state.vms.get(&shield.id) {
                    Some(Vm::Running { runtimes, host, .. }) => {
                        (runtimes.clone(), host.login_expires, host.renew_due, host.notices.clone())
                    }
                    _ => (Vec::new(), None, false, Vec::new()),
                };
                ShieldView {
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
                    renew_due,
                    host_notices,
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

    /// Applies `edit` to the stored config and persists it. The change is kept
    /// only if the save succeeds. A running VM keeps its applied config until
    /// the next start.
    fn edit(&self, id: &str, edit: impl FnOnce(&mut Shield) -> Result<()>) -> Result<()> {
        let mut state = lock(&self.state);
        let mut shields = state.shields.clone();
        let shield = shields
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(Error::ShieldNotFound)?;
        edit(shield)?;
        shield.check()?;
        self.store.save(&shields)?;
        state.shields = shields;
        Ok(())
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

    pub fn set_network_mode(&self, id: &str, mode: NetworkMode) -> Result<()> {
        self.edit(id, |s| {
            s.network.mode = mode;
            Ok(())
        })
    }

    /// The domain must also be valid for the sandbox, so that a later start
    /// in List mode cannot fail on it.
    pub fn add_domain(&self, id: &str, domain: &str) -> Result<()> {
        self.edit(id, |s| {
            s.network.add_domain(domain)?;
            sandbox_policy(
                &NetworkPolicy {
                    mode: NetworkMode::List,
                    domains: s.network.domains.clone(),
                },
                &[],
            )?;
            Ok(())
        })
    }

    /// Allows a blocked host in one edit. A shield in Deny mode changes to
    /// List mode, else the host would stay blocked.
    pub fn allow_host(&self, id: &str, host: &str) -> Result<()> {
        self.edit(id, |s| {
            if s.network.mode == NetworkMode::Deny {
                s.network.mode = NetworkMode::List;
            }
            s.network.add_domain(host)?;
            sandbox_policy(
                &NetworkPolicy {
                    mode: NetworkMode::List,
                    domains: s.network.domains.clone(),
                },
                &[],
            )?;
            Ok(())
        })
    }

    pub fn remove_domain(&self, id: &str, domain: &str) -> Result<()> {
        self.edit(id, |s| {
            s.network.remove_domain(domain);
            Ok(())
        })
    }

    /// Adds or replaces the secret. A new secret needs a value; `None` keeps
    /// the current value. The vault is written before the config, and a failed
    /// config save restores the vault.
    pub fn put_secret(&self, id: &str, secret: Secret, value: Option<SecretValue>) -> Result<()> {
        if !secret.origin.is_user() {
            return Err(Error::HostLoginSecret(secret.name));
        }
        let secret = secret.normalize()?;
        if value.as_ref().is_some_and(|v| v.expose().is_empty()) {
            return Err(Error::SecretValueRequired(secret.name));
        }
        let _edit = lock(&self.secret_edit);
        let is_new = {
            let state = lock(&self.state);
            let mut shield = state
                .shields
                .iter()
                .find(|s| s.id == id)
                .ok_or(Error::ShieldNotFound)?
                .clone();
            if shield.secrets.iter().any(|s| s.name == secret.name && !s.origin.is_user()) {
                return Err(Error::HostLoginSecret(secret.name));
            }
            let is_new = shield.put_secret(secret.clone());
            shield.check()?;
            is_new
        };
        let Some(value) = value else {
            if is_new {
                return Err(Error::SecretValueRequired(secret.name));
            }
            return self.edit(id, |s| {
                s.put_secret(secret);
                Ok(())
            });
        };

        let prior = self.vault.get(id, &secret.name)?;
        // After each write: a start that reads the count first and then the
        // vault either reads the new value, or later sees a changed count.
        self.vault.set(id, &secret.name, &value)?;
        lock(&self.state).bump_secrets_rev(id);
        let name = secret.name.clone();
        let saved = self.edit(id, |s| {
            s.put_secret(secret);
            Ok(())
        });
        if let Err(e) = saved {
            let restored = match &prior {
                Some(prior) => self.vault.set(id, &name, prior),
                None => self.vault.delete(id, &name),
            };
            // The old hosts must never get the new value. If the old value
            // cannot come back, remove the value: the secret is then missing.
            if restored.is_err() {
                let _ = self.vault.delete(id, &name);
            }
            lock(&self.state).bump_secrets_rev(id);
            return Err(e);
        }
        Ok(())
    }

    /// Adds the host agent to the shield, or changes it: its runtime, and
    /// its profile and host login if `used` asks for them. A login adds the
    /// agent's host-login secret; `allow_hosts` also adds its provider host to
    /// the allow list, which the user confirmed. Applies on the next start.
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
        let login = login.map(Secret::normalize).transpose()?;
        let _edit = lock(&self.secret_edit);
        self.edit(id, |s| {
            if let Some(secret) = &login {
                if s.secrets.iter().any(|x| x.name == secret.name && x.origin.is_user()) {
                    return Err(Error::DuplicateSecret(secret.name.clone()));
                }
                if allow_hosts {
                    if s.network.mode == NetworkMode::Deny {
                        s.network.mode = NetworkMode::List;
                    }
                    for host in &secret.hosts {
                        s.network.add_domain(host)?;
                    }
                }
            }
            s.put_host_agent(used, login);
            Ok(())
        })
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

    /// Needs no restart.
    pub fn set_auto_renew(&self, id: &str, on: bool) -> Result<()> {
        self.edit(id, |s| {
            s.auto_renew = on;
            Ok(())
        })
    }

    /// Saves the config first, so that a failure leaves a value with no
    /// secret, not a secret with no value.
    pub fn remove_secret(&self, id: &str, name: &str) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        self.edit(id, |s| {
            if s.secrets.iter().any(|x| x.name == name && !x.origin.is_user()) {
                return Err(Error::HostLoginSecret(name.to_owned()));
            }
            s.remove_secret(name)
        })?;
        lock(&self.state).bump_secrets_rev(id);
        self.vault.delete(id, name)
    }

    pub fn list_secrets(&self, id: &str) -> Result<Vec<SecretView>> {
        let secrets = lock(&self.state).shield_mut(id)?.secrets.clone();
        let accounts: HashSet<String> = self.vault.accounts()?.into_iter().collect();
        Ok(secrets
            .into_iter()
            .map(|secret| SecretView {
                set: !secret.origin.is_user() || accounts.contains(&secrets::account(id, &secret.name)),
                placeholder: secret.placeholder(),
                name: secret.name,
                hosts: secret.hosts,
                places: secret.places,
                origin: secret.origin,
            })
            .collect())
    }

    /// Saves the config first, then deletes the secret values, the agent
    /// state, the wrappers and the old chats. The first error is returned after all deletes ran.
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.stop(id).await.or_else(|e| match e {
            Error::NotRunning => Ok(()),
            e => Err(e),
        })?;
        let _edit = lock(&self.secret_edit);
        let secrets = {
            let mut state = lock(&self.state);
            let secrets = state.shield_mut(id)?.secrets.clone();
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
            state.secrets_rev.remove(id);
            state.net.remove(id);
            secrets
        };
        let mut result = Ok(());
        for secret in &secrets {
            if let Err(e) = self.vault.delete(id, &secret.name)
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

    /// The config snapshot and the vault reads hold `secret_edit`, so a start
    /// never pairs a new value with old hosts. The lock is released before the
    /// host logins are read (the Keychain can wait for the macOS prompt) and
    /// before the runtime start.
    pub async fn start(&self, id: &str) -> Result<()> {
        let (shield, applied_rev, values) = {
            let _edit = lock(&self.secret_edit);
            let (shield, applied_rev) = self.claim_start(id)?;
            match self.vault_values(&shield) {
                Ok(values) => (shield, applied_rev, values),
                Err(e) => {
                    lock(&self.state).vms.remove(id);
                    return Err(e);
                }
            }
        };
        let prepared = {
            let (host, data, shield) = (self.host.clone(), self.data.clone(), shield.clone());
            blocking(move || prepare(&host, &data, &shield, values)).await
        };
        let (entries, prepared) = match prepared
            .and_then(|mut p| Ok((secret_entries(&shield.secrets, std::mem::take(&mut p.values))?, p)))
        {
            Ok(ready) => ready,
            Err(e) => {
                lock(&self.state).vms.remove(id);
                return Err(e);
            }
        };
        let Prepared { binds, wanted, mut run, .. } = prepared;
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
        let result = match self.runtime.start(&shield, entries, &self.state_dir(id), &installed, &binds).await {
            Ok(vm) => match self.check_node(&vm).await {
                Ok(()) => Ok(vm),
                Err(e) => {
                    let _ = self.runtime.stop(&vm).await;
                    Err(e)
                }
            },
            Err(e) => Err(e),
        };

        let mut names: Vec<&'static str> = installed.iter().map(|r| r.name).collect();
        if let Ok(vm) = &result {
            if run.pi_host && let Err(e) = self.probe_tui(vm).await {
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
                    run.renew = run.logins.iter().map(|&(kind, started)| self.watch_renewal(id, &vm, kind, started)).collect();
                    state.vms.insert(
                        id.to_owned(),
                        Vm::Running {
                            vm,
                            applied: Box::new(shield),
                            applied_rev,
                            net,
                            runtimes: names,
                            host: run,
                        },
                    );
                    let apps = state.shield_mut(id).map(|s| s.apps.clone()).unwrap_or_default();
                    apps.into_iter().filter(|a| a.autostart).map(|a| a.id).collect()
                }
                Err(e) => {
                    state.vms.insert(id.to_owned(), Vm::Failed(e.to_string()));
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

    /// Renews one host login while the VM runs. When the login is due, it
    /// reads the login on the Mac again and follows [`login::renew_step`]:
    /// a newer login marks the VM and calls the renew sink, which restarts
    /// the shield if that may happen; no newer login shows the notice to
    /// open the agent on the Mac. It checks again on a backoff. It ends
    /// with the VM: a stop aborts it, and a new VM ends it.
    fn watch_renewal(&self, id: &str, vm: &Arc<R::Vm>, kind: AgentKind, started: i64) -> AbortHandle {
        let (state, runs, host, sink) = (self.state.clone(), self.runs.clone(), self.host.clone(), self.renew_sink.clone());
        let (id, vm) = (id.to_owned(), vm.clone());
        tokio::spawn(async move {
            let mut retry = RENEW_RETRY;
            loop {
                let now = host.now_ms();
                // The Keychain read can wait for the macOS prompt.
                let current = if now < started - login::RENEW_BEFORE_MS {
                    None
                } else {
                    let host = host.clone();
                    tokio::task::spawn_blocking(move || host.token(kind)).await.ok().and_then(|t| t.ok()?.expires)
                };
                let (step, changed) = {
                    let mut state = lock(&state);
                    let auto = state.shields.iter().any(|s| s.id == id && s.auto_renew);
                    let busy = state.is_busy(&id) || runs.has_live(&id);
                    let Some(Vm::Running { vm: running, host: run, .. }) = state.vms.get_mut(&id) else {
                        return;
                    };
                    if !Arc::ptr_eq(running, &vm) {
                        return;
                    }
                    let step = login::renew_step(started, current, now, busy, auto);
                    let label = kind.label();
                    let notice = format!(
                        "The {label} login expires soon and this Mac has no newer one. Open {label} on this Mac once to refresh it."
                    );
                    let changed = match step {
                        Step::Wait(_) => false,
                        Step::ExpiresSoon => {
                            let new = !run.notices.contains(&notice);
                            if new {
                                run.notices.push(notice);
                            }
                            new
                        }
                        // The Mac has a newer login now: the notice is stale.
                        Step::Restart | Step::Notify | Step::Retry => {
                            let count = run.notices.len();
                            run.notices.retain(|n| *n != notice);
                            let removed = run.notices.len() != count;
                            let due = std::mem::replace(&mut run.renew_due, true);
                            step == Step::Restart || removed || !due
                        }
                    };
                    (step, changed)
                };
                if changed && let Some(sink) = sink.get() {
                    sink(&id);
                }
                let wait = match step {
                    Step::Wait(wait) => wait,
                    _ => {
                        let wait = retry;
                        retry = (retry * 2).min(RENEW_RETRY_MAX);
                        wait
                    }
                };
                tokio::time::sleep(wait).await;
            }
        })
        .abort_handle()
    }

    /// The Mac has a newer host login for the shield: restarts the shield if
    /// the user turned that on and no agent turn, app or task run works in
    /// it. A new host token applies only to a new VM.
    pub async fn renew(&self, id: &str) -> Result<()> {
        {
            let state = lock(&self.state);
            let shield = state.shields.iter().find(|s| s.id == id).ok_or(Error::ShieldNotFound)?;
            if !matches!(state.vms.get(id), Some(Vm::Running { host, .. }) if host.renew_due) {
                return Ok(());
            }
            if !shield.auto_renew || state.is_busy(id) || self.runs.has_live(id) {
                return Ok(());
            }
        }
        self.stop(id).await?;
        self.start(id).await
    }

    /// Turns the runtime log lines into the shield's events.
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
                if !dedupe.keep(&event) {
                    continue;
                }
                let event = lock(&state).push_net(&id, event);
                if let Some(sink) = sink.get() {
                    sink(&id, &event);
                }
            }
        })
        .abort_handle()
    }

    /// Checks the shield, marks it as starting and returns its config.
    fn claim_start(&self, id: &str) -> Result<(Shield, u64)> {
        let mut state = lock(&self.state);
        let shield = state.shield_mut(id)?.clone();
        match state.vms.get(id) {
            Some(Vm::Starting) => return Err(Error::Starting),
            Some(Vm::Running { .. }) => return Err(Error::AlreadyStarted),
            Some(Vm::Stopping) => return Err(Error::Stopping),
            None | Some(Vm::Failed(_)) => {}
        }
        shield.verify_paths()?;
        shield.check_app_data(&self.data)?;
        shield.check_overlap_with(state.shields.iter().filter(|s| s.id != id))?;
        state.vms.insert(id.to_owned(), Vm::Starting);
        Ok((shield, state.secrets_rev(id)))
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

    /// The vault value of each user secret. A host login has `None`: its
    /// value comes from the Mac.
    fn vault_values(&self, shield: &Shield) -> Result<Vec<Option<SecretValue>>> {
        shield
            .secrets
            .iter()
            .map(|secret| match secret.origin {
                Origin::HostLogin { .. } => Ok(None),
                Origin::User => match self.vault.get(&shield.id, &secret.name)? {
                    Some(value) => Ok(Some(value)),
                    None => Err(Error::SecretMissing(secret.name.clone())),
                },
            })
            .collect()
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
    async fn probe_tui(&self, vm: &R::Vm) -> std::result::Result<(), String> {
        let mut argv = launch::command(AgentKind::Pi, launch::Found { pi_host: true, gemini: None }).expect("pi has a program");
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
        let (vm, applied, applied_rev, net, runtimes, host) = {
            let mut state = lock(&self.state);
            state.shield_mut(id)?;
            match state.vms.get(id) {
                Some(Vm::Running { .. }) => {}
                Some(Vm::Starting) => return Err(Error::Starting),
                Some(Vm::Stopping) => return Err(Error::Stopping),
                Some(Vm::Failed(_)) | None => return Err(Error::NotRunning),
            }
            let Some(Vm::Running {
                vm,
                applied,
                applied_rev,
                net,
                runtimes,
                host,
            }) = state.vms.insert(id.to_owned(), Vm::Stopping)
            else {
                unreachable!("checked above");
            };
            state.sessions.retain(|s| {
                if s.shield_id == id {
                    let _ = s.input.send(PtyInput::Kill);
                }
                s.shield_id != id
            });
            (vm, applied, applied_rev, net, runtimes, host)
        };

        let result = self.runtime.stop(&vm).await;

        let mut state = lock(&self.state);
        match result {
            Ok(()) => {
                state.vms.remove(id);
                for task in net.iter().chain(&host.renew) {
                    task.abort();
                }
            }
            // The follower stays with the VM that still runs.
            Err(_) => {
                state.vms.insert(
                    id.to_owned(),
                    Vm::Running {
                        vm,
                        applied,
                        applied_rev,
                        net,
                        runtimes,
                        host,
                    },
                );
            }
        }
        result
    }

    /// Stops the running shields one after another. `limit` bounds the total
    /// time, so app exit cannot hang on a stuck VM.
    pub async fn stop_all(&self, limit: Duration) {
        let running: Vec<String> = lock(&self.state)
            .vms
            .iter()
            .filter(|(_, vm)| matches!(vm, Vm::Running { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        let _ = tokio::time::timeout(limit, async {
            for id in &running {
                let _ = self.stop(id).await;
            }
        })
        .await;
    }

    /// Streams the session's output and its exit to `on_event`. The exit code is
    /// recorded before the `Exit` event is delivered, so a listing done in reaction
    /// to it already shows the session as exited.
    pub async fn open_session(
        &self,
        shield_id: &str,
        kind: SessionKind,
        rows: u16,
        cols: u16,
        on_event: impl Fn(PtyEvent) + Send + 'static,
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
        let mut pty = self.runtime.open_pty(&vm, &kind.argv(), &cwd, rows, cols).await?;
        let id = uuid::Uuid::new_v4().to_string();
        {
            // The shield may have stopped, or stopped and started again, while
            // the process opened. Its stop did not see this session, so kill it.
            let mut state = lock(&self.state);
            let same_vm = matches!(
                state.vms.get(shield_id),
                Some(Vm::Running { vm: current, .. }) if Arc::ptr_eq(current, &vm)
            );
            if !same_vm {
                let _ = pty.input.send(PtyInput::Kill);
                return Err(Error::NotRunning);
            }
            state.sessions.push(Session {
                id: id.clone(),
                shield_id: shield_id.to_owned(),
                kind,
                input: pty.input,
                exit_code: None,
                app: None,
            });
        }

        let state = self.state.clone();
        let session_id = id.clone();
        tokio::spawn(async move {
            while let Some(event) = pty.events.recv().await {
                if let PtyEvent::Exit(code) = event
                    && let Some(s) = lock(&state).sessions.iter_mut().find(|s| s.id == session_id)
                {
                    s.exit_code = Some(code);
                }
                on_event(event);
            }
        });
        Ok(id)
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
        let mut pty = self.runtime.open_pty(&vm, &argv, &app.cwd, rows, cols).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let output = Arc::new(Mutex::new(AppOutput::default()));
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
                app: Some(AppRun {
                    app_id: app_id.to_owned(),
                    output: output.clone(),
                    exited,
                }),
            });
        }

        let state = self.state.clone();
        let session_id = id.clone();
        tokio::spawn(async move {
            while let Some(event) = pty.events.recv().await {
                if let PtyEvent::Exit(code) = event
                    && let Some(s) = lock(&state).sessions.iter_mut().find(|s| s.id == session_id)
                {
                    s.exit_code = Some(code);
                }
                let exit = matches!(event, PtyEvent::Exit(_));
                lock(&output).push(event);
                if exit {
                    let _ = exited_tx.send(true);
                }
            }
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
        *state
            .app_stops
            .entry((shield_id.to_owned(), app_id.to_owned()))
            .or_default() += 1;
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

    /// Sends the kept output of the app run to `on_event`, and its exit if it
    /// has exited; then new output. A later attach takes over. Returns the
    /// token for [`Manager::detach_app`].
    pub fn attach_app(&self, session_id: &str, on_event: impl Fn(PtyEvent) + Send + 'static) -> Result<u64> {
        let output = self.app_output(session_id)?;
        let mut output = lock(&output);
        on_event(PtyEvent::Output(output.ring.iter().copied().collect()));
        if let Some(code) = output.exit {
            on_event(PtyEvent::Exit(code));
        }
        output.attaches += 1;
        let token = output.attaches;
        output.sink = Some((token, Box::new(on_event)));
        Ok(token)
    }

    /// Stops the output to the view of `token`. A later attach keeps its sink,
    /// and an ended run is not an error.
    pub fn detach_app(&self, session_id: &str, token: u64) {
        let Ok(output) = self.app_output(session_id) else {
            return;
        };
        let mut output = lock(&output);
        if output.sink.as_ref().is_some_and(|(t, _)| *t == token) {
            output.sink = None;
        }
    }

    fn app_output(&self, session_id: &str) -> Result<Arc<Mutex<AppOutput>>> {
        let state = lock(&self.state);
        let run = state.session(session_id)?.app.as_ref().ok_or(Error::SessionNotFound)?;
        Ok(run.output.clone())
    }

    pub fn write(&self, id: &str, data: Vec<u8>) -> Result<()> {
        self.send(id, PtyInput::Data(data))
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<()> {
        self.send(id, PtyInput::Resize { rows, cols })
    }

    fn send(&self, id: &str, input: PtyInput) -> Result<()> {
        lock(&self.state)
            .session(id)?
            .input
            .send(input)
            .map_err(|_| Error::SessionClosed)
    }

    /// An unknown id is not an error: a stop already removed the session.
    pub fn close_session(&self, id: &str) -> Result<()> {
        let mut state = lock(&self.state);
        if let Ok(session) = state.session(id) {
            let _ = session.input.send(PtyInput::Kill);
        }
        state.sessions.retain(|s| s.id != id);
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
        let (task, vm, applied, pi_host, gemini, unmounted) = {
            let mut state = lock(&self.state);
            let task = state.shield_mut(id)?.task(task_id)?.clone();
            match state.vms.get(id) {
                Some(Vm::Running { vm, applied, runtimes, host, .. }) => {
                    let unmounted = task
                        .agents
                        .iter()
                        .find(|a| a.kind != AgentKind::Pi && !runtimes.contains(&a.kind.name()))
                        .map(|a| self.unmounted(a.kind, host));
                    (task, vm.clone(), applied.clone(), host.pi_host, host.gemini.clone(), unmounted)
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
    /// The value of each secret, in the order of the config.
    values: Vec<SecretValue>,
    binds: Vec<Bind>,
    /// The host version of each native runtime that a host agent uses.
    wanted: HashMap<&'static str, String>,
    run: HostRun,
}

/// Copies the profiles and links them into the agents' state, reads the
/// host logins and writes the guest logins, and picks the host packages and
/// runtime versions. `vault` has the value of each user secret. It reads the
/// Keychain and copies files, so it runs on a blocking thread. The state dir
/// is guest-writable: each write there follows no link.
fn prepare(host: &HostAgents, data: &Path, shield: &Shield, vault: Vec<Option<SecretValue>>) -> Result<Prepared> {
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
    let mut binds = vec![Bind {
        guest: launch::BIN_DIR.to_owned(),
        host: bin,
    }];
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
            binds.push(Bind {
                guest: profile::guest_dir(kind),
                host: snapshot_dir,
            });
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
                _ => run.notices.push(format!("{} is not installed on this Mac, so the shield uses its own copy.", kind.label())),
            }
            continue;
        };
        match (kind, found.package.clone(), found.entry.as_deref()) {
            (AgentKind::Claude | AgentKind::Codex, ..) => {
                wanted.insert(kind.name(), found.version.clone());
            }
            (AgentKind::Pi, Some(package), _) => {
                binds.push(Bind {
                    guest: pi::HOST_DIR.to_owned(),
                    host: package,
                });
                run.pi_host = true;
            }
            (AgentKind::Gemini, Some(package), Some(entry)) => {
                binds.push(Bind {
                    guest: gemini::DIR.to_owned(),
                    host: package,
                });
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
    let mut values = Vec::new();
    for (secret, value) in shield.secrets.iter().zip(vault) {
        let Origin::HostLogin { agent } = secret.origin else {
            values.extend(value);
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
        match token.expires {
            Some(expires) => run.logins.push((agent, expires)),
            None => run.notices.push(format!(
                "The {} login of this Mac has no expiry that zwinger can read, so the shield does not renew it. \
                 Restart the shield when the agent reports a login error.",
                agent.label()
            )),
        }
        values.push(token.access);
    }
    run.login_expires = run.logins.iter().map(|&(_, expires)| expires).min();
    if login::denied_hosts(&shield.secrets).contains(&login::GITHUB_API) {
        run.notices.push(format!(
            "The GitHub API ({}) is blocked in this shield: pi's Copilot login is refreshed there.",
            login::GITHUB_API
        ));
    }
    Ok(Prepared {
        values,
        binds,
        wanted,
        run,
    })
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
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::sync::mpsc;

    use super::*;
    use crate::runtime::fake::{FakeRuntime, PtyGate};
    use crate::host_agents::fake::{FakeClock, FakeFs};
    use crate::host_agents::login::fake::{FakeLogins, claude_doc};
    use crate::secrets::fake::MemoryVault;
    use crate::secrets::tests::secret;

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
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        (manager, runtime, id, vault)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<PtyEvent>) -> PtyEvent {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    fn output(text: &str) -> PtyEvent {
        PtyEvent::Output(text.as_bytes().to_vec())
    }

    #[tokio::test]
    async fn delete_refuses_a_shield_that_is_starting() {
        let (manager, _runtime, id, vault) = manager_with_vault();
        manager.put_secret(&id, secret("TOKEN", &["api.example.com"]), Some(SecretValue::new("v"))).unwrap();
        lock(&manager.state).vms.insert(id.clone(), Vm::Starting);
        assert!(matches!(manager.delete(&id).await, Err(Error::Starting)));
        assert_eq!(manager.list().len(), 1);
        assert!(vault.get(&id, "TOKEN").unwrap().is_some());
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let (manager, runtime, id) = manager();
        assert!(matches!(
            manager.open_session(&id, SessionKind::Shell, 24, 80, |_| {}).await,
            Err(Error::NotRunning)
        ));

        manager.start(&id).await.unwrap();
        assert_eq!(manager.list()[0].status, Status::Running);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = manager
            .open_session(&id, SessionKind::Shell, 24, 80, move |e| {
                let _ = tx.send(e);
            })
            .await
            .unwrap();
        assert_eq!(next(&mut rx).await, output("24x80"));

        manager.write(&session, b"ls\r".to_vec()).unwrap();
        assert_eq!(next(&mut rx).await, output("ls\r"));

        manager.resize(&session, 40, 120).unwrap();
        assert_eq!(next(&mut rx).await, output("40x120"));

        manager.send(&session, PtyInput::Kill).unwrap();
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
        assert_eq!(manager.list()[0].sessions[0].exit_code, Some(137));

        manager.close_session(&session).unwrap();
        assert!(manager.list()[0].sessions.is_empty());
        assert!(matches!(
            manager.write(&session, b"x".to_vec()),
            Err(Error::SessionNotFound)
        ));

        manager.stop(&id).await.unwrap();
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert_eq!(
            *runtime.log.lock().unwrap(),
            [format!("start {id}"), "kill".into(), format!("stop {id}")]
        );
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
            async move { manager.open_session(&id, SessionKind::Shell, 24, 80, |_| {}).await }
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

    #[tokio::test]
    async fn agent_sessions_run_the_wrappers_of_the_mounted_agents() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        // Before any session: the profile file, and the bin dir bound read-only.
        assert_eq!(*runtime.profiles.lock().unwrap(), [launch::profile_argv()]);
        assert_eq!(
            *runtime.binds.lock().unwrap(),
            [Bind { guest: "/opt/zwinger/bin".into(), host: manager.bin_dir(&id) }]
        );
        assert_eq!(wrappers(&manager, &id), ["pi"]);
        let pi = std::fs::read_to_string(manager.bin_dir(&id).join("pi")).unwrap();
        assert_eq!(Some(pi), launch::wrapper(AgentKind::Pi, launch::Found::default(), true));

        manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80, |_| {}).await.unwrap();
        assert_eq!(runtime.argvs.lock().unwrap().last().unwrap(), &["/opt/zwinger/bin/pi"]);
        assert!(matches!(
            manager.open_session(&id, agent(AgentKind::Claude, false), 24, 80, |_| {}).await,
            Err(Error::RuntimeMissing(name)) if name == "claude"
        ));
        runtimes::fake_install(&manager.data, "claude");
        runtimes::fake_install(&manager.data, "codex");
        // Installed after the start: the VM has not mounted it.
        assert!(matches!(
            manager.open_session(&id, agent(AgentKind::Codex, false), 24, 80, |_| {}).await,
            Err(Error::RuntimeNotMounted(_))
        ));
        assert!(matches!(
            manager.open_session(&id, agent(AgentKind::Gemini, false), 24, 80, |_| {}).await,
            Err(Error::NotHostAgent(_))
        ));

        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        assert_eq!(wrappers(&manager, &id), ["claude", "codex", "pi"]);
        manager.open_session(&id, agent(AgentKind::Codex, true), 24, 80, |_| {}).await.unwrap();
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
    async fn a_live_agent_session_keeps_the_shield_busy() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        manager.open_session(&id, SessionKind::Shell, 24, 80, |_| {}).await.unwrap();
        assert!(!lock(&manager.state).is_busy(&id));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = manager
            .open_session(&id, agent(AgentKind::Pi, false), 24, 80, move |e| {
                let _ = tx.send(e);
            })
            .await
            .unwrap();
        assert!(lock(&manager.state).is_busy(&id));
        manager.send(&session, PtyInput::Kill).unwrap();
        while next(&mut rx).await != PtyEvent::Exit(137) {}
        assert!(!lock(&manager.state).is_busy(&id));
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
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault(), temp_dir(), host()).unwrap();
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
    async fn stop_kills_sessions() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager
            .open_session(&id, agent(AgentKind::Pi, false), 24, 80, move |e| {
                let _ = tx.send(e);
            })
            .await
            .unwrap();
        next(&mut rx).await;

        manager.stop(&id).await.unwrap();
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
        assert!(manager.list()[0].sessions.is_empty());
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

    fn hosts(events: &[NetEvent]) -> Vec<(u64, &str)> {
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
            let (shield, event) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(shield, id);
            sent.push(event);
        }
        assert_eq!(hosts(&sent), [(1, "google.com"), (2, "example.com")]);
        assert_eq!(manager.list_net_events(&id).unwrap(), sent);

        // The ring stays after a stop; a restart continues the numbers.
        manager.stop(&id).await.unwrap();
        net_closed(&runtime, 0).await;
        assert_eq!(manager.list_net_events(&id).unwrap().len(), 2);
        *runtime.net_script.lock().unwrap() = vec![dns_denied("2999-01-01T00:00:01Z", "google.com")];
        manager.start(&id).await.unwrap();
        let (_, event) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
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

        net_sender(&runtime, 0)
            .send(dns_denied("2999-01-01T00:00:00Z", "google.com"))
            .await
            .unwrap();
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
    async fn allow_host_leaves_deny_mode() {
        let (manager, _, id) = manager();
        assert_eq!(manager.list()[0].shield.network.mode, NetworkMode::Deny);
        manager.allow_host(&id, "google.com").unwrap();
        let network = &manager.list()[0].shield.network;
        assert_eq!(network.mode, NetworkMode::List);
        assert_eq!(network.domains, ["google.com"]);

        manager.set_network_mode(&id, NetworkMode::Public).unwrap();
        manager.allow_host(&id, "*.example.com").unwrap();
        let network = &manager.list()[0].shield.network;
        assert_eq!(network.mode, NetworkMode::Public);
        assert_eq!(network.domains, ["google.com", "*.example.com"]);

        assert!(matches!(manager.allow_host(&id, "not a host"), Err(Error::InvalidDomain(..))));
        assert!(matches!(manager.allow_host("missing", "google.com"), Err(Error::ShieldNotFound)));
    }

    #[tokio::test]
    async fn edit_while_running_sets_pending_restart() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        assert!(!manager.list()[0].pending_restart);

        manager.add_domain(&id, " Registry.npmjs.org ").unwrap();
        let view = &manager.list()[0];
        assert!(view.pending_restart);
        assert_eq!(view.shield.network.domains, ["registry.npmjs.org"]);

        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        assert!(!manager.list()[0].pending_restart);
    }

    #[tokio::test]
    async fn mode_change_with_same_domains_sets_pending_restart() {
        let (manager, _, id) = manager();
        manager.set_network_mode(&id, NetworkMode::List).unwrap();
        manager.add_domain(&id, "registry.npmjs.org").unwrap();
        manager.start(&id).await.unwrap();
        manager.set_network_mode(&id, NetworkMode::Deny).unwrap();
        assert!(manager.list()[0].pending_restart);
        manager.set_network_mode(&id, NetworkMode::List).unwrap();
        assert!(!manager.list()[0].pending_restart);
    }

    #[test]
    fn domain_edits_dedupe_and_validate() {
        let (manager, _, id) = manager();
        manager.add_domain(&id, "registry.npmjs.org").unwrap();
        manager.add_domain(&id, "Registry.NPMjs.org").unwrap();
        manager.add_domain(&id, "*.anthropic.com").unwrap();
        assert!(matches!(
            manager.add_domain(&id, "https://example.com"),
            Err(Error::InvalidDomain(..))
        ));
        assert!(matches!(manager.add_domain("nope", "a.io"), Err(Error::ShieldNotFound)));
        manager.remove_domain(&id, "registry.npmjs.org").unwrap();
        manager.remove_domain(&id, "registry.npmjs.org").unwrap();
        assert_eq!(manager.list()[0].shield.network.domains, ["*.anthropic.com"]);
    }

    #[test]
    fn close_unknown_session_is_ok() {
        let (manager, _, _) = manager();
        manager.close_session("gone").unwrap();
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
        manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80, |_| {}).await.unwrap();

        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80, |_| {}).await.unwrap();
        manager.open_session(&id, SessionKind::Shell, 24, 80, |_| {}).await.unwrap();
        assert_eq!(
            *runtime.cwds.lock().unwrap(),
            ["/workspace", "/workspace/agent", "/workspace"]
        );
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
        assert_eq!((tasks[0].request.as_str(), tasks[0].gates.as_slice()), ("Fix the map.", ["pnpm build".to_owned()].as_slice()));
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
                AppDraft {
                    id: None,
                    name: command.into(),
                    command: command.into(),
                    cwd: WORKSPACE.into(),
                    autostart,
                },
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
        assert_eq!(
            runtime.argvs.lock().unwrap().last().unwrap(),
            &["/bin/sh", "-lc", "pnpm dev"]
        );
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

        let attach = || {
            let (tx, rx) = mpsc::unbounded_channel();
            manager
                .attach_app(&session, move |e| {
                    let _ = tx.send(e);
                })
                .unwrap();
            rx
        };
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
        let attach = || {
            let (tx, rx) = mpsc::unbounded_channel();
            let token = manager
                .attach_app(&session, move |e| {
                    let _ = tx.send(e);
                })
                .unwrap();
            (token, rx)
        };
        let (old, _old_rx) = attach();
        let (new, mut rx) = attach();
        assert_ne!(old, new);
        manager.detach_app(&session, old);
        let _ = next(&mut rx).await;
        manager.write(&session, b"x".to_vec()).unwrap();
        while next(&mut rx).await != output("x") {}

        manager.detach_app(&session, new);
        manager.write(&session, b"y".to_vec()).unwrap();
        manager.send(&session, PtyInput::Kill).unwrap();
        until(|| runs(&manager) == [(web.clone(), Some(137))]).await;
        assert!(rx.try_recv().is_err());
        manager.detach_app("gone", new);
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
    async fn put_secret_stores_the_value_outside_the_config() {
        let (manager, runtime, id, vault) = manager_with_vault();
        manager
            .put_secret(&id, secret(" OPENAI_API_KEY ", &["API.openai.com"]), value("sk-hidden"))
            .unwrap();
        assert_eq!(stored(&vault, &id, "OPENAI_API_KEY").as_deref(), Some("sk-hidden"));

        let views = manager.list_secrets(&id).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].hosts, ["api.openai.com"]);
        assert_eq!(views[0].placeholder, "$MSB_OPENAI_API_KEY$");
        assert!(views[0].set);

        let config = std::fs::read_to_string(manager.store.path()).unwrap();
        let shields = serde_json::to_string(&manager.list()).unwrap();
        let secrets = serde_json::to_string(&views).unwrap();
        for text in [config, shields, secrets] {
            assert!(text.contains("OPENAI_API_KEY") && !text.contains("sk-hidden"), "{text}");
        }

        manager.start(&id).await.unwrap();
        assert_eq!(
            *runtime.secrets.lock().unwrap(),
            [(
                "OPENAI_API_KEY".to_owned(),
                vec![microsandbox::HostPattern::Exact("api.openai.com".into())]
            )]
        );
    }

    #[test]
    fn put_secret_needs_a_value_only_when_new() {
        let (manager, _, id, vault) = manager_with_vault();
        assert!(matches!(
            manager.put_secret(&id, secret("KEY", &["a.io"]), None),
            Err(Error::SecretValueRequired(_))
        ));
        assert!(matches!(
            manager.put_secret(&id, secret("KEY", &["a.io"]), value("")),
            Err(Error::SecretValueRequired(_))
        ));
        assert!(manager.list_secrets(&id).unwrap().is_empty());

        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v1")).unwrap();
        manager.put_secret(&id, secret("KEY", &["b.io"]), None).unwrap();
        assert_eq!(manager.list_secrets(&id).unwrap()[0].hosts, ["b.io"]);
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v1"));

        assert!(matches!(
            manager.put_secret(&id, secret("PATH", &["a.io"]), value("x")),
            Err(Error::InvalidSecret(..))
        ));
        assert!(matches!(
            manager.put_secret("nope", secret("KEY", &["a.io"]), value("x")),
            Err(Error::ShieldNotFound)
        ));
        assert_eq!(vault.values.lock().unwrap().len(), 1);
    }

    #[test]
    fn failed_save_restores_the_vault() {
        let (manager, _, id, vault) = manager_with_vault();
        manager.put_secret(&id, secret("OLD", &["a.io"]), value("old")).unwrap();
        break_store(&manager);

        assert!(manager.put_secret(&id, secret("OLD", &["a.io"]), value("new")).is_err());
        assert_eq!(stored(&vault, &id, "OLD").as_deref(), Some("old"));

        let error = manager.put_secret(&id, secret("NEW", &["a.io"]), value("fresh")).unwrap_err();
        assert!(!error.to_string().contains("fresh"));
        assert_eq!(stored(&vault, &id, "NEW"), None);
        let names: Vec<_> = manager.list_secrets(&id).unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["OLD"]);
    }

    #[test]
    fn remove_secret_saves_the_config_first() {
        let (manager, _, id, vault) = manager_with_vault();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v")).unwrap();
        assert!(matches!(manager.remove_secret(&id, "OTHER"), Err(Error::SecretNotFound(_))));

        break_store(&manager);
        assert!(manager.remove_secret(&id, "KEY").is_err());
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));
        assert_eq!(manager.list_secrets(&id).unwrap().len(), 1);

        let path = manager.store.path().to_owned();
        std::fs::remove_dir_all(&path).unwrap();
        manager.remove_secret(&id, "KEY").unwrap();
        assert_eq!(stored(&vault, &id, "KEY"), None);
        assert!(manager.list_secrets(&id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_removes_the_values() {
        let (manager, _, id, vault) = manager_with_vault();
        manager.put_secret(&id, secret("A", &["a.io"]), value("1")).unwrap();
        manager.put_secret(&id, secret("B", &["a.io"]), value("2")).unwrap();
        manager.delete(&id).await.unwrap();
        assert!(vault.values.lock().unwrap().is_empty());
        assert!(manager.list().is_empty());
    }

    #[tokio::test]
    async fn start_refuses_a_missing_value() {
        let (manager, runtime, id, vault) = manager_with_vault();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v")).unwrap();
        vault.delete(&id, "KEY").unwrap();
        assert!(!manager.list_secrets(&id).unwrap()[0].set);

        assert!(matches!(manager.start(&id).await, Err(Error::SecretMissing(n)) if n == "KEY"));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(runtime.log.lock().unwrap().is_empty());
    }

    /// `shields.json` can be edited by hand. A host that the builder does not
    /// accept must give an error, not a panic.
    #[tokio::test]
    async fn start_refuses_an_invalid_stored_host() {
        let (manager, runtime, id) = manager();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v")).unwrap();
        lock(&manager.state).shields[0].secrets[0].hosts = vec!["*".into()];

        assert!(matches!(manager.start(&id).await, Err(Error::InvalidDomain(..))));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(runtime.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn secret_changes_while_running_set_pending_restart() {
        let (manager, _, id) = manager();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v1")).unwrap();
        manager.start(&id).await.unwrap();
        assert!(!manager.list()[0].pending_restart);

        // The config is the same; only the value changes.
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v2")).unwrap();
        assert!(manager.list()[0].pending_restart);

        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        assert!(!manager.list()[0].pending_restart);

        manager.put_secret(&id, secret("KEY", &["b.io"]), None).unwrap();
        assert!(manager.list()[0].pending_restart);
        manager.put_secret(&id, secret("KEY", &["a.io"]), None).unwrap();
        assert!(!manager.list()[0].pending_restart);

        manager.remove_secret(&id, "KEY").unwrap();
        assert!(manager.list()[0].pending_restart);
    }

    #[tokio::test]
    async fn value_change_during_start_sets_pending_restart() {
        let (manager, runtime, id) = manager();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v1")).unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.start_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let starting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.start(&id).await }
        });
        gate.entered.notified().await;
        assert_eq!(manager.list()[0].status, Status::Starting);
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v2")).unwrap();
        gate.release.notify_one();

        starting.await.unwrap().unwrap();
        let view = &manager.list()[0];
        assert_eq!(view.status, Status::Running);
        assert!(view.pending_restart);
    }

    #[test]
    fn list_secrets_reads_accounts_not_values() {
        let (manager, _, id, vault) = manager_with_vault();
        manager.put_secret(&id, secret("A", &["a.io"]), value("1")).unwrap();
        manager.put_secret(&id, secret("B", &["a.io"]), value("2")).unwrap();
        vault.values.lock().unwrap().remove(&secrets::account(&id, "B"));

        let gets = vault.gets.load(std::sync::atomic::Ordering::SeqCst);
        let set: Vec<_> = manager.list_secrets(&id).unwrap().into_iter().map(|s| (s.name, s.set)).collect();
        assert_eq!(set, [("A".to_owned(), true), ("B".to_owned(), false)]);
        assert_eq!(vault.gets.load(std::sync::atomic::Ordering::SeqCst), gets);
    }

    #[test]
    fn load_prunes_values_with_no_secret() {
        let dir = temp_dir();
        let store = || Store::new(dir.join("shields.json"));
        let vault = vault();
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("keep")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.set("deleted-shield", "KEY", &SecretValue::new("y")).unwrap();

        Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let mut accounts = vault.accounts().unwrap();
        accounts.sort();
        assert_eq!(accounts, [secrets::account(&id, "KEY")]);
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("keep"));
    }

    #[test]
    fn failed_delete_is_an_error_but_does_not_stop_the_load() {
        let dir = temp_dir();
        let store = || Store::new(dir.join("shields.json"));
        let vault = vault();
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir(), host()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.fail_delete.store(true, std::sync::atomic::Ordering::SeqCst);

        assert!(matches!(manager.remove_secret(&id, "KEY"), Err(Error::Keychain(_))));
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

    /// A start during a host and value edit waits for the edit, so it never
    /// pairs the new value with the old hosts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_during_a_secret_edit_uses_the_new_hosts() {
        let dir = temp_dir();
        let vault = Arc::new(GatedVault::default());
        let runtime = FakeRuntime::default();
        let manager = Arc::new(
            Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir(), host()).unwrap(),
        );
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v1")).unwrap();

        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *vault.gate.lock().unwrap() = Some((entered_tx, release_rx));
        let putting = std::thread::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            move || manager.put_secret(&id, secret("KEY", &["b.io"]), value("v2"))
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

        assert_eq!(
            *runtime.secrets.lock().unwrap(),
            [("KEY".to_owned(), vec![microsandbox::HostPattern::Exact("b.io".into())])]
        );
        let view = &manager.list()[0];
        assert_eq!(view.status, Status::Running);
        assert!(!view.pending_restart);
    }

    const NOW: i64 = 1_800_000_000_000;

    /// A Mac with Claude Code 2.1.310, a `~/.claude` with a skill, Codex,
    /// pi and Gemini CLI. `home` is a real folder, as the profile copies it.
    fn mac(home: &Path) -> FakeFs {
        let mut fs = FakeFs {
            home: home.to_owned(),
            ..FakeFs::default()
        };
        let bin = home.join(".local/share/claude/versions/2.1.310");
        fs.links.insert(home.join(".local/bin/claude"), bin.clone());
        fs.versions.insert(bin, "2.1.310 (Claude Code)".into());
        fs.dirs.insert(home.join(".claude"));
        std::fs::create_dir_all(home.join(".claude/skills/review")).unwrap();
        std::fs::write(home.join(".claude/skills/review/SKILL.md"), "review").unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "secret").unwrap();
        fs.file("/usr/local/lib/node_modules/@openai/codex/package.json", r#"{"version":"0.163.0"}"#);
        fs.versions.insert("/usr/local/bin/codex".into(), "codex-cli 0.163.0".into());
        for (name, entry) in [("@earendil-works/pi-coding-agent", "dist/cli.js"), ("@google/gemini-cli", "bundle/gemini.js")] {
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
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir(), host).unwrap();
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
        assert_eq!(shield.network.mode, NetworkMode::List);
        assert_eq!(shield.network.domains, ["api.anthropic.com"]);
        let listed = manager.list_secrets(&id).unwrap();
        assert_eq!((listed[0].name.as_str(), listed[0].set), ("CLAUDE_CODE_OAUTH_TOKEN", true));
        assert_eq!(listed[0].origin, Origin::HostLogin { agent: AgentKind::Claude });
        // The user cannot edit or remove it, and the vault never holds it.
        let mut edit = secret("CLAUDE_CODE_OAUTH_TOKEN", &["api.anthropic.com"]);
        assert!(matches!(manager.put_secret(&id, edit.clone(), Some(SecretValue::new("x"))), Err(Error::HostLoginSecret(_))));
        edit.origin = Origin::HostLogin { agent: AgentKind::Claude };
        assert!(matches!(manager.put_secret(&id, edit, None), Err(Error::HostLoginSecret(_))));
        assert!(matches!(manager.remove_secret(&id, "CLAUDE_CODE_OAUTH_TOKEN"), Err(Error::HostLoginSecret(_))));

        manager.start(&id).await.unwrap();
        assert!(vault.values.lock().unwrap().is_empty());
        assert_eq!(
            *runtime.secrets.lock().unwrap(),
            [("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), vec![microsandbox::HostPattern::Exact("api.anthropic.com".into())])]
        );
        let snapshot = manager.profile_dir(&id).join("claude");
        assert_eq!(
            runtime.binds.lock().unwrap()[1..],
            [Bind { guest: "/opt/zwinger/profile/claude".into(), host: snapshot.clone() }]
        );
        assert!(snapshot.join("skills/review/SKILL.md").is_file());
        assert!(!snapshot.join(".credentials.json").exists());
        let skills = manager.state_dir(&id).join("claude/.claude/skills");
        assert_eq!(std::fs::read_link(skills).unwrap(), Path::new("/opt/zwinger/profile/claude/skills"));

        let view = manager.list().remove(0);
        assert_eq!((view.login_expires_at, view.renew_due), (Some(NOW + 3_600_000), false));
        // Neither the host version nor the pinned Claude Code is downloaded.
        assert!(view.host_notices.iter().any(|n| n.contains("Claude Code 2.1.310 is not downloaded")), "{:?}", view.host_notices);
        assert!(!view.pending_restart);

        // The profile off: its links and its snapshot go at the next start.
        manager.stop(&id).await.unwrap();
        manager.put_host_agent(&id, uses(AgentKind::Claude, false, true), false).await.unwrap();
        manager.start(&id).await.unwrap();
        assert_eq!(runtime.binds.lock().unwrap().len(), 1, "only the bin dir");
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
        assert!(auth.contains("$MSB_CODEX_HOST_LOGIN$") && auth.contains(login::INERT_REFRESH) && auth.contains("id.real"));
        assert!(!auth.contains("at-real") && !auth.contains("rt-real"));
        assert_eq!(runtime.secrets.lock().unwrap()[0].1, [microsandbox::HostPattern::Exact("chatgpt.com".into())]);
        // The access token here is no JWT, so its expiry is unknown.
        let notices = manager.list().remove(0).host_notices;
        assert_eq!(
            notices,
            [
                "The Codex login of this Mac has no expiry that zwinger can read, so the shield does not renew it. Restart the shield when the agent reports a login error.",
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
    async fn pi_copilot_login_blocks_the_github_api_and_gemini_has_no_host_login() {
        let logins = FakeLogins::default();
        let pi = serde_json::json!({ "github-copilot": {
            "type": "oauth", "access": "tid=1;proxy-ep=proxy.business.githubcopilot.com;", "refresh": "r", "expires": NOW + 3_600_000,
        } });
        logins.docs.lock().unwrap().insert(AgentKind::Pi, pi.to_string());
        let gemini = serde_json::json!({ "access_token": "ya29", "expiry_date": NOW + 3_600_000 });
        logins.docs.lock().unwrap().insert(AgentKind::Gemini, gemini.to_string());
        let (manager, runtime, id, _) = host_manager(logins, NOW);

        let err = manager.put_host_agent(&id, uses(AgentKind::Gemini, false, true), true).await.unwrap_err();
        assert!(err.to_string().contains("not supported yet"), "{err}");
        manager.add_domain(&id, "api.github.com").unwrap();
        assert!(matches!(
            manager.put_host_agent(&id, uses(AgentKind::Pi, false, true), true).await,
            Err(Error::RefreshHostConflict(..))
        ));
        manager.remove_domain(&id, "api.github.com").unwrap();
        manager.put_host_agent(&id, uses(AgentKind::Pi, false, true), true).await.unwrap();
        assert_eq!(manager.list()[0].shield.network.domains, ["api.business.githubcopilot.com"]);

        manager.start(&id).await.unwrap();
        assert_eq!(runtime.secrets.lock().unwrap()[0].1, [microsandbox::HostPattern::Exact("api.business.githubcopilot.com".into())]);
        let notices = manager.list().remove(0).host_notices;
        assert!(notices.iter().any(|n| n.contains("GitHub API (api.github.com) is blocked")), "{notices:?}");
    }

    /// A shield with Claude Code's host login, its fake login source and
    /// clock, and the ids that the renew sink got.
    struct Renewal {
        manager: Manager<FakeRuntime>,
        runtime: FakeRuntime,
        id: String,
        logins: Arc<FakeLogins>,
        clock: Arc<FakeClock>,
        due: mpsc::UnboundedReceiver<String>,
    }

    async fn renewal(doc: String) -> Renewal {
        let (dir, home) = (temp_dir(), temp_dir());
        let runtime = FakeRuntime::default();
        let logins = Arc::new(FakeLogins::default());
        logins.docs.lock().unwrap().insert(AgentKind::Claude, doc);
        let clock = Arc::new(FakeClock(NOW.into()));
        let host = Arc::new(HostAgents::new(Arc::new(mac(&home)), logins.clone(), clock.clone(), temp_dir()));
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault(), temp_dir(), host).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        let (tx, due) = mpsc::unbounded_channel();
        manager.set_renew_sink(Box::new(move |id| {
            let _ = tx.send(id.to_owned());
        }));
        manager.put_host_agent(&id, uses(AgentKind::Claude, false, true), false).await.unwrap();
        Renewal {
            manager,
            runtime,
            id,
            logins,
            clock,
            due,
        }
    }

    impl Renewal {
        /// Moves the fake clock and the paused timers forward.
        async fn pass(&self, minutes: i64) {
            self.clock.0.fetch_add(minutes * 60_000, std::sync::atomic::Ordering::SeqCst);
            tokio::time::advance(Duration::from_secs(minutes as u64 * 60)).await;
        }

        /// The next id from the renew sink. The renewal reads the login on a
        /// blocking thread, so this waits in real time. A short sleep, not a
        /// yield: the paused timers fire only when the runtime parks.
        async fn next_due(&mut self) -> String {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(id) = self.due.try_recv() {
                    return id;
                }
                assert!(std::time::Instant::now() < deadline, "no renewal");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }

        fn starts(&self) -> usize {
            self.runtime.log.lock().unwrap().iter().filter(|l| l.starts_with("start")).count()
        }
    }

    #[tokio::test]
    async fn the_renewal_waits_for_a_newer_login_on_the_mac() {
        let mut r = renewal(claude_doc("t", NOW + 20 * 60_000)).await;
        r.manager.set_auto_renew(&r.id, true).unwrap();
        r.manager.start(&r.id).await.unwrap();
        tokio::time::pause();

        // Not due yet.
        r.pass(9).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(r.due.try_recv().is_err());
        // Due, but the Mac has the same login: a restart would not renew it.
        r.pass(2).await;
        assert_eq!(r.next_due().await, r.id);
        let view = r.manager.list().remove(0);
        assert!(!view.renew_due);
        assert!(view.host_notices.iter().any(|n| n.contains("Open Claude Code on this Mac once")), "{:?}", view.host_notices);
        r.manager.renew(&r.id).await.unwrap();
        assert_eq!(r.starts(), 1);

        // The user opened Claude Code on the Mac: a newer login. An app works,
        // so the restart waits.
        r.logins.docs.lock().unwrap().insert(AgentKind::Claude, claude_doc("t2", NOW + 3 * 3_600_000));
        let app = app(&r.manager, &r.id, "pnpm dev", false);
        r.manager.start_app(&r.id, &app).await.unwrap();
        r.pass(1).await;
        assert_eq!(r.next_due().await, r.id);
        assert!(r.manager.list()[0].renew_due);
        r.manager.renew(&r.id).await.unwrap();
        assert_eq!(r.starts(), 1);

        // Idle: the next check restarts with the new login.
        r.manager.stop_app(&r.id, &app).unwrap();
        until(|| r.manager.list()[0].app_runs.iter().all(|a| a.exit_code.is_some())).await;
        r.pass(2).await;
        assert_eq!(r.next_due().await, r.id);
        r.manager.renew(&r.id).await.unwrap();
        assert_eq!(r.starts(), 2);
        let view = r.manager.list().remove(0);
        assert_eq!((view.login_expires_at, view.renew_due), (Some(NOW + 3 * 3_600_000), false));
        assert!(view.host_notices.iter().all(|n| !n.contains("expires soon")), "{:?}", view.host_notices);
    }

    #[tokio::test]
    async fn without_auto_restart_a_newer_login_shows_restart_to_renew() {
        let mut r = renewal(claude_doc("t", NOW + 5 * 60_000)).await;
        r.manager.start(&r.id).await.unwrap();
        tokio::time::pause();
        // Due at the start, with the same login: the notice, no restart.
        assert_eq!(r.next_due().await, r.id);
        assert!(!r.manager.list()[0].renew_due);
        assert!(r.manager.list()[0].host_notices.iter().any(|n| n.contains("expires soon")));
        r.logins.docs.lock().unwrap().insert(AgentKind::Claude, claude_doc("t2", NOW + 3_600_000));
        r.pass(1).await;
        assert_eq!(r.next_due().await, r.id);
        let view = r.manager.list().remove(0);
        assert!(view.renew_due);
        assert!(view.host_notices.iter().all(|n| !n.contains("expires soon")), "{:?}", view.host_notices);
        r.manager.renew(&r.id).await.unwrap();
        assert_eq!(r.starts(), 1);
    }

    #[tokio::test]
    async fn a_login_with_no_expiry_is_not_renewed() {
        let doc = serde_json::json!({ "claudeAiOauth": { "accessToken": "t" } }).to_string();
        let r = renewal(doc).await;
        r.manager.start(&r.id).await.unwrap();
        let view = r.manager.list().remove(0);
        assert_eq!(view.login_expires_at, None);
        assert!(view.host_notices.iter().any(|n| n.contains("does not renew it")), "{:?}", view.host_notices);
        assert!(matches!(lock(&r.manager.state).vms.get(&r.id), Some(Vm::Running { host, .. }) if host.renew.is_empty()));
    }

    #[tokio::test]
    async fn a_renewal_ends_with_the_vm() {
        let r = renewal(claude_doc("t", NOW + 3_600_000)).await;
        r.manager.start(&r.id).await.unwrap();
        let renewal = match lock(&r.manager.state).vms.get(&r.id) {
            Some(Vm::Running { host, .. }) => host.renew[0].clone(),
            _ => unreachable!(),
        };
        r.manager.stop(&r.id).await.unwrap();
        until(|| renewal.is_finished()).await;
        assert!(!r.manager.list()[0].renew_due);
    }

    #[tokio::test]
    async fn host_js_agents_run_from_the_mac_with_their_fallbacks() {
        let (manager, runtime, id, _) = host_manager(FakeLogins::default(), NOW);
        manager.put_host_agent(&id, uses(AgentKind::Pi, false, false), false).await.unwrap();
        manager.put_host_agent(&id, uses(AgentKind::Gemini, false, false), false).await.unwrap();
        manager.start(&id).await.unwrap();
        let binds: Vec<String> = runtime.binds.lock().unwrap().iter().map(|b| b.guest.clone()).collect();
        assert_eq!(binds, ["/opt/zwinger/bin", "/opt/zwinger/host/pi", "/opt/zwinger/host/gemini"]);
        assert!(manager.list()[0].runtimes.contains(&"gemini"));
        // The TUI probe: a terminal, with no session file.
        assert_eq!(
            runtime.argvs.lock().unwrap()[0],
            ["env", "PI_CODING_AGENT_DIR=/opt/zwinger/state/agent", "node", "/opt/zwinger/host/pi/dist/cli.js", "--no-session"]
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
        assert!(matches!(manager.open_session(&id, gemini, 24, 80, |_| {}).await, Err(Error::HostAgentFailed(..))));
        assert_eq!(wrappers(&manager, &id), ["pi"]);
        assert!(wrapper("pi").contains("exec node /opt/zwinger/pi/cli.js "));

        // Gemini CLI that is no host agent of the shield.
        let (plain, _, plain_id) = manager_with_host();
        plain.start(&plain_id).await.unwrap();
        assert!(matches!(plain.open_session(&plain_id, gemini, 24, 80, |_| {}).await, Err(Error::NotHostAgent(_))));
    }

    fn manager_with_host() -> (Manager<FakeRuntime>, FakeRuntime, String) {
        let (manager, runtime, id, _) = host_manager(FakeLogins::default(), NOW);
        (manager, runtime, id)
    }
}
