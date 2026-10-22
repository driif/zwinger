use std::future::Future;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use microsandbox::sandbox::{DeploymentProfile, MAX_SANDBOX_LIST_LIMIT};
use microsandbox::{ExecEvent, LogLevel, Sandbox};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedSender};

use crate::agent::pi::{AGENT_DIR, PI_DIR};
use crate::agent::{AgentKind, RESERVED, STATE_DIR, launch};
use crate::error::{Error, Result};
use crate::netlog;
use crate::runtimes::Installed;
use crate::shield::{Shield, WORKSPACE};

const STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
const KILL_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const IMAGE: &str = "node:22-slim";
/// The queue size of each direction of a pipe. A full queue holds the sender.
pub const PIPE_QUEUE: usize = 64;
/// The queue size of the output of a PTY. A full queue holds the reader, so
/// the process blocks on its writes.
pub const PTY_QUEUE: usize = 64;
/// Runs `process.argv[1..]` in its own process group, with the same stdio.
/// On SIGTERM it kills the whole group, so the tool processes of an agent
/// die with it. It exits with the code of the process, or 128 + its signal.
const GROUP_WRAPPER: &str = r#"const { spawn } = require("child_process");
const { signals } = require("os").constants;
const [command, ...args] = process.argv.slice(1);
const child = spawn(command, args, { detached: true, stdio: "inherit" });
process.on("SIGTERM", () => {
  try { process.kill(-child.pid, "SIGKILL"); } catch {}
});
child.on("error", (e) => { console.error(e.message); process.exit(127); });
child.on("exit", (code, signal) => process.exit(code ?? 128 + signals[signal]));
"#;
const SIGTERM: i32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SessionKind {
    /// A login shell in the VM. The UI calls it "VM shell"; the name stays
    /// `shell`, so saved terminals load.
    Shell,
    /// The user's login shell on the Mac, unsandboxed, in the shield's folder.
    Host,
    /// The agent's own TUI, through its wrapper. `resume` continues the
    /// agent's last session.
    Agent { agent: AgentKind, resume: bool },
}

impl SessionKind {
    pub fn argv(self) -> Vec<String> {
        match self {
            Self::Shell => vec!["/bin/bash".to_owned(), "-l".to_owned()],
            Self::Host => vec![crate::terminal::host::login_shell(), "-l".to_owned()],
            Self::Agent { agent, resume } => {
                let mut argv = vec![format!("{}/{}", launch::BIN_DIR, agent.name())];
                if resume {
                    argv.extend(launch::continue_args(agent).iter().map(|a| (*a).to_owned()));
                }
                argv
            }
        }
    }
}

/// The env of each PTY exec. A login shell resets `PATH`; the profile file
/// of [`launch::PROFILE`] puts the wrappers back.
pub const PTY_ENV: [(&str, &str); 3] = [("PATH", launch::PATH), ("TERM", "xterm-256color"), ("COLORTERM", "truecolor")];

/// What the guest needs to use the shield's proxy: the host port that the
/// VM may reach, and the env of every process (see [`launch::net_env`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestNet {
    pub port: u16,
    pub env: Vec<(String, String)>,
}

/// A host folder that a start mounts read-only, such as a host agent's
/// package or profile snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub guest: String,
    pub host: PathBuf,
}

#[derive(Debug)]
pub enum PtyInput {
    Data(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "camelCase")]
pub enum PtyEvent {
    Output(Vec<u8>),
    Exit(i32),
}

/// A running terminal process. `Exit` is the last event; the stream ends after it.
pub struct Pty {
    pub input: UnboundedSender<PtyInput>,
    pub events: Receiver<PtyEvent>,
}

#[derive(Debug)]
pub enum PipeInput {
    Data(Vec<u8>),
    /// Closes stdin.
    Close,
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
}

/// A running process with no terminal. `Exit` is the last event. Both queues
/// hold `PIPE_QUEUE` messages, so a slow reader slows the process down.
pub struct Pipe {
    pub input: Sender<PipeInput>,
    pub events: Receiver<PipeEvent>,
}

