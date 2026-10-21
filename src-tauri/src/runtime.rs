use std::future::Future;

use std::collections::BTreeMap;

use microsandbox::sandbox::{DeploymentProfile, MAX_SANDBOX_LIST_LIMIT};
use microsandbox::{ExecEvent, Sandbox};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::error::{Error, Result};
use crate::shield::{self, NetworkMode, Shield, WORKSPACE};

const STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
const KILL_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);
const IMAGE: &str = "node:22-slim";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionKind {
    Shell,
    Pi,
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
    pub events: UnboundedReceiver<PtyEvent>,
}

/// The seam between the shield manager and the VM backend, so tests do not boot VMs.
pub trait SandboxRuntime: Send + Sync + 'static {
    type Vm: Send + Sync + 'static;

    fn start(&self, shield: &Shield) -> impl Future<Output = Result<Self::Vm>> + Send;
    fn stop(&self, vm: &Self::Vm) -> impl Future<Output = Result<()>> + Send;
    fn open_pty(
        &self,
        vm: &Self::Vm,
        kind: SessionKind,
        cwd: &str,
        rows: u16,
        cols: u16,
    ) -> impl Future<Output = Result<Pty>> + Send;
    /// Stops and removes the VMs that an earlier app run left behind.
    fn reap(&self) -> impl Future<Output = Result<()>> + Send;
}

/// Builds the tenant policy. The MultiTenant profile adds a host-owned public-only
/// floor on top. Host, LAN, loopback, link-local and metadata are denied first in
/// every mode. The Host deny skips TCP/UDP port 53: the DNS interceptor evaluates
/// queries against Host rules, so a full Host deny would refuse every query.
pub fn sandbox_policy(policy: &shield::NetworkPolicy) -> Result<microsandbox::NetworkPolicy> {
    let (suffixes, domains): (Vec<&str>, Vec<&str>) = policy
        .domains
        .iter()
        .map(String::as_str)
        .partition(|d| d.starts_with("*."));
    let builder = microsandbox::NetworkPolicy::builder()
        .default_deny()
        .egress(|e| e.tcp().udp().port_range(0, 52).port_range(54, u16::MAX).deny_host())
        .egress(|e| e.icmpv4().icmpv6().deny_host())
        .egress(|e| e.deny_private().deny_loopback().deny_link_local().deny_meta());
    let builder = match policy.mode {
        NetworkMode::Deny => builder,
        NetworkMode::List => builder.egress(|e| {
            e.allow_domains(domains)
                .allow_domain_suffixes(suffixes.into_iter().map(|s| &s[2..]))
        }),
        NetworkMode::Public => builder
            .rule(|r| r.egress().udp().tcp().port(53).allow_host())
            .egress(|e| e.allow_public()),
    };
    builder
        .build()
        .map_err(|e| Error::InvalidDomain(policy.domains.join(", "), e.to_string()))
}

const LABEL: (&str, &str) = ("app", "zwinger");
const NAME_PREFIX: &str = "zwinger-";

pub struct Microsandbox;

impl SandboxRuntime for Microsandbox {
    type Vm = Sandbox;

    async fn start(&self, shield: &Shield) -> Result<Sandbox> {
        let policy = sandbox_policy(&shield.network)?;
        let mut builder = Sandbox::builder(format!("{NAME_PREFIX}{}", shield.id))
            .image(IMAGE)
            .label(LABEL.0, LABEL.1)
            .deployment_profile(DeploymentProfile::MultiTenant)
            .ephemeral(true)
            .replace()
            .workdir(WORKSPACE)
            .volume(WORKSPACE, |m| m.bind(&shield.folder));
        for mount in &shield.mounts {
            builder = builder.volume(&mount.guest, |m| {
                let m = m.bind(&mount.host);
                if mount.readonly { m.readonly() } else { m }
            });
        }
        Ok(builder.network(|n| n.policy(policy)).create().await?)
    }

    /// A graceful stop has no deadline in microsandbox, so a stuck VM is
    /// killed. The shield then always leaves the Stopping state.
    async fn stop(&self, vm: &Sandbox) -> Result<()> {
        if vm.stop_with_timeout(STOP_LIMIT).await.is_ok() {
            return Ok(());
        }
        Ok(vm.kill_with_timeout(KILL_LIMIT).await?)
    }

    async fn reap(&self) -> Result<()> {
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
        let mut result = Ok(());
        for handle in found.into_values() {
            if let Err(e) = handle.destroy().await {
                result = Err(e.into());
            }
        }
        result
    }

