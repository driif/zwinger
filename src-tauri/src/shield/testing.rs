//! The shared fixture of the shield and terminal tests.

pub(crate) use std::collections::BTreeMap;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::time::Duration;

pub(crate) use tokio::sync::mpsc;
pub(crate) use tokio::time::Instant;

pub(crate) use crate::agent::{AgentKind, launch};
pub(crate) use crate::error::{Error, Result};
pub(crate) use crate::host_agents::HostAgents;
pub(crate) use crate::host_agents::fake::{FakeClock, FakeFs};
pub(crate) use crate::host_agents::login;
pub(crate) use crate::host_agents::login::fake::{FakeLogins, claude_doc};
pub(crate) use crate::network::{Action, DefaultAction, Inject, RuleDraft, Target};
pub(crate) use crate::proxy::ca::{self, Ca};
pub(crate) use crate::proxy::log::Entry;
pub(crate) use crate::proxy::{self, Live};
pub(crate) use crate::runtime::PTY_QUEUE;
pub(crate) use crate::runtime::fake::{FakeRuntime, PtyGate};
pub(crate) use crate::runtime::{Bind, PipeEvent, PtyEvent, PtyInput, SandboxRuntime, SessionKind};
pub(crate) use crate::runtimes;
pub(crate) use crate::secrets::fake::MemoryVault;
pub(crate) use crate::secrets::{self, SecretValue, Vault};
pub(crate) use crate::shield::manager::*;
pub(crate) use crate::shield::network::*;
pub(crate) use crate::shield::{self, AppDraft, HostAgentUse, Pos, TaskDraft, WORKSPACE};
pub(crate) use crate::store::Store;
pub(crate) use crate::terminal::output::*;

pub(crate) fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zwinger-sessions-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// Host shells run `/bin/sh`, so the tests do not run the user's login files.
pub(crate) fn host_sh() {
    static SET: std::sync::Once = std::sync::Once::new();
    // SAFETY: the tests read SHELL only to start a host shell.
    SET.call_once(|| unsafe { std::env::set_var("SHELL", "/bin/sh") });
}

pub(crate) fn vault() -> Arc<MemoryVault> {
    Arc::new(MemoryVault::default())
}

/// No host agents.
pub(crate) fn host() -> Arc<HostAgents> {
    host_with(FakeFs::default(), FakeLogins::default(), 0)
}

pub(crate) fn host_with(fs: FakeFs, logins: FakeLogins, now: i64) -> Arc<HostAgents> {
    Arc::new(HostAgents::new(Arc::new(fs), Arc::new(logins), Arc::new(FakeClock(now.into())), temp_dir()))
}

pub(crate) fn manager() -> (Manager<FakeRuntime>, FakeRuntime, String) {
    let (manager, runtime, id, _) = manager_with_vault();
    (manager, runtime, id)
}

pub(crate) fn manager_with_vault() -> (Manager<FakeRuntime>, FakeRuntime, String, Arc<MemoryVault>) {
    let dir = temp_dir();
    let runtime = FakeRuntime::default();
    let vault = vault();
    let manager =
        Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir(), host()).unwrap();
    let id = manager.create(dir.to_str().unwrap()).unwrap();
    (manager, runtime, id, vault)
}

pub(crate) async fn next(rx: &mut mpsc::UnboundedReceiver<PtyEvent>) -> PtyEvent {
    tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap()
}

pub(crate) fn output(text: &str) -> PtyEvent {
    PtyEvent::Output(text.as_bytes().to_vec())
}

pub(crate) fn attach(manager: &Manager<FakeRuntime>, session: &str) -> (u64, mpsc::UnboundedReceiver<PtyEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let token = manager
        .attach_session(session, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
    (token, rx)
}

pub(crate) fn host_rule(host: &str, action: Action) -> RuleDraft {
    RuleDraft { id: None, target: Target::Host { host: host.into() }, action }
}

/// An Allow rule for `host`. Returns its id.
pub(crate) fn allow<R: SandboxRuntime>(manager: &Manager<R>, id: &str, host: &str) -> String {
    manager.put_rule(id, host_rule(host, Action::Allow)).unwrap()
}

pub(crate) fn bearer() -> Option<Inject> {
    Some(Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() })
}

/// Sets the credential `name` on an Allow rule for `host`.
pub(crate) fn credential<R: SandboxRuntime>(
    manager: &Manager<R>,
    id: &str,
    host: &str,
    name: &str,
    value: Option<&str>,
) -> Result<()> {
    let rule = allow(manager, id, host);
    manager.set_rule_credential(id, &rule, name, bearer(), value.map(SecretValue::new))
}

pub(crate) fn live(manager: &Manager<FakeRuntime>, id: &str) -> Arc<Live> {
    lock(&manager.state).live.get(id).cloned().expect("the shield has a proxy")
}

