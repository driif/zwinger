use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::{Error, Result};
use crate::runtime::{PtyEvent, PtyInput, SandboxRuntime, SessionKind, sandbox_policy, secret_entries};
use crate::secrets::{self, Places, Secret, SecretValue, Vault};
use crate::shield::{self, NetworkMode, NetworkPolicy, Shield, WORKSPACE};
use crate::store::Store;

/// Owns the shield configs, the running VMs and their terminal sessions.
pub struct Manager<R: SandboxRuntime> {
    runtime: R,
    store: Store,
    vault: Arc<dyn Vault>,
    /// Serializes the secret edits, so that a rollback cannot undo a later edit.
    /// The state lock is not held during vault calls.
    secret_edit: Mutex<()>,
    state: Arc<Mutex<State<R::Vm>>>,
}

struct State<V> {
    shields: Vec<Shield>,
    vms: HashMap<String, Vm<V>>,
    sessions: Vec<Session>,
    /// Per shield, counts the secret value changes. The config does not hold
    /// the values, so a running VM compares this count instead.
    secrets_rev: HashMap<String, u64>,
}

enum Vm<V> {
    Starting,
    /// `applied` is the config the VM booted with; it differs from the stored
    /// config after an edit, until the next start. `applied_rev` is the
    /// `secrets_rev` that the start read before it read the values.
    Running {
        vm: Arc<V>,
        applied: Shield,
        applied_rev: u64,
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShieldView {
    #[serde(flatten)]
    pub shield: Shield,
    pub status: Status,
    pub pending_restart: bool,
    pub sessions: Vec<SessionView>,
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

    fn session(&self, id: &str) -> Result<&Session> {
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .ok_or(Error::SessionNotFound)
    }
}

impl<R: SandboxRuntime> Manager<R> {
    pub fn new(runtime: R, store: Store, vault: Arc<dyn Vault>) -> Result<Self> {
        let shields = store.load()?;
        prune_vault(vault.as_ref(), &shields);
        Ok(Self {
            runtime,
            store,
            vault,
            secret_edit: Mutex::new(()),
            state: Arc::new(Mutex::new(State {
                shields,
                vms: HashMap::new(),
                sessions: Vec::new(),
                secrets_rev: HashMap::new(),
            })),
        })
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
                    // A new name is visible at once, so it needs no restart.
                    Some(Vm::Running {
                        applied, applied_rev, ..
                    }) => (
                        Status::Running,
                        Shield {
                            name: shield.name.clone(),
                            ..applied.clone()
                        } != *shield
                            || *applied_rev != state.secrets_rev(&shield.id),
                    ),
                    Some(Vm::Stopping) => (Status::Stopping, false),
                    Some(Vm::Failed(message)) => (Status::Error(message.clone()), false),
                };
                ShieldView {
                    shield: shield.clone(),
                    status,
                    pending_restart,
                    sessions: state
                        .sessions
                        .iter()
                        .filter(|s| s.shield_id == shield.id)
                        .map(|s| SessionView {
                            id: s.id.clone(),
                            kind: s.kind,
                            exit_code: s.exit_code,
                        })
                        .collect(),
                }
            })
            .collect()
    }

    pub fn create(&self, folder: &str) -> Result<String> {
        let shield = Shield::new(folder)?;
        let id = shield.id.clone();
        let mut state = lock(&self.state);
        state.shields.push(shield);
        self.store.save(&state.shields)?;
        Ok(id)
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

    /// Saves the config first, then deletes the secret values. The first vault
    /// error is returned after all deletes ran.
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
            {
                Ok(entries) => (shield, applied_rev, entries),
                Err(e) => {
                    lock(&self.state).vms.remove(id);
                    return Err(e);
                }
            }
        };
        let result = self.runtime.start(&shield, entries).await;

        let mut state = lock(&self.state);
        match result {
            Ok(vm) => {
                state.vms.insert(
                    id.to_owned(),
                    Vm::Running {
                        vm: Arc::new(vm),
                        applied: shield,
                        applied_rev,
                    },
                );
                Ok(())
            }
            Err(e) => {
                state.vms.insert(id.to_owned(), Vm::Failed(e.to_string()));
                Err(e)
            }
        }
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
        shield.check_overlap_with(state.shields.iter().filter(|s| s.id != id))?;
        state.vms.insert(id.to_owned(), Vm::Starting);
        Ok((shield, state.secrets_rev(id)))
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
        let (vm, applied, applied_rev) = {
            let mut state = lock(&self.state);
            state.shield_mut(id)?;
            let (vm, applied, applied_rev) = match state.vms.get(id) {
                Some(Vm::Running {
                    vm,
                    applied,
                    applied_rev,
                }) => (vm.clone(), applied.clone(), *applied_rev),
                Some(Vm::Starting) => return Err(Error::Starting),
                Some(Vm::Stopping) => return Err(Error::Stopping),
                Some(Vm::Failed(_)) | None => return Err(Error::NotRunning),
            };
            state.vms.insert(id.to_owned(), Vm::Stopping);
            state.sessions.retain(|s| {
                if s.shield_id == id {
                    let _ = s.input.send(PtyInput::Kill);
                }
                s.shield_id != id
            });
            (vm, applied, applied_rev)
        };

        let result = self.runtime.stop(&vm).await;

        let mut state = lock(&self.state);
        match result {
            Ok(()) => {
                state.vms.remove(id);
            }
            Err(_) => {
                state.vms.insert(
                    id.to_owned(),
                    Vm::Running {
                        vm,
                        applied,
                        applied_rev,
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
        let mut pty = self.runtime.open_pty(&vm, kind, &cwd, rows, cols).await?;
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
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::sync::mpsc;

    use super::*;
    use crate::runtime::fake::{FakeRuntime, PtyGate};
    use crate::secrets::fake::MemoryVault;
    use crate::secrets::tests::secret;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone()).unwrap();
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
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault()).unwrap();
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
        let manager = Manager::new(FakeRuntime::default(), store(), vault()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.add_mount(&id, temp_dir().to_str().unwrap(), "/mnt/data", true).unwrap();
        manager.set_mount_readonly(&id, "/mnt/data", false).unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store(), vault()).unwrap();
        let mounts = &reloaded.list()[0].shield.mounts;
        assert_eq!(mounts.len(), 1);
        assert!(!mounts[0].readonly);

        reloaded.delete(&id).await.unwrap();
        assert!(Manager::new(FakeRuntime::default(), store(), vault()).unwrap().list().is_empty());
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
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("keep")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.set("deleted-shield", "KEY", &SecretValue::new("y")).unwrap();

        Manager::new(FakeRuntime::default(), store(), vault.clone()).unwrap();
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
        let manager = Manager::new(FakeRuntime::default(), store(), vault.clone()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.put_secret(&id, secret("KEY", &["a.io"]), value("v")).unwrap();
        vault.set(&id, "GONE", &SecretValue::new("x")).unwrap();
        vault.fail_delete.store(true, std::sync::atomic::Ordering::SeqCst);

        assert!(matches!(manager.remove_secret(&id, "KEY"), Err(Error::Keychain(_))));
        let reloaded = Manager::new(FakeRuntime::default(), store(), vault.clone()).unwrap();
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
            Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone()).unwrap(),
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