/// The seam between the shield manager and the VM backend, so tests do not boot VMs.
pub trait SandboxRuntime: Send + Sync + Clone + 'static {
    type Vm: Send + Sync + 'static;

    /// The VM reaches only `net.port` on the host. `state` is the host dir
    /// of the shield's agent state. Each runtime in `runtimes` is mounted
    /// read-only at `/opt/zwinger/<name>`, and each of `binds` read-only at
    /// its guest path.
    fn start(
        &self,
        shield: &Shield,
        net: &GuestNet,
        state: &Path,
        runtimes: &[Installed],
        binds: &[Bind],
    ) -> impl Future<Output = Result<Self::Vm>> + Send;
    fn stop(&self, vm: &Self::Vm) -> impl Future<Output = Result<()>> + Send;
    fn open_pty(
        &self,
        vm: &Self::Vm,
        argv: &[String],
        cwd: &str,
        rows: u16,
        cols: u16,
    ) -> impl Future<Output = Result<Pty>> + Send;
    /// Runs `argv` with stdin, stdout and stderr as pipes.
    fn open_pipe(&self, vm: &Self::Vm, argv: &[String], cwd: &str) -> impl Future<Output = Result<Pipe>> + Send;
    /// Stops and removes the VMs that an earlier app run left behind.
    /// Returns the names of the VMs that it removed.
    fn reap(&self) -> impl Future<Output = Result<Vec<String>>> + Send;
    /// Follows the raw runtime log lines of the VM that are written from now
    /// on. The lines stop when the receiver is dropped.
    fn net_lines(&self, vm: &Self::Vm) -> impl Future<Output = Result<Receiver<String>>> + Send;
}

/// The outer wall of the shield (ADR 0003): all egress is denied but TCP to
/// the proxy's port on the host, with no DNS. Ingress is denied. The VM runs
/// with the SingleTenant profile: the public-only floor of MultiTenant
/// denies the host, so the guest could not reach the proxy.
pub fn sandbox_policy(port: u16) -> Result<microsandbox::NetworkPolicy> {
    microsandbox::NetworkPolicy::builder()
        .default_deny()
        .rule(|r| r.egress().tcp().port(port).allow_host())
        .build()
        .map_err(|e| Error::Proxy(e.to_string()))
}

const LABEL: (&str, &str) = ("app", "zwinger");
const NAME_PREFIX: &str = "zwinger-";

/// The shield id of a VM name that [`Microsandbox`] gave.
pub fn shield_id(vm_name: &str) -> Option<&str> {
    vm_name.strip_prefix(NAME_PREFIX)
}

