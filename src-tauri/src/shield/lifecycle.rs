use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::sync::mpsc::Receiver;
use tokio::task::AbortHandle;

use crate::agent::{AgentKind, gemini, guest_login, launch, pi};
use crate::error::{Error, Result};
use crate::host_agents::login::{self, HostToken};
use crate::host_agents::{self, HostAgents, profile};
use crate::netlog::{self, Dedupe};
use crate::proxy::ca::Ca;
use crate::proxy::inject::Login;
use crate::proxy::log::Entry;
use crate::proxy::{self, Live, Proxy};
use crate::runtime::{Bind, GuestNet, PipeEvent, PipeInput, PtyEvent, PtyInput, SandboxRuntime};
use crate::runtimes;
use crate::secrets::{self, SecretValue};
use crate::shield::apps::APP_SIZE;
use crate::shield::manager::{HostRun, Manager, Vm, bin_dir, blocking, lock};
use crate::shield::network::placeholders;
use crate::shield::{Shield, WORKSPACE};

pub(crate) const NODE_LIMIT: Duration = Duration::from_secs(15);

/// The bytes of `node --version` output that are kept.
pub(crate) const NODE_OUT: usize = 1 << 10;

/// How long a shield stop waits for its task runs to write their manifests.
pub(crate) const RUN_STOP_LIMIT: Duration = Duration::from_secs(15);

/// How long the check that a host package loads in the VM may run.
pub(crate) const PROBE_LIMIT: Duration = Duration::from_secs(15);

/// The host pi's TUI that still runs after this long has loaded.
pub(crate) const TUI_PROBE: Duration = Duration::from_secs(4);

/// The bytes of a probe's stderr that a notice shows.
pub(crate) const PROBE_ERR: usize = 512;

/// The shields that `stop_all` stops at once.
pub(crate) const STOP_PARALLEL: usize = 4;

/// How often a running shield looks for agent logins that were made in the guest.
pub(crate) const GUEST_LOGIN_POLL: Duration = Duration::from_secs(10);

impl<R: SandboxRuntime> Manager<R> {
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
    pub(crate) async fn ca(&self) -> Result<Arc<Ca>> {
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
    pub(crate) async fn start_proxy(
        &self,
        id: &str,
        shield: &Shield,
        live: Arc<Live>,
        tokens: Vec<HostToken>,
    ) -> Result<Proxy> {
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
    pub(crate) fn watch_guest_logins(&self, id: &str, vm: &Arc<R::Vm>) -> AbortHandle {
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
    pub(crate) fn follow_net(&self, id: &str, mut lines: Receiver<String>) -> AbortHandle {
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
    pub(crate) fn claim_start(&self, id: &str) -> Result<Shield> {
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
    pub(crate) async fn check_node(&self, vm: &R::Vm) -> Result<()> {
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
    pub(crate) fn vault_values(&self, shield: &Shield) -> Result<HashMap<String, SecretValue>> {
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
    pub(crate) async fn probe(&self, vm: &R::Vm, argv: &[String]) -> std::result::Result<(), String> {
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
    pub(crate) async fn probe_tui(&self, vm: &R::Vm, proxy: Option<&str>) -> std::result::Result<(), String> {
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
}

/// What a start needs for the shield's host agents.
pub(crate) struct Prepared {
    /// The token of each host login, in the order of the config.
    pub(crate) tokens: Vec<HostToken>,
    pub(crate) binds: Vec<Bind>,
    /// The host version of each native runtime that a host agent uses.
    pub(crate) wanted: HashMap<&'static str, String>,
    pub(crate) run: HostRun,
}

/// Copies the profiles and links them into the agents' state, reads the
/// host logins and writes the guest logins, and picks the host packages and
/// runtime versions. It reads the Keychain and copies files, so it runs on a
/// blocking thread. The state dir is guest-writable: each write there
/// follows no link.
pub(crate) fn prepare(host: &HostAgents, data: &Path, shield: &Shield) -> Result<Prepared> {
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

/// The agents that run in the VM: pi always, the others if their runtime is mounted.
pub(crate) fn mounted(runtimes: &[&str]) -> Vec<AgentKind> {
    host_agents::KINDS.into_iter().filter(|k| *k == AgentKind::Pi || runtimes.contains(&k.name())).collect()
}

/// The end of a probe's output, else its exit code.
pub(crate) fn probe_error(out: &[u8], code: i32) -> String {
    let text = String::from_utf8_lossy(out);
    let tail: String = text.trim().chars().rev().take(PROBE_ERR).collect::<Vec<_>>().into_iter().rev().collect();
    if tail.is_empty() { format!("it ended with code {code}") } else { tail }
}

#[cfg(test)]
mod tests {
    use crate::shield::testing::*;

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