    async fn open_pty(
        &self,
        vm: &Sandbox,
        kind: SessionKind,
        cwd: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Pty> {
        let (cmd, args): (&str, &[&str]) = match kind {
            SessionKind::Shell => ("/bin/bash", &["-l"]),
            SessionKind::Pi => ("npx", &["-y", "@earendil-works/pi-coding-agent"]),
        };
        let mut handle = vm
            .exec_stream_with(cmd, |e| {
                e.args(args.iter().copied())
                    .cwd(cwd)
                    .env("TERM", "xterm-256color")
                    .tty(true)
                    .stdin_pipe()
            })
            .await?;
        let sink = handle.take_stdin().ok_or(Error::SessionClosed)?;
        let control = handle.control();
        control.resize(rows, cols).await?;

        let (input, mut input_rx) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();

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
                        let _ = events_tx.send(PtyEvent::Output(bytes.to_vec()));
                    }
                    ExecEvent::Exited { code: c } => {
                        code = c;
                        break;
                    }
                    ExecEvent::Failed(failed) => {
                        let message = format!("{}\r\n", failed.message);
                        let _ = events_tx.send(PtyEvent::Output(message.into_bytes()));
                        break;
                    }
                    ExecEvent::Started { .. } | ExecEvent::StdinError(_) => {}
                }
            }
            let _ = events_tx.send(PtyEvent::Exit(code));
        });

        Ok(Pty { input, events })
    }
}

#[cfg(test)]
pub mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    use tokio::sync::Notify;

    /// Echoes input back as output. `Kill` ends the process with code 137.
    #[derive(Default, Clone)]
    pub struct FakeRuntime {
        pub log: Arc<Mutex<Vec<String>>>,
        pub fail_stop: Arc<Mutex<bool>>,
        pub pty_gate: Arc<Mutex<Option<Arc<PtyGate>>>>,
        pub cwds: Arc<Mutex<Vec<String>>>,
    }

    /// Holds `open_pty` after it signals `entered`, until the test signals `release`.
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

        async fn start(&self, shield: &Shield) -> Result<FakeVm> {
            self.log.lock().unwrap().push(format!("start {}", shield.id));
            Ok(FakeVm {
                shield_id: shield.id.clone(),
            })
        }

        async fn stop(&self, vm: &FakeVm) -> Result<()> {
            if *self.fail_stop.lock().unwrap() {
                return Err(Error::Io(std::io::Error::other("stop failed")));
            }
            self.log.lock().unwrap().push(format!("stop {}", vm.shield_id));
            Ok(())
        }

        async fn reap(&self) -> Result<()> {
            self.log.lock().unwrap().push("reap".into());
            Ok(())
        }

        async fn open_pty(
            &self,
            _: &FakeVm,
            _: SessionKind,
            cwd: &str,
            rows: u16,
            cols: u16,
        ) -> Result<Pty> {
            self.cwds.lock().unwrap().push(cwd.to_owned());
            let gate = self.pty_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            let log = self.log.clone();
            let (input, mut input_rx) = mpsc::unbounded_channel();
            let (tx, events) = mpsc::unbounded_channel();
            let _ = tx.send(PtyEvent::Output(format!("{rows}x{cols}").into_bytes()));
            tokio::spawn(async move {
                while let Some(msg) = input_rx.recv().await {
                    match msg {
                        PtyInput::Data(data) => {
                            let _ = tx.send(PtyEvent::Output(data));
                        }
                        PtyInput::Resize { rows, cols } => {
                            let _ = tx.send(PtyEvent::Output(format!("{rows}x{cols}").into_bytes()));
                        }
                        PtyInput::Kill => {
                            log.lock().unwrap().push("kill".into());
                            let _ = tx.send(PtyEvent::Exit(137));
                            return;
                        }
                    }
                }
            });
            Ok(Pty { input, events })
        }
    }
}

#[cfg(test)]
mod tests {
    use microsandbox::NetworkAction;
    use serde_json::{Value, json};

    use super::*;

    fn policy(mode: NetworkMode, domains: &[&str]) -> microsandbox::NetworkPolicy {
        sandbox_policy(&shield::NetworkPolicy {
            mode,
            domains: domains.iter().map(|d| (*d).to_owned()).collect(),
        })
        .unwrap()
    }

    fn rules(policy: &microsandbox::NetworkPolicy) -> Value {
        serde_json::to_value(&policy.rules).unwrap()
    }

    /// The DNS interceptor's decision for a UDP/53 query.
    fn dns(policy: &microsandbox::NetworkPolicy, name: &str) -> NetworkAction {
        policy.evaluate_dns_query(
            &name.parse().unwrap(),
            serde_json::from_value(json!("udp")).unwrap(),
            53,
        )
    }