/// Stops and removes the zwinger VMs of the default backend. Resolves to their names.
pub async fn reap_zwinger() -> Result<Vec<String>> {
    // Sandboxes from round 01 have no label, so the name prefix also counts.
    let mut found = BTreeMap::new();
    for labelled in [true, false] {
        let mut cursor = None;
        loop {
            let page = Sandbox::list_with(|mut list| {
                list = list.limit(MAX_SANDBOX_LIST_LIMIT);
                if labelled {
                    list = list.label(LABEL.0, LABEL.1);
                }
                match cursor.take() {
                    Some(c) => list.cursor(c),
                    None => list,
                }
            })
            .await?;
            for handle in page.sandboxes {
                if labelled || handle.name().starts_with(NAME_PREFIX) {
                    found.insert(handle.name().to_owned(), handle);
                }
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
    }
    // One stuck sandbox must not keep the others alive.
    let (mut reaped, mut failed) = (Vec::new(), None);
    for (name, handle) in found {
        match handle.destroy().await {
            Ok(()) => reaped.push(name),
            Err(e) => failed = Some(e),
        }
    }
    match failed {
        Some(e) => Err(e.into()),
        None => Ok(reaped),
    }
}

/// `pi` is the host copy of the pi bundle, from [`crate::agent::pi::install_pi`].
#[derive(Clone)]
pub struct Microsandbox {
    pub pi: PathBuf,
}

impl SandboxRuntime for Microsandbox {
    type Vm = Sandbox;

    async fn start(
        &self,
        shield: &Shield,
        net: &GuestNet,
        state: &Path,
        runtimes: &[Installed],
        binds: &[Bind],
    ) -> Result<Sandbox> {
        let policy = sandbox_policy(net.port)?;
        let mut builder = Sandbox::builder(format!("{NAME_PREFIX}{}", shield.id))
            .image(IMAGE)
            .label(LABEL.0, LABEL.1)
            .deployment_profile(DeploymentProfile::SingleTenant)
            .ephemeral(true)
            .replace()
            // The network events are debug lines of the runtime log.
            .log_level(LogLevel::Debug)
            .workdir(WORKSPACE)
            .volume(WORKSPACE, |m| m.bind(&shield.folder));
        for mount in &shield.mounts {
            builder = builder.volume(&mount.guest, |m| {
                let m = m.bind(&mount.host);
                if mount.readonly { m.readonly() } else { m }
            });
        }
        builder = builder
            .volume(PI_DIR, |m| m.bind(&self.pi).readonly())
            .volume(STATE_DIR, |m| m.bind(state))
            .env("PI_CODING_AGENT_DIR", AGENT_DIR)
            .network(|n| n.policy(policy));
        for runtime in runtimes {
            builder = builder.volume(format!("{RESERVED}/{}", runtime.name), |m| m.bind(&runtime.dir).readonly());
        }
        for bind in binds {
            builder = builder.volume(&bind.guest, |m| m.bind(&bind.host).readonly());
        }
        // The builder env reaches every exec; a login shell resets only `PATH`.
        for (name, value) in &net.env {
            builder = builder.env(name, value);
        }
        Ok(builder.create().await?)
    }

    /// A graceful stop has no deadline in microsandbox, so a stuck VM is
    /// killed. The shield then always leaves the Stopping state.
    async fn stop(&self, vm: &Sandbox) -> Result<()> {
        if vm.stop_with_timeout(STOP_LIMIT).await.is_ok() {
            return Ok(());
        }
        Ok(vm.kill_with_timeout(KILL_LIMIT).await?)
    }

    async fn reap(&self) -> Result<Vec<String>> {
        reap_zwinger().await
    }

    /// Tails `runtime.log` itself: `follow_logs` returns a `futures` Stream,
    /// and this crate has no `futures` dependency to read it.
    async fn net_lines(&self, vm: &Sandbox) -> Result<Receiver<String>> {
        let path = microsandbox::logs::log_dir_for(vm.name()).join("runtime.log");
        Ok(netlog::tail(path).await?)
    }

    async fn open_pty(&self, vm: &Sandbox, argv: &[String], cwd: &str, rows: u16, cols: u16) -> Result<Pty> {
        let mut handle = vm
            .exec_stream_with(&argv[0], |e| {
                PTY_ENV
                    .iter()
                    .fold(e.args(&argv[1..]).cwd(cwd), |e, (name, value)| e.env(*name, *value))
                    .tty(true)
                    .stdin_pipe()
            })
            .await?;
        let sink = handle.take_stdin().ok_or(Error::SessionClosed)?;
        let control = handle.control();
        control.resize(rows, cols).await?;

        let (input, mut input_rx) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::channel(PTY_QUEUE);

        tokio::spawn(async move {
            while let Some(msg) = input_rx.recv().await {
                // A failed write or resize only means the process is gone; the
                // output task reports the exit.
                let _ = match msg {
                    PtyInput::Data(data) => sink.write(data).await,
                    PtyInput::Resize { rows, cols } => control.resize(rows, cols).await,
                    PtyInput::Kill => control.kill().await,
                };
            }
        });
        tokio::spawn(async move {
            let mut code = -1;
            while let Some(event) = handle.recv().await {
                match event {
                    ExecEvent::Stdout(bytes) | ExecEvent::Stderr(bytes) => {
                        let _ = events_tx.send(PtyEvent::Output(bytes.to_vec())).await;
                    }
                    ExecEvent::Exited { code: c } => {
                        code = c;
                        break;
                    }
                    ExecEvent::Failed(failed) => {
                        let message = format!("{}\r\n", failed.message);
                        let _ = events_tx.send(PtyEvent::Output(message.into_bytes())).await;
                        break;
                    }
                    ExecEvent::Started { .. } | ExecEvent::StdinError(_) => {}
                }
            }
            let _ = events_tx.send(PtyEvent::Exit(code)).await;
        });

        Ok(Pty { input, events })
    }

    /// Each process runs in the group wrapper, so a kill also ends its children.
    async fn open_pipe(&self, vm: &Sandbox, argv: &[String], cwd: &str) -> Result<Pipe> {
        let argv = group_argv(argv);
        let mut handle = vm.exec_stream_with(&argv[0], |e| e.args(&argv[1..]).cwd(cwd).stdin_pipe()).await?;
        let sink = handle.take_stdin().ok_or(Error::AgentClosed)?;
        let control = handle.control();

        let (input, mut input_rx) = mpsc::channel(PIPE_QUEUE);
        let (events_tx, events) = mpsc::channel(PIPE_QUEUE);
        let (exited_tx, exited) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            while let Some(msg) = input_rx.recv().await {
                // A failed write only means the process is gone; the output
                // task reports the exit.
                let _ = match msg {
                    PipeInput::Data(data) => sink.write(data).await,
                    PipeInput::Close => sink.close().await,
                    PipeInput::Kill => {
                        // The wrapper kills the group. If it does not exit, it is killed alone.
                        let (fallback, mut exited) = (control.clone(), exited.clone());
                        tokio::spawn(async move {
                            if tokio::time::timeout(KILL_LIMIT, exited.wait_for(|e| *e)).await.is_err() {
                                let _ = fallback.kill().await;
                            }
                        });
                        match control.signal(SIGTERM).await {
                            Ok(()) => Ok(()),
                            Err(_) => control.kill().await,
                        }
                    }
                };
            }
        });
        tokio::spawn(async move {
            let mut code = -1;
            // Input runs in its own task, so a full event queue does not hold a kill.
            while let Some(event) = handle.recv().await {
                match event {
                    ExecEvent::Stdout(bytes) => {
                        let _ = events_tx.send(PipeEvent::Stdout(bytes.to_vec())).await;
                    }
                    ExecEvent::Stderr(bytes) => {
                        let _ = events_tx.send(PipeEvent::Stderr(bytes.to_vec())).await;
                    }
                    ExecEvent::Exited { code: c } => {
                        code = c;
                        break;
                    }
                    ExecEvent::Failed(failed) => {
                        let message = format!("{}\n", failed.message);
                        let _ = events_tx.send(PipeEvent::Stderr(message.into_bytes())).await;
                        break;
                    }
                    ExecEvent::Started { .. } | ExecEvent::StdinError(_) => {}
                }
            }
            exited_tx.send_replace(true);
            let _ = events_tx.send(PipeEvent::Exit(code)).await;
        });

        Ok(Pipe { input, events })
    }
}

