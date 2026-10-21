use std::future::Future;

use std::collections::BTreeMap;

use microsandbox::sandbox::{DeploymentProfile, MAX_SANDBOX_LIST_LIMIT, SecretBuilder};
use microsandbox::{ExecEvent, Sandbox};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::error::{Error, Result};
use crate::secrets::{Secret, SecretValue};
use crate::shield::{self, NetworkMode, Shield, WORKSPACE};

const STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
const KILL_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);
/// Where microsandbox puts its TLS interception CA in the guest
/// (`microsandbox_protocol::GUEST_TLS_CA_PATH`, not re-exported).
const GUEST_TLS_CA_PATH: &str = "/.msb/tls/ca.pem";
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

    /// `secrets` comes from [`secret_entries`].
    fn start(
        &self,
        shield: &Shield,
        secrets: Vec<SecretBuilder>,
    ) -> impl Future<Output = Result<Self::Vm>> + Send;
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

/// Pairs each secret with its value. The guest gets the placeholder in the
/// variable; the proxy replaces it with the value only in the selected places
/// of requests to the secret's hosts, and only after the TLS identity check.
/// Checks each secret again, because `SecretBuilder::allow` panics on an
/// invalid host and the stored config can be edited by hand.
pub fn secret_entries(secrets: &[Secret], values: Vec<SecretValue>) -> Result<Vec<SecretBuilder>> {
    secrets
        .iter()
        .zip(values)
        .map(|(secret, value)| {
            let secret = secret.clone().normalize()?;
            let builder = SecretBuilder::new()
                .env(&secret.name)
                .placeholder(secret.placeholder())
                .value(value.expose());
            // An empty field list means all header fields to microsandbox.
            let builder = if secret.places.header_fields.is_empty() {
                builder.substitute_in_headers(false)
            } else {
                builder.substitute_in_header_fields(&secret.places.header_fields)
            };
            let builder = builder
                .substitute_in_query(secret.places.query)
                .substitute_in_body(secret.places.body);
            Ok(secret.hosts.iter().fold(builder, |b, host| b.allow(host)))
        })
        .collect()
}

const LABEL: (&str, &str) = ("app", "zwinger");
const NAME_PREFIX: &str = "zwinger-";

pub struct Microsandbox;

impl SandboxRuntime for Microsandbox {
    type Vm = Sandbox;

    async fn start(&self, shield: &Shield, secrets: Vec<SecretBuilder>) -> Result<Sandbox> {
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
        builder = builder.network(|n| n.policy(policy));
        // Secrets turn on TLS interception. Node does not read the system
        // trust store, so it gets the interception CA explicitly.
        if !secrets.is_empty() {
            builder = builder.env("NODE_EXTRA_CA_CERTS", GUEST_TLS_CA_PATH);
        }
        for secret in secrets {
            builder = builder.secret_entry(secret.build());
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

    use microsandbox::HostPattern;

    use super::*;

    use tokio::sync::Notify;

    /// The name and the hosts of a secret.
    pub type StartedSecret = (String, Vec<HostPattern>);

    /// Echoes input back as output. `Kill` ends the process with code 137.
    #[derive(Default, Clone)]
    pub struct FakeRuntime {
        pub log: Arc<Mutex<Vec<String>>>,
        pub fail_stop: Arc<Mutex<bool>>,
        pub pty_gate: Arc<Mutex<Option<Arc<PtyGate>>>>,
        pub cwds: Arc<Mutex<Vec<String>>>,
        pub start_gate: Arc<Mutex<Option<Arc<PtyGate>>>>,
        /// The names and hosts of the secrets of the last start, never the values.
        pub secrets: Arc<Mutex<Vec<StartedSecret>>>,
    }

    /// Holds `open_pty` or `start` after it signals `entered`, until the test
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

        async fn start(&self, shield: &Shield, secrets: Vec<SecretBuilder>) -> Result<FakeVm> {
            let gate = self.start_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            self.log.lock().unwrap().push(format!("start {}", shield.id));
            let mut started = self.secrets.lock().unwrap();
            started.clear();
            for secret in secrets {
                let entry = secret.build();
                started.push((entry.env_var, entry.allowed_hosts));
            }
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
    use microsandbox::{HostPattern, NetworkAction};
    use serde_json::{Value, json};

    use super::*;
    use crate::secrets::Places;

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
    fn secret_entries_map_places_and_hosts() {
        let secrets = [
            Secret {
                name: "OPENAI_API_KEY".into(),
                hosts: vec!["api.openai.com".into(), "openai.com".into()],
                places: Places {
                    header_fields: vec!["Authorization".into(), "x-api-key".into()],
                    query: false,
                    body: false,
                },
            },
            Secret {
                name: "MAPS_KEY".into(),
                hosts: vec!["maps.example.com".into()],
                places: Places {
                    header_fields: vec![],
                    query: true,
                    body: true,
                },
            },
        ];
        let values = vec![SecretValue::new("sk-1"), SecretValue::new("mk-2")];
        let entries: Vec<_> = secret_entries(&secrets, values)
            .unwrap()
            .into_iter()
            .map(SecretBuilder::build)
            .collect();

        assert_eq!(entries[0].env_var, "OPENAI_API_KEY");
        assert_eq!(*entries[0].value, "sk-1");
        assert_eq!(entries[0].placeholder, "$MSB_OPENAI_API_KEY$");
        assert_eq!(
            entries[0].allowed_hosts,
            [
                HostPattern::Exact("api.openai.com".into()),
                HostPattern::Exact("openai.com".into())
            ]
        );
        let s = &entries[0].substitution;
        assert!(s.headers && !s.query && !s.body);
        assert_eq!(s.header_fields, ["authorization", "x-api-key"]);

        assert_eq!(*entries[1].value, "mk-2");
        let s = &entries[1].substitution;
        assert!(!s.headers && s.header_fields.is_empty() && s.query && s.body);

        for entry in &entries {
            assert!(entry.require_tls_identity);
            assert!(entry.passthrough_hosts.is_empty());
        }
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
