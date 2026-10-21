use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::agent::cli::{self, CliAgent, Spawn};
use crate::agent::pi::{self, Agent};
use crate::agent::{AgentCommand, AgentKind, Chat, Handle, Sink};
use crate::error::{Error, Result};
use crate::netlog::{self, Dedupe, NetEvent};
use crate::runtime::{
    PipeEvent, PipeInput, PtyEvent, PtyInput, SandboxRuntime, SessionKind, sandbox_policy, secret_entries,
};
use crate::runtimes;
use crate::secrets::{self, Places, Secret, SecretValue, Vault};
use crate::shield::{self, AppDraft, NetworkMode, NetworkPolicy, Pos, Shield, WORKSPACE};
use crate::store::Store;

const NODE_LIMIT: Duration = Duration::from_secs(15);
/// The bytes of `node --version` output that are kept.
const NODE_OUT: usize = 1 << 10;
/// How long a closed agent has to exit before it is killed.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);
/// The network events kept per shield.
const NET_RING: usize = 500;
/// The output bytes kept per app run, for a view that attaches later.
const APP_RING: usize = 256 << 10;
/// The terminal size of an app until a view attaches and resizes it.
const APP_SIZE: (u16, u16) = (24, 80);

/// Gets each new network event with its shield id.
pub type NetSink = Box<dyn Fn(&str, &NetEvent) + Send + Sync>;

/// Owns the shield configs, the running VMs, their terminal sessions and agents.
pub struct Manager<R: SandboxRuntime> {
    runtime: R,
    store: Store,
    /// The app data dir. It holds the agent runtimes, `shields/<id>`, the
    /// agent state of each shield, and `chats/<id>`, the host-only chats of
    /// Claude Code and Codex.
    data: PathBuf,
    vault: Arc<dyn Vault>,
    /// Serializes the secret edits, so that a rollback cannot undo a later edit.
    /// The state lock is not held during vault calls.
    secret_edit: Mutex<()>,
    state: Arc<Mutex<State<R::Vm>>>,
    net_sink: Arc<OnceLock<NetSink>>,
}

struct State<V> {
    shields: Vec<Shield>,
    vms: HashMap<String, Vm<V>>,
    sessions: Vec<Session>,
    /// At most one per shield and agent kind.
    agents: Vec<AgentEntry>,
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
    },
    /// Shutdown is in progress. If it fails, the VM goes back to `Running`.
    Stopping,
    Failed(String),
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