/// `argv` in the group wrapper. `--` ends the options of node.
fn group_argv(argv: &[String]) -> Vec<String> {
    let mut wrapped = ["node", "-e", GROUP_WRAPPER, "--"].map(str::to_owned).to_vec();
    wrapped.extend_from_slice(argv);
    wrapped
}

#[cfg(test)]
pub mod fake {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    use tokio::sync::Notify;

    /// Echoes input back as output. `Kill` ends the process with code 137.
    #[derive(Default, Clone)]
    pub struct FakeRuntime {
        pub log: Arc<Mutex<Vec<String>>>,
        pub fail_stop: Arc<Mutex<bool>>,
        pub pty_gate: Arc<Mutex<Option<Arc<PtyGate>>>>,
        pub cwds: Arc<Mutex<Vec<String>>>,
        pub argvs: Arc<Mutex<Vec<Vec<String>>>>,
        pub start_gate: Arc<Mutex<Option<Arc<PtyGate>>>>,
        /// Holds the exit of a killed PTY process.
        pub kill_gate: Arc<Mutex<Option<Arc<PtyGate>>>>,
        /// The guest network of the last start.
        pub net: Arc<Mutex<Option<GuestNet>>>,
        /// The runtime names of the last start.
        pub runtimes: Arc<Mutex<Vec<String>>>,
        /// The read-only binds of the last start.
        pub binds: Arc<Mutex<Vec<Bind>>>,
        /// What `node --version` prints. `None` prints a supported version.
        pub node_version: Arc<Mutex<Option<String>>>,
        /// The scripts whose probe fails: `node <script> --version`, or the
        /// start of the pi TUI.
        pub probe_fail: Arc<Mutex<Vec<String>>>,
        /// The root execs that wrote the profile file.
        pub profiles: Arc<Mutex<Vec<Vec<String>>>>,
        /// The count of the root execs that trusted the interception CA.
        pub trusts: Arc<Mutex<usize>>,
        /// The trust exec fails as in an image with no system bundle.
        pub trust_fail: Arc<Mutex<bool>>,
        /// The VM names that `reap` removes.
        pub reaped: Arc<Mutex<Vec<String>>>,
        /// How long each stop takes.
        pub stop_delay: Arc<Mutex<Duration>>,
        /// The stops that run now, and the most that ran at once.
        pub stopping: Arc<std::sync::atomic::AtomicUsize>,
        pub most_stopping: Arc<std::sync::atomic::AtomicUsize>,
        /// The other ends of the pipes from `open_pipe`, except the Node
        /// check. The test plays the process: it reads stdin, and sends output
        /// in chunks of its choice and the exit when it wants.
        pub pipes: Arc<Mutex<Vec<FakePipe>>>,
        /// The lines that each `net_lines` sends first.
        pub net_script: Arc<Mutex<Vec<String>>>,
        /// The senders of the `net_lines` calls. The test sends more lines
        /// and sees when the receiver is gone.
        pub net_lines: Arc<Mutex<Vec<Sender<String>>>>,
        /// The bytes that the next PTY prints after its size, in 64 KiB chunks.
        pub burst: Arc<Mutex<usize>>,
        /// The burst bytes that the PTY reader has taken so far.
        pub burst_sent: Arc<std::sync::atomic::AtomicUsize>,
    }