    fn deny(destination: Value, protocols: Value, ports: Value) -> Value {
        json!({
            "direction": "egress",
            "destination": destination,
            "protocols": protocols,
            "ports": ports,
            "action": "deny",
        })
    }

    fn allow(destination: Value) -> Value {
        json!({
            "direction": "egress",
            "destination": destination,
            "protocols": [],
            "ports": [],
            "action": "allow",
        })
    }

    fn floor() -> Vec<Value> {
        let host = json!({ "group": "host" });
        let mut rules = vec![
            deny(
                host.clone(),
                json!(["tcp", "udp"]),
                json!([{ "start": 0, "end": 52 }, { "start": 54, "end": 65535 }]),
            ),
            deny(host, json!(["icmpv4", "icmpv6"]), json!([])),
        ];
        for group in ["private", "loopback", "link_local", "metadata"] {
            rules.push(deny(json!({ "group": group }), json!([]), json!([])));
        }
        rules
    }

    #[test]
    fn every_mode_denies_by_default_and_starts_with_the_floor() {
        for policy in [
            policy(NetworkMode::Deny, &["registry.npmjs.org"]),
            policy(NetworkMode::List, &[]),
            policy(NetworkMode::List, &["registry.npmjs.org"]),
            policy(NetworkMode::Public, &[]),
        ] {
            assert_eq!(policy.default_egress, NetworkAction::Deny);
            assert_eq!(policy.default_ingress, NetworkAction::Deny);
            assert_eq!(rules(&policy).as_array().unwrap()[..6], floor()[..]);
        }
    }

    #[test]
    fn allow_list_maps_domains_and_wildcards_without_open_dns() {
        let policy = policy(NetworkMode::List, &["registry.npmjs.org", "*.anthropic.com"]);
        let mut expected = floor();
        expected.push(allow(json!({ "domain": "registry.npmjs.org" })));
        expected.push(allow(json!({ "domain_suffix": "anthropic.com" })));
        assert_eq!(rules(&policy), Value::Array(expected));
    }

    #[test]
    fn allow_list_resolves_only_listed_names() {
        let policy = policy(NetworkMode::List, &["registry.npmjs.org", "*.anthropic.com"]);
        assert_eq!(dns(&policy, "registry.npmjs.org"), NetworkAction::Allow);
        assert_eq!(dns(&policy, "api.anthropic.com"), NetworkAction::Allow);
        assert_eq!(dns(&policy, "anthropic.com"), NetworkAction::Allow);
        assert_eq!(dns(&policy, "example.com"), NetworkAction::Deny);
        assert_eq!(dns(&policy, "npmjs.org"), NetworkAction::Deny);
        assert_eq!(dns(&policy, "host.microsandbox.internal"), NetworkAction::Deny);
    }

    #[test]
    fn empty_allow_list_resolves_nothing() {
        assert_eq!(dns(&policy(NetworkMode::List, &[]), "registry.npmjs.org"), NetworkAction::Deny);
    }

    #[test]
    fn deny_allows_no_domains() {
        let policy = policy(NetworkMode::Deny, &["registry.npmjs.org", "*.anthropic.com"]);
        assert_eq!(rules(&policy), Value::Array(floor()));
        assert_eq!(dns(&policy, "registry.npmjs.org"), NetworkAction::Deny);
        assert_eq!(dns(&policy, "api.anthropic.com"), NetworkAction::Deny);
    }

    #[test]
    fn public_opens_dns_and_public_only() {
        let policy = policy(NetworkMode::Public, &["registry.npmjs.org"]);
        let mut expected = floor();
        expected.push(json!({
            "direction": "egress",
            "destination": { "group": "host" },
            "protocols": ["udp", "tcp"],
            "ports": [{ "start": 53, "end": 53 }],
            "action": "allow",
        }));
        expected.push(allow(json!({ "group": "public" })));
        assert_eq!(rules(&policy), Value::Array(expected));
        assert_eq!(
            rules(&policy)[6],
            serde_json::to_value(microsandbox::NetworkRule::allow_dns()).unwrap()
        );
        assert_eq!(dns(&policy, "example.com"), NetworkAction::Allow);
    }

    #[test]
    fn invalid_domain_is_rejected() {
        let result = sandbox_policy(&shield::NetworkPolicy {
            mode: NetworkMode::List,
            domains: vec!["not a domain".into()],
        });
        assert!(matches!(result, Err(Error::InvalidDomain(..))));
    }
}
