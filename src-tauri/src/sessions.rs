use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::{Error, Result};
use crate::runtime::{PtyEvent, PtyInput, SandboxRuntime, SessionKind, sandbox_policy};
use crate::shield::{self, NetworkMode, NetworkPolicy, Shield, WORKSPACE};
use crate::store::Store;

/// Owns the shield configs, the running VMs and their terminal sessions.
pub struct Manager<R: SandboxRuntime> {
    runtime: R,
    store: Store,
    state: Arc<Mutex<State<R::Vm>>>,
}

struct State<V> {
    shields: Vec<Shield>,
    vms: HashMap<String, Vm<V>>,
    sessions: Vec<Session>,
}

enum Vm<V> {
    Starting,
    /// `applied` is the config the VM booted with; it differs from the stored
    /// config after an edit, until the next start.
    Running { vm: Arc<V>, applied: Shield },
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

fn lock<V>(state: &Mutex<State<V>>) -> MutexGuard<'_, State<V>> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<V> State<V> {
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
    pub fn new(runtime: R, store: Store) -> Result<Self> {
        let shields = store.load()?;
        Ok(Self {
            runtime,
            store,
            state: Arc::new(Mutex::new(State {
                shields,
                vms: HashMap::new(),
                sessions: Vec::new(),
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
                    Some(Vm::Running { applied, .. }) => (
                        Status::Running,
                        Shield {
                            name: shield.name.clone(),
                            ..applied.clone()
                        } != *shield,
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

    /// Applies `edit` to the stored config and persists it. A running VM keeps
    /// its applied config until the next start.
    fn edit(&self, id: &str, edit: impl FnOnce(&mut Shield) -> Result<()>) -> Result<()> {
        let mut state = lock(&self.state);
        let mut shield = state.shield_mut(id)?.clone();
        edit(&mut shield)?;
        shield.check()?;
        *state.shield_mut(id)? = shield;
        self.store.save(&state.shields)
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

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.stop(id).await.or_else(|e| match e {
            Error::NotRunning => Ok(()),
            e => Err(e),
        })?;
        let mut state = lock(&self.state);
        state.vms.remove(id);
        state.shields.retain(|s| s.id != id);
        self.store.save(&state.shields)
    }

    pub async fn start(&self, id: &str) -> Result<()> {
        let shield = {
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
            shield
        };

        let result = self.runtime.start(&shield).await;

        let mut state = lock(&self.state);
        match result {
            Ok(vm) => {
                state.vms.insert(
                    id.to_owned(),
                    Vm::Running {
                        vm: Arc::new(vm),
                        applied: shield,
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

    /// The VM stays tracked until the runtime confirms shutdown, so a failed
    /// stop leaves the shield running and the user can retry.
    pub async fn stop(&self, id: &str) -> Result<()> {
        let (vm, applied) = {
            let mut state = lock(&self.state);
            state.shield_mut(id)?;
            let (vm, applied) = match state.vms.get(id) {
                Some(Vm::Running { vm, applied }) => (vm.clone(), applied.clone()),
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
            (vm, applied)
        };

        let result = self.runtime.stop(&vm).await;

        let mut state = lock(&self.state);
        match result {
            Ok(()) => {
                state.vms.remove(id);
            }
            Err(_) => {
                state.vms.insert(id.to_owned(), Vm::Running { vm, applied });
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
            Some(Vm::Running { vm, applied }) => {
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

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manager() -> (Manager<FakeRuntime>, FakeRuntime, String) {
        let dir = temp_dir();
        let runtime = FakeRuntime::default();
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json"))).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        (manager, runtime, id)
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
        let manager = Manager::new(runtime.clone(), Store::new(dir.join("shields.json"))).unwrap();
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
        let manager = Manager::new(FakeRuntime::default(), store()).unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        manager.add_mount(&id, temp_dir().to_str().unwrap(), "/mnt/data", true).unwrap();
        manager.set_mount_readonly(&id, "/mnt/data", false).unwrap();

        let reloaded = Manager::new(FakeRuntime::default(), store()).unwrap();
        let mounts = &reloaded.list()[0].shield.mounts;
        assert_eq!(mounts.len(), 1);
        assert!(!mounts[0].readonly);

        reloaded.delete(&id).await.unwrap();
        assert!(Manager::new(FakeRuntime::default(), store()).unwrap().list().is_empty());
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
}