    pub struct FakePipe {
        pub argv: Vec<String>,
        pub stdin: Receiver<PipeInput>,
        pub out: Sender<PipeEvent>,
    }

    /// Holds `open_pty`, `open_pipe`, `start` or a PTY kill after it signals `entered`, until the test
    /// signals `release`.
    #[derive(Default)]
    pub struct PtyGate {
        pub entered: Notify,
        pub release: Notify,
    }

    pub struct FakeVm {
        pub shield_id: String,
    }

    impl SandboxRuntime for FakeRuntime {
        type Vm = FakeVm;

        async fn start(
            &self,
            shield: &Shield,
            net: &GuestNet,
            _: &Path,
            runtimes: &[Installed],
            binds: &[Bind],
        ) -> Result<FakeVm> {
            *self.runtimes.lock().unwrap() = runtimes.iter().map(|r| r.name.to_owned()).collect();
            *self.binds.lock().unwrap() = binds.to_vec();
            let gate = self.start_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            self.log.lock().unwrap().push(format!("start {}", shield.id));
            *self.net.lock().unwrap() = Some(net.clone());
            Ok(FakeVm { shield_id: shield.id.clone() })
        }

        async fn stop(&self, vm: &FakeVm) -> Result<()> {
            if *self.fail_stop.lock().unwrap() {
                return Err(Error::Io(std::io::Error::other("stop failed")));
            }
            let now = self.stopping.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_stopping.fetch_max(now, Ordering::SeqCst);
            let delay = *self.stop_delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            self.stopping.fetch_sub(1, Ordering::SeqCst);
            self.log.lock().unwrap().push(format!("stop {}", vm.shield_id));
            Ok(())
        }

        async fn reap(&self) -> Result<Vec<String>> {
            self.log.lock().unwrap().push("reap".into());
            Ok(self.reaped.lock().unwrap().clone())
        }

        async fn net_lines(&self, _: &FakeVm) -> Result<Receiver<String>> {
            let script = self.net_script.lock().unwrap().clone();
            let (tx, rx) = mpsc::channel(script.len() + netlog::TAIL_QUEUE);
            for line in script {
                let _ = tx.try_send(line);
            }
            self.net_lines.lock().unwrap().push(tx);
            Ok(rx)
        }