struct AgentEntry {
    id: String,
    shield_id: String,
    kind: AgentKind,
    agent: Handle,
    /// `close_agent` asked the agent to exit. The entry stays until it has
    /// exited, so no second agent starts on the same state meanwhile.
    closing: bool,
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
    /// The agent runtimes that the running VM has mounted.
    pub runtimes: Vec<&'static str>,
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
    let configured: HashSet<String> = shields
        .iter()
        .flat_map(|shield| shield.secrets.iter().map(|s| secrets::account(&shield.id, &s.name)))
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
    pub fn new(runtime: R, store: Store, vault: Arc<dyn Vault>, data: PathBuf) -> Result<Self> {
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
                agents: Vec::new(),
                secrets_rev: HashMap::new(),
                net: HashMap::new(),
                app_stops: HashMap::new(),
            })),
            net_sink: Arc::new(OnceLock::new()),
        })
    }

    /// Only the first sink counts.
    pub fn set_net_sink(&self, sink: NetSink) {
        let _ = self.net_sink.set(sink);
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
                            ..(**applied).clone()
                        } != *shield
                            || *applied_rev != state.secrets_rev(&shield.id),
                    ),
                    Some(Vm::Stopping) => (Status::Stopping, false),
                    Some(Vm::Failed(message)) => (Status::Error(message.clone()), false),
                };
                let sessions = state.sessions.iter().filter(|s| s.shield_id == shield.id);
                let runtimes = match state.vms.get(&shield.id) {
                    Some(Vm::Running { runtimes, .. }) => runtimes.clone(),
                    _ => Vec::new(),
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
                }
            })
            .collect()
    }

    fn chat_dir(&self, id: &str, kind: AgentKind) -> PathBuf {
        self.data.join("chats").join(id).join(kind.name())
    }

    fn state_dir(&self, id: &str) -> PathBuf {
        self.data.join("shields").join(id)
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
            sandbox_policy(&NetworkPolicy {
                mode: NetworkMode::List,
                domains: s.network.domains.clone(),
            })?;
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
            sandbox_policy(&NetworkPolicy {
                mode: NetworkMode::List,
                domains: s.network.domains.clone(),
            })?;
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

    /// Saves the config first, so that a failure leaves a value with no
    /// secret, not a secret with no value.
    pub fn remove_secret(&self, id: &str, name: &str) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        self.edit(id, |s| s.remove_secret(name))?;
        lock(&self.state).bump_secrets_rev(id);
        self.vault.delete(id, name)
    }

    pub fn list_secrets(&self, id: &str) -> Result<Vec<SecretView>> {
        let secrets = lock(&self.state).shield_mut(id)?.secrets.clone();
        let accounts: HashSet<String> = self.vault.accounts()?.into_iter().collect();
        Ok(secrets
            .into_iter()
            .map(|secret| SecretView {
                set: accounts.contains(&secrets::account(id, &secret.name)),
                placeholder: secret.placeholder(),
                name: secret.name,
                hosts: secret.hosts,
                places: secret.places,
            })
            .collect())
    }

    /// Saves the config first, then deletes the secret values, the agent
    /// state and the chats. The first error is returned after all deletes ran.
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
        for dir in [self.state_dir(id), self.data.join("chats").join(id)] {
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
    /// runtime start.
    pub async fn start(&self, id: &str) -> Result<()> {
        let (shield, applied_rev, entries) = {
            let _edit = lock(&self.secret_edit);
            let (shield, applied_rev) = self.claim_start(id)?;
            match self
                .secret_values(&shield)
                .and_then(|values| secret_entries(&shield.secrets, values))
                .and_then(|entries| {
                    // Claude Code's HOME and CODEX_HOME. Codex needs its dir to exist.
                    for kind in [AgentKind::Claude, AgentKind::Codex] {
                        std::fs::create_dir_all(self.state_dir(id).join(kind.name()))?;
                    }
                    Ok(entries)
                }) {
                Ok(entries) => (shield, applied_rev, entries),
                Err(e) => {
                    lock(&self.state).vms.remove(id);
                    return Err(e);
                }
            }
        };
        let installed = runtimes::installed(&self.data);
        let result = match self.runtime.start(&shield, entries, &self.state_dir(id), &installed).await {
            Ok(vm) => match self.check_node(&vm).await {
                Ok(()) => Ok(vm),
                Err(e) => {
                    let _ = self.runtime.stop(&vm).await;
                    Err(e)
                }
            },
            Err(e) => Err(e),
        };

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
                    state.vms.insert(
                        id.to_owned(),
                        Vm::Running {
                            vm: Arc::new(vm),
                            applied: Box::new(shield),
                            applied_rev,
                            net,
                            runtimes: installed.iter().map(|r| r.name).collect(),
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

    fn secret_values(&self, shield: &Shield) -> Result<Vec<SecretValue>> {
        shield
            .secrets
            .iter()
            .map(|secret| {
                self.vault
                    .get(&shield.id, &secret.name)?
                    .ok_or_else(|| Error::SecretMissing(secret.name.clone()))
            })
            .collect()
    }

    /// The VM stays tracked until the runtime confirms shutdown, so a failed
    /// stop leaves the shield running and the user can retry.
    pub async fn stop(&self, id: &str) -> Result<()> {
        let (vm, applied, applied_rev, net, runtimes) = {
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
            state.agents.retain(|a| {
                if a.shield_id == id {
                    a.agent.close();
                    a.agent.kill();
                }
                a.shield_id != id
            });
            (vm, applied, applied_rev, net, runtimes)
        };

        let result = self.runtime.stop(&vm).await;

        let mut state = lock(&self.state);
        match result {
            Ok(()) => {
                state.vms.remove(id);
                if let Some(net) = net {
                    net.abort();
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
            Some(Vm::Running { vm, applied, .. }) => {
                let cwd = match kind {
                    SessionKind::Pi => applied.harness.as_deref(),
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

    /// Returns the id of the shield's agent of `kind` and sends its records
    /// to `sink`. A running agent gets the new sink, so a reopened tab takes
    /// it over; else a new agent starts, after a closing one has exited.
    pub async fn open_agent(&self, shield_id: &str, kind: AgentKind, sink: Sink) -> Result<String> {
        match kind {
            AgentKind::Pi => self.open_pi(shield_id, sink).await,
            _ => self.open_cli(shield_id, kind, sink).await,
        }
    }

    /// Starts pi in the applied harness.
    async fn open_pi(&self, shield_id: &str, sink: Sink) -> Result<String> {
        // Taken by the branch that uses it; each such branch returns.
        let mut sink = Some(sink);
        let find = |a: &&AgentEntry| a.shield_id == shield_id && a.kind == AgentKind::Pi;
        loop {
            let found = {
                let state = lock(&self.state);
                match state.agents.iter().find(find) {
                    Some(entry) if entry.closing => Err(entry.agent.clone()),
                    Some(entry) => {
                        entry.agent.set_sink(sink.take().expect("sink"));
                        return Ok(entry.id.clone());
                    }
                    None => match state.vms.get(shield_id) {
                        Some(Vm::Running { vm, applied, .. }) => {
                            Ok((vm.clone(), applied.harness.as_deref().unwrap_or(WORKSPACE).to_owned()))
                        }
                        _ => return Err(Error::NotRunning),
                    },
                }
            };
            let (vm, cwd) = match found {
                Ok(found) => found,
                Err(closing) => {
                    closing.exited().await;
                    continue;
                }
            };
            let pipe = self.runtime.open_pipe(&vm, &pi::pi_argv(true), &cwd).await?;
            let id = uuid::Uuid::new_v4().to_string();
            let added = {
                let mut state = lock(&self.state);
                // As in `open_session`: a stop that ran meanwhile did not see this process.
                let same_vm = matches!(
                    state.vms.get(shield_id),
                    Some(Vm::Running { vm: current, .. }) if Arc::ptr_eq(current, &vm)
                );
                if !same_vm {
                    let _ = pipe.input.try_send(PipeInput::Kill);
                    return Err(Error::NotRunning);
                }
                match state.agents.iter().find(find) {
                    // A second open that ran at the same time won. If that
                    // agent is closing already, wait for it and try again.
                    Some(entry) => {
                        let _ = pipe.input.try_send(PipeInput::Kill);
                        if !entry.closing {
                            entry.agent.set_sink(sink.take().expect("sink"));
                            return Ok(entry.id.clone());
                        }
                        Err(entry.agent.clone())
                    }
                    None => {
                        let agent = Arc::new(Agent::new(pipe.input, sink.take().expect("sink")));
                        state.agents.push(AgentEntry {
                            id: id.clone(),
                            shield_id: shield_id.to_owned(),
                            kind: AgentKind::Pi,
                            agent: Handle::Pi(agent.clone()),
                            closing: false,
                        });
                        Ok(agent)
                    }
                }
            };
            let agent = match added {
                Ok(agent) => agent,
                Err(closing) => {
                    closing.exited().await;
                    continue;
                }
            };

            let state = self.state.clone();
            let events = pipe.events;
            tokio::spawn(async move {
                let exit = agent.run(events).await;
                lock(&state)
                    .agents
                    .retain(|a| !matches!(&a.agent, Handle::Pi(p) if Arc::ptr_eq(p, &agent)));
                agent.finish(exit);
            });
            return Ok(id);
        }
    }

    /// Makes the Claude Code or Codex agent. It starts its processes itself,
    /// in the applied harness, so no process starts here.
    async fn open_cli(&self, shield_id: &str, kind: AgentKind, sink: Sink) -> Result<String> {
        loop {
            let closing = {
                let mut state = lock(&self.state);
                match state.agents.iter().find(|a| a.shield_id == shield_id && a.kind == kind) {
                    Some(entry) if entry.closing => entry.agent.clone(),
                    Some(entry) => {
                        entry.agent.set_sink(sink);
                        return Ok(entry.id.clone());
                    }
                    None => {
                        let (vm, cwd) = match state.vms.get(shield_id) {
                            Some(Vm::Running { runtimes, .. }) if !runtimes.contains(&kind.name()) => {
                                let installed = runtimes::installed(&self.data).iter().any(|r| r.name == kind.name());
                                let name = kind.name().to_owned();
                                return Err(if installed {
                                    Error::RuntimeNotMounted(name)
                                } else {
                                    Error::RuntimeMissing(name)
                                });
                            }
                            Some(Vm::Running { vm, applied, .. }) => {
                                (vm.clone(), applied.harness.as_deref().unwrap_or(WORKSPACE).to_owned())
                            }
                            _ => return Err(Error::NotRunning),
                        };
                        let id = uuid::Uuid::new_v4().to_string();
                        // A stop ends the agent first, so no process opens in a stopped VM.
                        let runtime = self.runtime.clone();
                        let spawn: Spawn = Box::new(move |argv, at| {
                            let (runtime, vm) = (runtime.clone(), vm.clone());
                            let cwd = at.unwrap_or_else(|| cwd.clone());
                            Box::pin(async move {
                                let pipe = runtime.open_pipe(&vm, &argv, &cwd).await?;
                                Ok((pipe, cwd))
                            })
                        });
                        let on_finish = {
                            let (state, id) = (self.state.clone(), id.clone());
                            Box::new(move || lock(&state).agents.retain(|a| a.id != id))
                        };
                        let agent = CliAgent::new(kind, self.chat_dir(shield_id, kind), spawn, sink, on_finish);
                        state.agents.push(AgentEntry {
                            id: id.clone(),
                            shield_id: shield_id.to_owned(),
                            kind,
                            agent: Handle::Cli(Arc::new(agent)),
                            closing: false,
                        });
                        return Ok(id);
                    }
                }
            };
            closing.exited().await;
        }
    }

    pub async fn agent_call(&self, id: &str, command: AgentCommand) -> Result<Value> {
        let (agent, shield_id) = {
            let state = lock(&self.state);
            let entry = state
                .agents
                .iter()
                .find(|a| a.id == id && !a.closing)
                .ok_or(Error::AgentNotFound)?;
            (entry.agent.clone(), entry.shield_id.clone())
        };
        match agent {
            Handle::Pi(agent) => {
                let state = self.state_dir(&shield_id);
                let wire = blocking(move || command.wire(&state)).await?;
                agent.call(wire).await
            }
            Handle::Cli(agent) => agent.call(command).await,
        }
    }

    /// Asks the agent to exit and returns when it has exited. Kills it if it
    /// has not exited after `CLOSE_LIMIT`. An unknown id is not an error.
    pub async fn close_agent(&self, id: &str) {
        let agent = {
            let mut state = lock(&self.state);
            let Some(entry) = state.agents.iter_mut().find(|a| a.id == id) else {
                return;
            };
            entry.closing = true;
            entry.agent.clone()
        };
        agent.close();
        if tokio::time::timeout(CLOSE_LIMIT, agent.exited()).await.is_err() {
            agent.kill();
            agent.exited().await;
        }
    }

    /// Reads the chat files on the host, so the list also works while the
    /// shield is stopped. Newest first, of all agents.
    pub async fn list_chats(&self, shield_id: &str) -> Result<Vec<Chat>> {
        lock(&self.state).shield_mut(shield_id)?;
        let state = self.state_dir(shield_id);
        let dirs = [AgentKind::Claude, AgentKind::Codex].map(|kind| (kind, self.chat_dir(shield_id, kind)));
        blocking(move || {
            let mut chats = pi::list_chats(&state);
            for (kind, dir) in &dirs {
                chats.extend(cli::list_chats(dir, *kind));
            }
            chats.sort_by(|a, b| b.started.cmp(&a.started));
            Ok(chats)
        })
        .await
    }

    fn cli_agent(&self, shield_id: &str, kind: AgentKind) -> Option<Arc<CliAgent>> {
        let state = lock(&self.state);
        let entry = state.agents.iter().find(|a| a.shield_id == shield_id && a.kind == kind)?;
        match &entry.agent {
            Handle::Cli(agent) => Some(agent.clone()),
            Handle::Pi(_) => None,
        }
    }

    /// A Claude Code or Codex chat that its agent holds is let go first; that
    /// fails while it runs.
    pub async fn delete_chat(&self, shield_id: &str, kind: AgentKind, chat: &str) -> Result<()> {
        lock(&self.state).shield_mut(shield_id)?;
        let chat = chat.to_owned();
        if kind == AgentKind::Pi {
            let state = self.state_dir(shield_id);
            return blocking(move || pi::delete_chat(&state, &chat)).await;
        }
        if let Some(agent) = self.cli_agent(shield_id, kind) {
            agent.forget(&chat)?;
        }
        let dir = self.chat_dir(shield_id, kind);
        blocking(move || cli::delete_chat(&dir, &chat)).await
    }

    /// Renames a Claude Code or Codex chat. pi renames its chats itself.
    pub async fn rename_chat(&self, shield_id: &str, kind: AgentKind, chat: &str, title: &str) -> Result<()> {
        lock(&self.state).shield_mut(shield_id)?;
        if kind == AgentKind::Pi {
            return Err(Error::Unsupported("a rename without pi"));
        }
        let (dir, chat, title) = (self.chat_dir(shield_id, kind), chat.to_owned(), title.to_owned());
        blocking(move || cli::rename_chat(&dir, kind, &chat, &title)).await
    }
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
    use crate::runtime::fake::{FakePipe, FakeRuntime, PtyGate};
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

    fn manager() -> (Manager<FakeRuntime>, FakeRuntime, String) {
        let (manager, runtime, id, _) = manager_with_vault();
        (manager, runtime, id)
    }

    fn manager_with_vault() -> (Manager<FakeRuntime>, FakeRuntime, String, Arc<MemoryVault>) {
        let dir = temp_dir();
        let runtime = FakeRuntime::default();
        let vault = vault();
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir()).unwrap();
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

    fn sink() -> (Sink, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Box::new(move |record| {
                let _ = tx.send(record);
            }),
            rx,
        )
    }

    async fn record(rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn stdin(pipe: &mut FakePipe) -> PipeInput {
        tokio::time::timeout(Duration::from_secs(2), pipe.stdin.recv())
            .await
            .unwrap()
            .unwrap()
    }

    fn take_pipe(runtime: &FakeRuntime) -> FakePipe {
        runtime.pipes.lock().unwrap().pop().expect("no pipe was opened")
    }

    #[tokio::test]
    async fn agent_lifecycle() {
        let (manager, runtime, id) = manager();
        let (first, _first_rx) = sink();
        assert!(matches!(manager.open_agent(&id, AgentKind::Pi, first).await, Err(Error::NotRunning)));
        manager.start(&id).await.unwrap();
        assert!(manager.state_dir(&id).is_dir());

        let manager = Arc::new(manager);
        let (first, mut first_rx) = sink();
        let agent = manager.open_agent(&id, AgentKind::Pi, first).await.unwrap();
        let mut pipe = take_pipe(&runtime);
        assert_eq!(pipe.argv, pi::pi_argv(true));
        assert_eq!(pipe.cwd, "/workspace");

        let call = tokio::spawn({
            let (manager, agent) = (manager.clone(), agent.clone());
            async move { manager.agent_call(&agent, AgentCommand::GetState).await }
        });
        let PipeInput::Data(line) = stdin(&mut pipe).await else { panic!("expected a command") };
        let sent: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(sent["type"], "get_state");
        let response = format!(
            "{{\"type\":\"agent_start\"}}\n{{\"id\":\"{}\",\"type\":\"response\",\"success\":true,\"data\":{{\"isStreaming\":false}}}}\n",
            sent["id"].as_str().unwrap()
        );
        let (a, b) = response.split_at(30);
        pipe.out.send(PipeEvent::Stdout(a.into())).await.unwrap();
        pipe.out.send(PipeEvent::Stdout(b.into())).await.unwrap();
        assert_eq!(call.await.unwrap().unwrap(), serde_json::json!({ "isStreaming": false }));
        assert_eq!(record(&mut first_rx).await["type"], "agent_start");

        // A reopened tab takes the agent over; no second pi starts.
        let (second, mut second_rx) = sink();
        assert_eq!(manager.open_agent(&id, AgentKind::Pi, second).await.unwrap(), agent);
        assert!(runtime.pipes.lock().unwrap().is_empty());
        pipe.out.send(PipeEvent::Stdout(b"{\"type\":\"turn_start\"}\n".to_vec())).await.unwrap();
        assert_eq!(record(&mut second_rx).await["type"], "turn_start");

        assert!(matches!(
            manager
                .agent_call(&agent, AgentCommand::SwitchSession { chat: "../x.jsonl".into() })
                .await,
            Err(Error::ChatNotFound)
        ));

        // Stop ends the agent and fails the call that waits.
        let waiting = tokio::spawn({
            let (manager, agent) = (manager.clone(), agent.clone());
            async move { manager.agent_call(&agent, AgentCommand::GetMessages).await }
        });
        assert!(matches!(stdin(&mut pipe).await, PipeInput::Data(_)));
        manager.stop(&id).await.unwrap();
        assert!(matches!(stdin(&mut pipe).await, PipeInput::Close));
        assert!(matches!(stdin(&mut pipe).await, PipeInput::Kill));
        assert!(matches!(
            manager.agent_call(&agent, AgentCommand::GetState).await,
            Err(Error::AgentNotFound)
        ));
        pipe.out.send(PipeEvent::Exit(137)).await.unwrap();
        assert!(matches!(waiting.await.unwrap(), Err(Error::AgentClosed)));
        assert_eq!(record(&mut second_rx).await["type"], "agent_exit");
        assert!(first_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn exited_agent_is_replaced_on_open() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let (first, mut rx) = sink();
        let agent = manager.open_agent(&id, AgentKind::Pi, first).await.unwrap();
        let pipe = take_pipe(&runtime);
        pipe.out.send(PipeEvent::Stderr(b"Error: no API key\n".to_vec())).await.unwrap();
        pipe.out.send(PipeEvent::Exit(1)).await.unwrap();
        assert_eq!(
            record(&mut rx).await,
            serde_json::json!({ "type": "agent_exit", "code": 1, "stderr": ["Error: no API key"] })
        );

        let (second, _rx) = sink();
        assert_ne!(manager.open_agent(&id, AgentKind::Pi, second).await.unwrap(), agent);
        take_pipe(&runtime);
    }

    #[tokio::test]
    async fn close_agent_closes_stdin() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let (first, _rx) = sink();
        let agent = manager.open_agent(&id, AgentKind::Pi, first).await.unwrap();
        let mut pipe = take_pipe(&runtime);
        let manager = Arc::new(manager);
        let closing = tokio::spawn({
            let (manager, agent) = (manager.clone(), agent.clone());
            async move { manager.close_agent(&agent).await }
        });
        assert!(matches!(stdin(&mut pipe).await, PipeInput::Close));
        assert!(matches!(
            manager.agent_call(&agent, AgentCommand::Abort).await,
            Err(Error::AgentNotFound)
        ));

        // An open during the close waits for the old pi to exit.
        let opening = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.open_agent(&id, AgentKind::Pi, sink().0).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!closing.is_finished() && !opening.is_finished());
        assert!(runtime.pipes.lock().unwrap().is_empty());

        pipe.out.send(PipeEvent::Exit(0)).await.unwrap();
        closing.await.unwrap();
        assert_ne!(opening.await.unwrap().unwrap(), agent);
        take_pipe(&runtime);
        manager.close_agent("gone").await;
    }

    #[tokio::test]
    async fn agent_opened_during_stop_is_killed() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.pty_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let opening = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.open_agent(&id, AgentKind::Pi, sink().0).await }
        });
        gate.entered.notified().await;
        manager.stop(&id).await.unwrap();
        gate.release.notify_one();

        assert!(matches!(opening.await.unwrap(), Err(Error::NotRunning)));
        let mut pipe = take_pipe(&runtime);
        assert!(matches!(stdin(&mut pipe).await, PipeInput::Kill));
        assert!(lock(&manager.state).agents.is_empty());
    }

    async fn record_of(rx: &mut mpsc::UnboundedReceiver<Value>, kind: &str) -> Value {
        loop {
            let next = record(rx).await;
            if next["type"] == kind {
                return next;
            }
        }
    }

    fn prompt(message: &str) -> AgentCommand {
        AgentCommand::Prompt { message: message.into() }
    }

    #[tokio::test]
    async fn stop_ends_the_agents_of_all_kinds() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        assert!(matches!(
            manager.open_agent(&id, AgentKind::Claude, sink().0).await,
            Err(Error::RuntimeMissing(name)) if name == "claude"
        ));
        runtimes::fake_install(&manager.data, "claude");
        runtimes::fake_install(&manager.data, "codex");
        // Installed after the start: the VM has not mounted it.
        assert!(matches!(
            manager.open_agent(&id, AgentKind::Codex, sink().0).await,
            Err(Error::RuntimeNotMounted(_))
        ));
        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        assert_eq!(manager.list()[0].runtimes, ["claude", "codex"]);
        assert_eq!(*runtime.runtimes.lock().unwrap(), ["claude", "codex"]);
        assert!(manager.state_dir(&id).join("codex").is_dir() && manager.state_dir(&id).join("claude").is_dir());

        let manager = Arc::new(manager);
        manager.open_agent(&id, AgentKind::Pi, sink().0).await.unwrap();
        let mut pi_pipe = take_pipe(&runtime);
        let claude = manager.open_agent(&id, AgentKind::Claude, sink().0).await.unwrap();
        // One agent per kind: a reopen takes it over.
        let (claude_sink, mut claude_rx) = sink();
        assert_eq!(manager.open_agent(&id, AgentKind::Claude, claude_sink).await.unwrap(), claude);
        assert!(runtime.pipes.lock().unwrap().is_empty());
        manager.agent_call(&claude, prompt("hi")).await.unwrap();
        let mut claude_pipe = take_pipe(&runtime);
        assert_eq!(claude_pipe.argv, crate::agent::claude::argv(None, None));
        assert_eq!(claude_pipe.cwd, "/workspace");
        assert!(matches!(stdin(&mut claude_pipe).await, PipeInput::Data(_)));
        let (codex_sink, mut codex_rx) = sink();
        let codex = manager.open_agent(&id, AgentKind::Codex, codex_sink).await.unwrap();
        assert_eq!(lock(&manager.state).agents.len(), 3);

        manager.stop(&id).await.unwrap();
        assert!(matches!(stdin(&mut pi_pipe).await, PipeInput::Close));
        assert!(matches!(stdin(&mut pi_pipe).await, PipeInput::Kill));
        assert!(matches!(stdin(&mut claude_pipe).await, PipeInput::Close));
        assert!(matches!(stdin(&mut claude_pipe).await, PipeInput::Kill));
        assert_eq!(record_of(&mut codex_rx, "agent_exit").await["code"], 0);
        claude_pipe.out.send(PipeEvent::Exit(137)).await.unwrap();
        assert_eq!(record_of(&mut claude_rx, "message_end").await["message"]["role"], "user");
        assert_eq!(record_of(&mut claude_rx, "message_end").await["message"]["stopReason"], "error");
        record_of(&mut claude_rx, "agent_exit").await;
        pi_pipe.out.send(PipeEvent::Exit(137)).await.unwrap();
        assert!(matches!(manager.agent_call(&codex, AgentCommand::GetState).await, Err(Error::AgentNotFound)));
        assert!(lock(&manager.state).agents.is_empty());
        assert!(manager.list()[0].runtimes.is_empty());
    }

    #[tokio::test]
    async fn chats_of_all_agents_are_listed_and_deleted_by_kind() {
        let (manager, runtime, id) = manager();
        runtimes::fake_install(&manager.data, "claude");
        manager.start(&id).await.unwrap();
        let sessions = manager.state_dir(&id).join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        std::fs::write(sessions.join("a.jsonl"), "{\"type\":\"session\",\"timestamp\":\"2026-01-01T00:00:00.000Z\"}\n").unwrap();

        let (claude_sink, mut rx) = sink();
        let claude = manager.open_agent(&id, AgentKind::Claude, claude_sink).await.unwrap();
        manager.agent_call(&claude, prompt("Claude chat")).await.unwrap();
        let pipe = take_pipe(&runtime);
        pipe.out
            .send(PipeEvent::Stdout(b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}\n".to_vec()))
            .await
            .unwrap();
        record_of(&mut rx, "agent_settled").await;

        let chats = manager.list_chats(&id).await.unwrap();
        let got: Vec<(AgentKind, &str)> = chats.iter().map(|c| (c.agent, c.title.as_str())).collect();
        assert_eq!(got, [(AgentKind::Claude, "Claude chat"), (AgentKind::Pi, "New chat")]);
        let chat = chats[0].id.clone();
        manager.rename_chat(&id, AgentKind::Claude, &chat, "Renamed").await.unwrap();
        assert_eq!(manager.list_chats(&id).await.unwrap()[0].title, "Renamed");
        assert!(matches!(manager.delete_chat(&id, AgentKind::Pi, &chat).await, Err(Error::ChatNotFound)));
        assert!(matches!(manager.rename_chat(&id, AgentKind::Pi, "a.jsonl", "x").await, Err(Error::Unsupported(_))));

        // The agent lets the chat go, and its process.
        manager.delete_chat(&id, AgentKind::Claude, &chat).await.unwrap();
        pipe.out.send(PipeEvent::Exit(0)).await.unwrap();
        let state = manager.agent_call(&claude, AgentCommand::GetState).await.unwrap();
        assert_eq!(state["sessionFile"], Value::Null);
        assert_eq!(manager.list_chats(&id).await.unwrap().len(), 1);

        manager.agent_call(&claude, prompt("Second")).await.unwrap();
        record_of(&mut rx, "agent_start").await;
        manager.stop(&id).await.unwrap();
        manager.delete(&id).await.unwrap();
        assert!(!manager.data.join("chats").join(&id).exists());
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
    async fn delete_removes_the_chats() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        let sessions = manager.state_dir(&id).join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        std::fs::write(sessions.join("a.jsonl"), "{\"type\":\"session\",\"timestamp\":\"t\"}\n").unwrap();
        manager.stop(&id).await.unwrap();
        assert_eq!(manager.list_chats(&id).await.unwrap()[0].id, "a.jsonl");

        manager.delete(&id).await.unwrap();
        assert!(!manager.state_dir(&id).exists());
        assert!(matches!(manager.list_chats(&id).await, Err(Error::ShieldNotFound)));
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
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault(), temp_dir()).unwrap();
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
            .open_session(&id, SessionKind::Pi, 24, 80, move |e| {
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
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.add_mount(&id, temp_dir().to_str().unwrap(), "/mnt/data", true).unwrap();
        manager.set_mount_readonly(&id, "/mnt/data", false).unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir()).unwrap();
        let mounts = &reloaded.list()[0].shield.mounts;
        assert_eq!(mounts.len(), 1);
        assert!(!mounts[0].readonly);

        reloaded.delete(&id).await.unwrap();
        assert!(Manager::new(FakeRuntime::default(), store(), vault(), temp_dir()).unwrap().list().is_empty());
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
        manager.open_session(&id, SessionKind::Pi, 24, 80, |_| {}).await.unwrap();

        manager.stop(&id).await.unwrap();
        manager.start(&id).await.unwrap();
        manager.open_session(&id, SessionKind::Pi, 24, 80, |_| {}).await.unwrap();
        manager.open_session(&id, SessionKind::Shell, 24, 80, |_| {}).await.unwrap();
        assert_eq!(
            *runtime.cwds.lock().unwrap(),
            ["/workspace", "/workspace/agent", "/workspace"]
        );
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
        let manager = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir()).unwrap();
        let a = manager.create(temp_dir().to_str().unwrap()).unwrap();
        let b = manager.create(temp_dir().to_str().unwrap()).unwrap();
        assert!(matches!(manager.reorder_shields(std::slice::from_ref(&b)), Err(Error::InvalidOrder)));
        manager.reorder_shields(&[b.clone(), a.clone()]).unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault(), temp_dir()).unwrap();
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
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("keep")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.set("deleted-shield", "KEY", &SecretValue::new("y")).unwrap();

        Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir()).unwrap();
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
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.fail_delete.store(true, std::sync::atomic::Ordering::SeqCst);

        assert!(matches!(manager.remove_secret(&id, "KEY"), Err(Error::Keychain(_))));
        let reloaded = Manager::new(FakeRuntime::default(), store(), vault.clone(), temp_dir()).unwrap();
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
            Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir()).unwrap(),
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
}
