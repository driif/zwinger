use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use serde::Serialize;
use tokio::task::AbortHandle;

use crate::agent::{AgentKind, launch};
use crate::error::{Error, Result};
use crate::host_agents::HostAgents;
use crate::host_agents::login;
use crate::network::{self, Action, RuleDraft, Target};
use crate::proxy::ca::{self, Ca};
use crate::proxy::log::Entry;
use crate::proxy::{Live, Proxy};
use crate::runtime::SandboxRuntime;
use crate::secrets::{self, SecretValue, Vault};
use crate::shield::apps::AppRunView;
use crate::shield::lifecycle::mounted;
use crate::shield::network::placeholders;
use crate::shield::{self, HostAgentUse, Pos, Shield};
use crate::store::Store;
use crate::task_runs;
use crate::terminal::{Session, SessionView};

/// The network log entries kept per shield.
pub(crate) const NET_RING: usize = 2000;

/// Gets each new network log entry with its shield id.
pub type NetSink = Box<dyn Fn(&str, &Entry) + Send + Sync>;

/// Gets the id of a shield whose guest logins changed.
pub type GuestSink = Box<dyn Fn(&str) + Send + Sync>;

/// Owns the shield configs, the running VMs and their terminal sessions.
pub struct Manager<R: SandboxRuntime> {
    pub(crate) runtime: R,
    pub(crate) store: Store,
    /// The app data dir. It holds the agent runtimes, `shields/<id>`, the
    /// agent state of each shield, and `bin/<id>`, the host-only agent
    /// wrappers of each shield.
    pub(crate) data: PathBuf,
    pub(crate) vault: Arc<dyn Vault>,
    /// The zwinger CA, loaded at the first start.
    pub(crate) ca: tokio::sync::OnceCell<Arc<Ca>>,
    /// Serializes the secret edits, so that a rollback cannot undo a later edit.
    /// The state lock is not held during vault calls.
    pub(crate) secret_edit: Mutex<()>,
    /// Per account, the value that the saved config expects (`None`: no
    /// value) after a restore that did not end. Until the vault is seen to
    /// hold it, starts and edits that use the account fail.
    pub(crate) unsettled: Mutex<HashMap<String, Option<SecretValue>>>,
    pub(crate) state: Arc<Mutex<State<R::Vm>>>,
    pub(crate) net_sink: Arc<OnceLock<NetSink>>,
    pub(crate) runs: Arc<task_runs::Registry>,
    /// Serializes run starts, so two starts cannot take the same run name.
    pub(crate) run_start: Mutex<()>,
    /// The stage limits of new task runs.
    pub(crate) run_config: task_runs::Config,
    pub(crate) host: Arc<HostAgents>,
    pub(crate) guest_sink: Arc<OnceLock<GuestSink>>,
    pub(crate) app_notices: Mutex<Vec<String>>,
}

pub(crate) struct State<V> {
    pub(crate) shields: Vec<Shield>,
    pub(crate) vms: HashMap<String, Vm<V>>,
    pub(crate) sessions: Vec<Session>,
    /// Per shield that starts or runs, the policy and the credential values
    /// of its proxy. An edit changes them at once.
    pub(crate) live: HashMap<String, Arc<Live>>,
    /// Per shield, the last network log entries. They stay after a stop, so
    /// the user can still allow a blocked host.
    pub(crate) net: HashMap<String, VecDeque<Entry>>,
    /// Per shield and app, counts the app stops. A start that saw an older
    /// count was stopped while its process opened.
    pub(crate) app_stops: HashMap<(String, String), u64>,
}

pub(crate) enum Vm<V> {
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
pub(crate) struct HostRun {
    /// pi runs from the host install; else the bundled pi runs.
    pub(crate) pi_host: bool,
    /// The guest path of Gemini CLI's entry point, if it runs in the VM.
    pub(crate) gemini: Option<String>,
    /// The URL of the shield's proxy.
    pub(crate) proxy: Option<String>,
    /// What the user should know, such as a fallback.
    pub(crate) notices: Vec<String>,
    /// The host agents that do not run in the VM, with the reason.
    pub(crate) failed: Vec<(AgentKind, String)>,
    /// The agents with a login that was made in the guest.
    pub(crate) guest_logins: Vec<AgentKind>,
    /// Looks for guest logins while the VM runs. A stop aborts it.
    pub(crate) guest_watch: Option<AbortHandle>,
}

impl HostRun {
    pub(crate) fn found(&self) -> launch::Found<'_> {
        launch::Found { pi_host: self.pi_host, gemini: self.gemini.as_deref(), proxy: self.proxy.as_deref() }
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

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "message", rename_all = "camelCase")]
pub enum Status {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error(String),
}

/// Deletes the values that no configured secret uses, such as the values of a
/// shield whose delete failed. A failure does not stop the app load.
pub(crate) fn prune_vault(vault: &dyn Vault, shields: &[Shield]) {
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

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<V> State<V> {
    pub(crate) fn shield_mut(&mut self, id: &str) -> Result<&mut Shield> {
        self.shields.iter_mut().find(|s| s.id == id).ok_or(Error::ShieldNotFound)
    }

    /// Numbers the entry and adds it to the shield's ring.
    pub(crate) fn push_net(&mut self, id: &str, mut event: Entry) -> Entry {
        let ring = self.net.entry(id.to_owned()).or_default();
        event.seq = ring.back().map_or(1, |last| last.seq + 1);
        if ring.len() == NET_RING {
            ring.pop_front();
        }
        ring.push_back(event.clone());
        event
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
    pub(crate) fn bin_dir(&self, id: &str) -> PathBuf {
        bin_dir(&self.data, id)
    }

    pub(crate) fn state_dir(&self, id: &str) -> PathBuf {
        self.data.join("shields").join(id)
    }

    /// The host-owned profile snapshots of the shield's host agents.
    pub(crate) fn profile_dir(&self, id: &str) -> PathBuf {
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

    /// Applies `edit` to the stored config and persists it. The change is kept
    /// only if the save succeeds. A running VM keeps its applied config until
    /// the next start.
    pub(crate) fn edit(&self, id: &str, edit: impl FnOnce(&mut Shield) -> Result<()>) -> Result<()> {
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
    use crate::shield::testing::*;

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

    #[test]
    fn list_folders_reads_the_shield_folder() {
        let (manager, _, id) = manager();
        let folder = manager.list()[0].shield.folder.clone();
        std::fs::create_dir(folder.join("web")).unwrap();
        assert_eq!(manager.list_folders(&id).unwrap(), ["web"]);
        assert!(matches!(manager.list_folders("nope"), Err(Error::ShieldNotFound)));
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
}