        async fn open_pty(&self, _: &FakeVm, argv: &[String], cwd: &str, rows: u16, cols: u16) -> Result<Pty> {
            self.cwds.lock().unwrap().push(cwd.to_owned());
            self.argvs.lock().unwrap().push(argv.to_vec());
            // The TUI probe of the host pi ends at once: with an error, or as if pi had started.
            if argv.last().is_some_and(|a| a == "--no-session") {
                let (input, _) = mpsc::unbounded_channel();
                let (tx, events) = mpsc::channel(PTY_QUEUE);
                if argv.iter().any(|a| self.probe_fail.lock().unwrap().contains(a)) {
                    let _ =
                        tx.try_send(PtyEvent::Output(b"Error: Cannot find module './darwin-arm64.node'\r\n".to_vec()));
                    let _ = tx.try_send(PtyEvent::Exit(1));
                } else {
                    let _ = tx.try_send(PtyEvent::Exit(0));
                }
                return Ok(Pty { input, events });
            }
            let gate = self.pty_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            let log = self.log.clone();
            let kill_gate = self.kill_gate.clone();
            let (input, mut input_rx) = mpsc::unbounded_channel();
            let (tx, events) = mpsc::channel(PTY_QUEUE);
            let _ = tx.try_send(PtyEvent::Output(format!("{rows}x{cols}").into_bytes()));
            let burst = std::mem::take(&mut *self.burst.lock().unwrap());
            if burst > 0 {
                let (tx, sent) = (tx.clone(), self.burst_sent.clone());
                tokio::spawn(async move {
                    let chunk = 64 << 10;
                    for start in (0..burst).step_by(chunk) {
                        let len = chunk.min(burst - start);
                        if tx.send(PtyEvent::Output(vec![b'x'; len])).await.is_err() {
                            return;
                        }
                        sent.fetch_add(len, std::sync::atomic::Ordering::SeqCst);
                    }
                });
            }
            tokio::spawn(async move {
                while let Some(msg) = input_rx.recv().await {
                    match msg {
                        PtyInput::Data(data) => {
                            let _ = tx.send(PtyEvent::Output(data)).await;
                        }
                        PtyInput::Resize { rows, cols } => {
                            let _ = tx.send(PtyEvent::Output(format!("{rows}x{cols}").into_bytes())).await;
                        }
                        PtyInput::Kill => {
                            log.lock().unwrap().push("kill".into());
                            let gate = kill_gate.lock().unwrap().clone();
                            if let Some(gate) = gate {
                                gate.entered.notify_one();
                                gate.release.notified().await;
                            }
                            let _ = tx.send(PtyEvent::Exit(137)).await;
                            return;
                        }
                    }
                }
            });
            Ok(Pty { input, events })
        }

        async fn open_pipe(&self, _: &FakeVm, argv: &[String], _: &str) -> Result<Pipe> {
            let (input, stdin) = mpsc::channel(PIPE_QUEUE);
            let (out, events) = mpsc::channel(PIPE_QUEUE);
            if argv == ["node", "--version"] {
                let version = self.node_version.lock().unwrap().clone();
                let version = version.unwrap_or_else(|| "v22.19.0\n".into());
                let _ = out.try_send(PipeEvent::Stdout(version.into_bytes()));
                let _ = out.try_send(PipeEvent::Exit(0));
                return Ok(Pipe { input, events });
            }
            if argv == launch::profile_argv() {
                self.profiles.lock().unwrap().push(argv.to_vec());
                let _ = out.try_send(PipeEvent::Exit(0));
                return Ok(Pipe { input, events });
            }
            if argv == launch::trust_argv() {
                *self.trusts.lock().unwrap() += 1;
                if *self.trust_fail.lock().unwrap() {
                    let _ = out.try_send(PipeEvent::Stderr(b"the image has no system CA bundle\n".to_vec()));
                    let _ = out.try_send(PipeEvent::Exit(3));
                } else {
                    let _ = out.try_send(PipeEvent::Exit(0));
                }
                return Ok(Pipe { input, events });
            }
            if argv.len() == 3 && argv[0] == "node" && argv[2] == "--version" {
                if self.probe_fail.lock().unwrap().contains(&argv[1]) {
                    let _ =
                        out.try_send(PipeEvent::Stderr(b"Error: Cannot find module './darwin-arm64.node'\n".to_vec()));
                    let _ = out.try_send(PipeEvent::Exit(1));
                } else {
                    let _ = out.try_send(PipeEvent::Stdout(b"1.0.0\n".to_vec()));
                    let _ = out.try_send(PipeEvent::Exit(0));
                }
                return Ok(Pipe { input, events });
            }
            let gate = self.pty_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            self.pipes.lock().unwrap().push(FakePipe { argv: argv.to_vec(), stdin, out });
            Ok(Pipe { input, events })
        }
    }
}

#[cfg(test)]
mod tests {
    use microsandbox::NetworkAction;
    use serde_json::json;

    use super::*;