/// Stops the shield (and in the second case starts it again) while
/// `open_pty` is held, then releases it.
pub(crate) async fn open_during(restart: bool) {
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

pub(crate) fn agent(agent: AgentKind, resume: bool) -> SessionKind {
    SessionKind::Agent { agent, resume }
}

pub(crate) fn wrappers(manager: &Manager<FakeRuntime>, id: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(manager.bin_dir(id))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

pub(crate) fn dns_denied(at: &str, host: &str) -> String {
    format!("{at} DEBUG microsandbox_network::engine::dns::forwarder: DNS query denied by network policy domain={host}")
}

pub(crate) fn net_sender(runtime: &FakeRuntime, index: usize) -> mpsc::Sender<String> {
    runtime.net_lines.lock().unwrap()[index].clone()
}

pub(crate) async fn net_closed(runtime: &FakeRuntime, index: usize) {
    tokio::time::timeout(Duration::from_secs(2), net_sender(runtime, index).closed())
        .await
        .expect("the follow task still runs");
}

pub(crate) fn hosts(events: &[Entry]) -> Vec<(u64, &str)> {
    events.iter().map(|e| (e.seq, e.host.as_str())).collect()
}

pub(crate) fn task_draft(id: Option<String>, title: &str, kinds: [AgentKind; 2]) -> TaskDraft {
    TaskDraft {
        id,
        title: title.into(),
        request: "  Fix the map.  ".into(),
        agents: kinds.map(|kind| shield::TaskAgent { kind, model: None }),
        gates: vec!["pnpm build".into(), "  ".into()],
        workdir: "/workspace".into(),
    }
}

pub(crate) fn app(manager: &Manager<FakeRuntime>, id: &str, command: &str, autostart: bool) -> String {
    manager
        .put_app(
            id,
            AppDraft { id: None, name: command.into(), command: command.into(), cwd: WORKSPACE.into(), autostart },
        )
        .unwrap()
}

pub(crate) fn runs(manager: &Manager<FakeRuntime>) -> Vec<(String, Option<i32>)> {
    manager.list()[0].app_runs.iter().map(|r| (r.app_id.clone(), r.exit_code)).collect()
}

pub(crate) async fn until(mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !done() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition not met");
}

pub(crate) fn terminal_ids(manager: &Manager<FakeRuntime>) -> Vec<String> {
    manager.list()[0].shield.terminals.iter().map(|t| t.id.clone()).collect()
}

pub(crate) async fn app_start_during(remove: bool) {
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

pub(crate) fn value(text: &str) -> Option<SecretValue> {
    Some(SecretValue::new(text))
}

pub(crate) fn stored(vault: &MemoryVault, id: &str, name: &str) -> Option<String> {
    vault.get(id, name).unwrap().map(|v| v.expose().to_owned())
}

/// Makes the next config save fail: the store renames onto a non-empty folder.
pub(crate) fn break_store(manager: &Manager<FakeRuntime>) {
    let path = manager.store.path().to_owned();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir_all(path.join("blocker")).unwrap();
}

/// Holds each `set` before the write until the test releases it.
#[derive(Default)]
pub(crate) struct GatedVault {
    pub(crate) inner: MemoryVault,
    pub(crate) gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
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

pub(crate) const NOW: i64 = 1_800_000_000_000;

/// A Mac with Claude Code 2.1.310, a `~/.claude` with a skill, Codex,
/// pi and Gemini CLI. `home` is a real folder, as the profile copies it.
pub(crate) fn mac(home: &Path) -> FakeFs {
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

pub(crate) fn host_manager(
    logins: FakeLogins,
    now: i64,
) -> (Manager<FakeRuntime>, FakeRuntime, String, Arc<MemoryVault>) {
    let (dir, home) = (temp_dir(), temp_dir());
    let runtime = FakeRuntime::default();
    let vault = vault();
    let host = host_with(mac(&home), logins, now);
    let manager =
        Manager::new(runtime.clone(), Store::new(dir.join("shields.json")), vault.clone(), temp_dir(), host).unwrap();
    let id = manager.create(dir.to_str().unwrap()).unwrap();
    (manager, runtime, id, vault)
}

pub(crate) fn uses(kind: AgentKind, profile: bool, login: bool) -> HostAgentUse {
    HostAgentUse { kind, profile, login }
}

pub(crate) fn manager_with_host() -> (Manager<FakeRuntime>, FakeRuntime, String) {
    let (manager, runtime, id, _) = host_manager(FakeLogins::default(), NOW);
    (manager, runtime, id)
}

/// The hosts of the rules with a credential.
pub(crate) fn holders(manager: &Manager<FakeRuntime>) -> Vec<String> {
    let rules = manager.list().remove(0).shield.network.rules;
    rules.iter().filter(|r| !r.credentials.is_empty()).map(|r| r.label()).collect()
}

pub(crate) fn put_codex_login(manager: &Manager<FakeRuntime>, id: &str) -> PathBuf {
    let codex = manager.state_dir(id).join("codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(codex.join("auth.json"), r#"{"tokens":{"access_token":"eyJ"}}"#).unwrap();
    codex.join("auth.json")
}

/// Runs `script` with `sh -c` in the VM. Returns the exit code and the
/// stdout; after a failure also the stderr.
pub(crate) async fn sh<R: SandboxRuntime>(manager: &Manager<R>, vm: &R::Vm, script: &str) -> (i32, String) {
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