    #[test]
    fn the_vm_reaches_only_the_proxy_port() {
        let policy = sandbox_policy(41234).unwrap();
        assert_eq!(policy.default_egress, NetworkAction::Deny);
        assert_eq!(policy.default_ingress, NetworkAction::Deny);
        assert_eq!(
            serde_json::to_value(&policy.rules).unwrap(),
            json!([{
                "direction": "egress",
                "destination": { "group": "host" },
                "protocols": ["tcp"],
                "ports": [{ "start": 41234, "end": 41234 }],
                "action": "allow",
            }])
        );
        // No name resolves: the guest finds the proxy in /etc/hosts.
        let dns = |name: &str| {
            policy.evaluate_dns_query(&name.parse().unwrap(), serde_json::from_value(json!("udp")).unwrap(), 53)
        };
        assert_eq!(dns("example.com"), NetworkAction::Deny);
        assert_eq!(dns("host.microsandbox.internal"), NetworkAction::Deny);
    }

    #[test]
    fn sessions_run_a_login_shell_or_an_agent_wrapper() {
        assert_eq!(SessionKind::Shell.argv(), ["/bin/bash", "-l"]);
        let claude = SessionKind::Agent { agent: AgentKind::Claude, resume: false };
        assert_eq!(claude.argv(), ["/opt/zwinger/bin/claude"]);
        let codex = SessionKind::Agent { agent: AgentKind::Codex, resume: true };
        assert_eq!(codex.argv(), ["/opt/zwinger/bin/codex", "resume", "--last"]);
        let gemini = SessionKind::Agent { agent: AgentKind::Gemini, resume: true };
        assert_eq!(gemini.argv(), ["/opt/zwinger/bin/gemini", "--resume", "latest"]);
        let pi = SessionKind::Agent { agent: AgentKind::Pi, resume: true };
        assert_eq!(pi.argv(), ["/opt/zwinger/bin/pi", "-c"]);
        assert_eq!(serde_json::to_value(codex).unwrap(), json!({ "type": "agent", "agent": "codex", "resume": true }));
        assert_eq!(serde_json::from_value::<SessionKind>(json!({ "type": "shell" })).unwrap(), SessionKind::Shell);
        assert_eq!(serde_json::to_value(SessionKind::Host).unwrap(), json!({ "type": "host" }));
        assert_eq!(serde_json::from_value::<SessionKind>(json!({ "type": "host" })).unwrap(), SessionKind::Host);
        assert_eq!(SessionKind::Host.argv()[1], "-l");
    }

    #[test]
    fn pty_execs_get_the_wrappers_on_path_and_true_colour() {
        let env: BTreeMap<_, _> = PTY_ENV.into_iter().collect();
        assert!(env["PATH"].starts_with("/opt/zwinger/bin:"));
        assert!(env["PATH"].split(':').any(|d| d == "/usr/local/bin"), "node is in /usr/local/bin");
        assert_eq!((env["TERM"], env["COLORTERM"]), ("xterm-256color", "truecolor"));
    }

    #[test]
    fn pipes_run_in_the_group_wrapper() {
        let argv = group_argv(&["claude".to_owned(), "-p".to_owned()]);
        assert_eq!(argv, ["node", "-e", GROUP_WRAPPER, "--", "claude", "-p"]);
    }

    /// Runs the wrapper with the host node, as the guest does.
    #[cfg(unix)]
    #[test]
    fn group_wrapper_keeps_stdio_and_codes_and_kills_the_group() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};

        let run = |script: &str| {
            let argv = group_argv(&["sh".to_owned(), "-c".to_owned(), script.to_owned()]);
            Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap()
        };
        let mut echo = run("read line; echo \"got $line\"; exit 3");
        echo.stdin.take().unwrap().write_all(b"hi\n").unwrap();
        let out = echo.wait_with_output().unwrap();
        assert_eq!((out.status.code(), out.stdout.as_slice()), (Some(3), b"got hi\n".as_slice()));

        // A grandchild, as a tool process of an agent is.
        let mut wrapper = run("sleep 30 & echo $!; wait");
        let mut line = String::new();
        BufReader::new(wrapper.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let sleep = line.trim().to_owned();
        let alive = || {
            let ps = Command::new("ps").args(["-o", "stat=", "-p", &sleep]).output().unwrap();
            let stat = String::from_utf8_lossy(&ps.stdout).trim().to_owned();
            !stat.is_empty() && !stat.starts_with('Z')
        };
        assert!(alive());
        assert!(Command::new("kill").args(["-TERM", &wrapper.id().to_string()]).status().unwrap().success());
        assert_eq!(wrapper.wait().unwrap().code(), Some(137));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive() {
            assert!(std::time::Instant::now() < deadline, "the grandchild is still running");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
