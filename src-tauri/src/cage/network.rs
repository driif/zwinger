use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::cage::manager::{Manager, Vm, lock};
use crate::cage::{self, CLAUDE_TOKEN, Cage, HostAgentUse, WORKSPACE};
use crate::error::{Error, Result};
use crate::network::{Action, Credential, DefaultAction, Inject, NetworkPolicy, RuleDraft, Source, Target};
use crate::proxy::log::Entry;
use crate::runtime::{PipeEvent, PipeInput, SandboxRuntime};
use crate::secrets::{self, CredentialRef, Owner, SecretValue};

/// How long the network test may run in the guest.
pub(crate) const NET_TEST_LIMIT: Duration = Duration::from_secs(25);

/// The bytes of the network test output that are read.
pub(crate) const NET_TEST_OUT: usize = 8 << 10;

/// One stage of the network test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetStage {
    pub ok: bool,
    pub detail: String,
}

/// The network test: the cage reaches its proxy, then the host through it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetTest {
    pub proxy: NetStage,
    /// `None` if the first stage failed.
    pub target: Option<NetStage>,
}

impl<R: SandboxRuntime> Manager<R> {
    /// The network log of VM `key`. It stays after a stop.
    pub fn list_net_events(&self, key: &str) -> Result<Vec<Entry>> {
        let state = lock(&self.state);
        Ok(state.net.get(key).map(|ring| ring.iter().cloned().collect()).unwrap_or_default())
    }

    /// Applies `edit` to the network policy and saves it. Each running
    /// proxy of the cage uses the new policy from its next request.
    pub(crate) fn edit_network<T>(&self, id: &str, edit: impl FnOnce(&mut NetworkPolicy) -> Result<T>) -> Result<T> {
        let mut out = None;
        self.edit_cage(id, |c, _| {
            out = Some(edit(&mut c.network)?);
            Ok(())
        })?;
        self.publish(id, None);
        Ok(out.expect("the edit ran"))
    }

    /// Sends the stored policy to the proxy of each VM of the cage, in one
    /// step with the new `value` of a credential.
    pub(crate) fn publish(&self, id: &str, value: Option<(&CredentialRef, SecretValue)>) {
        let state = lock(&self.state);
        let Ok(cage) = state.cage(id) else {
            return;
        };
        for live in state.lives_of(id) {
            live.edit(|policy, values| {
                *policy = cage.network.clone();
                if let Some((key, value)) = &value {
                    values.insert((*key).clone(), value.clone());
                }
            });
        }
    }

    pub fn set_network_default(&self, id: &str, default: DefaultAction) -> Result<()> {
        self.edit_network(id, |n| {
            n.default = default;
            Ok(())
        })
    }

    pub fn set_offline(&self, id: &str, offline: bool) -> Result<()> {
        self.edit_network(id, |n| {
            n.offline = offline;
            Ok(())
        })
    }

    /// Adds or changes a rule. Returns its id.
    pub fn put_rule(&self, id: &str, draft: RuleDraft) -> Result<String> {
        self.edit_network(id, |n| n.put_rule(draft))
    }

    /// Saves the config first, then deletes the values that no other rule
    /// uses, so that a failure leaves a value with no credential.
    pub fn remove_rule(&self, id: &str, rule_id: &str) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        let removed = self.edit_network(id, |n| n.remove_rule(rule_id))?;
        self.drop_unused(id, removed.credentials.iter().map(Credential::key))
    }

    /// Removes the values of `keys` that no credential of the cage uses from
    /// its running proxies. A local value also leaves the vault; a library
    /// value stays for the other cages.
    pub(crate) fn drop_unused(&self, id: &str, keys: impl IntoIterator<Item = CredentialRef>) -> Result<()> {
        let (network, lives) = {
            let state = lock(&self.state);
            (state.cage(id)?.network.clone(), state.lives_of(id))
        };
        let mut result = Ok(());
        for key in keys.into_iter().filter(|k| !network.uses(k)) {
            for live in &lives {
                live.set_value(&key, None);
            }
            if !key.source.is_local() {
                continue;
            }
            match self.vault.delete(&Owner::cage(id), &key.name) {
                Ok(()) => {
                    lock(&self.unsettled).remove(&key.account(id));
                }
                Err(e) => result = Err(e),
            }
        }
        result
    }

    /// Adds, changes or (with `inject` `None`) removes the credential `name`
    /// of the rule. A local credential with a new name, or with no stored
    /// value, needs a value; `None` keeps the current value. A library
    /// credential takes no value: the library entry must exist.
    /// The vault is written before the config, and a failed config save
    /// restores the vault. A running proxy uses the value from its next request.
    pub fn set_rule_credential(
        &self,
        id: &str,
        rule_id: &str,
        name: &str,
        inject: Option<Inject>,
        source: Source,
        value: Option<SecretValue>,
    ) -> Result<()> {
        let Some(inject) = inject else {
            let _edit = lock(&self.secret_edit);
            let removed = self.edit_network(id, |n| {
                let rule = n.rule_mut(rule_id)?;
                let i = rule.credentials.iter().position(|c| c.name == name);
                let i = i.ok_or_else(|| Error::SecretNotFound(name.to_owned()))?;
                Ok(rule.credentials.remove(i).key())
            })?;
            return self.drop_unused(id, [removed]);
        };
        let credential = Credential { name: name.to_owned(), inject, source }.normalize()?;
        let key = credential.key();
        check_value(&key, value.as_ref())?;
        self.bind(id, &key, value, |c| {
            let rule = c.network.rule_mut(rule_id)?;
            Ok(replace(&mut rule.credentials, &credential))
        })
    }

    /// Binds the Claude token to an exact Allow rule for api.anthropic.com.
    /// A local token needs `value`; a library token takes none and leaves
    /// the cage's local account alone.
    pub fn set_claude_token(
        &self,
        id: &str,
        value: Option<SecretValue>,
        source: Source,
        allow: bool,
        profile: bool,
    ) -> Result<()> {
        const HOST: &str = "api.anthropic.com";
        if self.host.last().agent(AgentKind::Claude).is_none() {
            return Err(Error::HostAgentMissing("Claude Code".into()));
        }
        let key = CredentialRef { source, name: CLAUDE_TOKEN.into() };
        let value = value.map(|v| SecretValue::new(v.expose().trim()));
        if source.is_local() && value.is_none() {
            return Err(Error::SecretValueRequired(CLAUDE_TOKEN.into()));
        }
        check_value(&key, value.as_ref())?;
        let credential = Credential {
            name: CLAUDE_TOKEN.into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
            source,
        };
        self.bind(id, &key, value, |cage| {
            let network = &mut cage.network;
            let exact = network
                .rules
                .iter()
                .position(|r| r.action == Action::Allow && matches!(&r.target, Target::Host { host } if host == HOST));
            let index = match exact {
                Some(index) => index,
                None => {
                    let exact_block = network.rules.iter().any(|rule| {
                        rule.action == Action::Block && matches!(&rule.target, Target::Host { host } if host == HOST)
                    });
                    if (network.default == DefaultAction::Block || exact_block) && !allow {
                        return Err(Error::InvalidRule("allow api.anthropic.com first".into()));
                    }
                    let id = network.put_rule(RuleDraft {
                        id: None,
                        target: Target::Host { host: HOST.into() },
                        action: Action::Allow,
                    })?;
                    network.rules.iter().position(|r| r.id == id).expect("new rule")
                }
            };
            let replaced = replace(&mut network.rules[index].credentials, &credential);
            cage.put_host_agent(HostAgentUse { kind: AgentKind::Claude, profile, login: true }, None);
            Ok(replaced)
        })
    }

    /// Saves `put` with the value of `key`, under `secret_edit`. A `value` is
    /// written to the vault before the config, and a failed config save
    /// restores the vault. With no `value` the stored value is used: a local
    /// one only if the cage already uses it. `put` returns the keys that it
    /// replaced; their values go if no other credential uses them.
    fn bind(
        &self,
        id: &str,
        key: &CredentialRef,
        value: Option<SecretValue>,
        put: impl Fn(&mut Cage) -> Result<Vec<CredentialRef>>,
    ) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        let owner = key.vault_key(id);
        self.settle(&owner, &key.name)?;
        let is_new = {
            let state = lock(&self.state);
            let mut cage = state.cage(id)?.clone();
            let is_new = !cage.network.uses(key);
            put(&mut cage)?;
            cage.check()?;
            is_new
        };
        let prior = self.vault.entry(&owner, &key.name)?;
        let mut replaced = Vec::new();
        let mut save = || self.edit_cage(id, |c, _| put(c).map(|keys| replaced = keys));
        let value = match value {
            Some(value) => {
                // A failed set can still have changed the value, for example when
                // the file vault fails after its rename. The restore is safe either way.
                if let Err(e) = self.vault.set(&owner, &key.name, &value).and_then(|()| save()) {
                    self.restore_value(&owner, &key.name, prior.as_ref());
                    return Err(e);
                }
                value
            }
            None => {
                let prior = match prior {
                    Some(prior) if !(is_new && key.source.is_local()) => prior,
                    None if !key.source.is_local() => {
                        return Err(Error::LibraryCredentialNotFound(key.name.clone()));
                    }
                    _ => return Err(Error::SecretValueRequired(key.name.clone())),
                };
                save()?;
                prior.value
            }
        };
        // A request must not see the new rule with the old value.
        self.publish(id, Some((key, value)));
        self.drop_unused(id, replaced)
    }

    /// The names of the cage's local credentials that have a stored value.
    /// Returns no values.
    pub fn stored_credentials(&self, id: &str) -> Result<Vec<String>> {
        let names: BTreeSet<String> = {
            let state = lock(&self.state);
            let network = &state.cage(id)?.network;
            network.credentials().into_iter().filter(|c| c.source.is_local()).map(|c| c.name.clone()).collect()
        };
        let stored: HashSet<String> = self
            .vault
            .accounts()?
            .into_iter()
            .filter(|(owner, _)| *owner == Owner::cage(id))
            .map(|(_, name)| name)
            .collect();
        Ok(names.into_iter().filter(|name| stored.contains(name)).collect())
    }

    /// The entries of the credential library, by name. Returns no values.
    pub fn library_credentials(&self) -> Result<Vec<LibraryCredential>> {
        let mut entries = Vec::new();
        for (owner, name) in self.vault.accounts()? {
            if owner == Owner::Library {
                let saved_at = self.vault.entry(&Owner::Library, &name)?.and_then(|e| e.saved_at);
                entries.push(LibraryCredential { name, saved_at });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Adds a library value, or with `rotate` replaces one. An add of an
    /// existing name and a rotation of a missing name are refused, so a stale
    /// listing cannot replace or create a value by mistake. Creating one
    /// attaches it nowhere. Each running proxy of a cage that attached it uses
    /// the new value from its next request, and a start waits for the save.
    pub fn set_library_credential(&self, name: &str, value: SecretValue, rotate: bool) -> Result<()> {
        let key = CredentialRef::library(&secrets::check_name(name)?);
        if value.expose().is_empty() {
            return Err(Error::SecretValueRequired(key.name));
        }
        let _edit = lock(&self.secret_edit);
        self.settle(&Owner::Library, &key.name)?;
        let prior = self.vault.entry(&Owner::Library, &key.name)?;
        match (rotate, prior.is_some()) {
            (false, true) => return Err(Error::LibraryCredentialExists(key.name)),
            (true, false) => return Err(Error::LibraryCredentialNotFound(key.name)),
            _ => {}
        }
        if let Err(e) = self.vault.set(&Owner::Library, &key.name, &value) {
            self.restore_value(&Owner::Library, &key.name, prior.as_ref());
            return Err(e);
        }
        let state = lock(&self.state);
        for cage in state.cages.iter().filter(|c| c.network.uses(&key)) {
            for live in state.lives_of(&cage.id) {
                live.set_value(&key, Some(value.clone()));
            }
        }
        Ok(())
    }

    /// Deletes a library value. While a rule of a cage attaches it, the
    /// delete is refused and names those cages, so no cage loses a value
    /// that it still sends.
    pub fn delete_library_credential(&self, name: &str) -> Result<()> {
        let key = CredentialRef::library(name);
        let _edit = lock(&self.secret_edit);
        let users: Vec<String> = {
            let state = lock(&self.state);
            state.cages.iter().filter(|c| c.network.uses(&key)).map(|c| c.name.clone()).collect()
        };
        if !users.is_empty() {
            return Err(Error::LibraryCredentialInUse { name: key.name, cages: users });
        }
        self.vault.delete(&Owner::Library, &key.name)?;
        lock(&self.unsettled).remove(&(Owner::Library, key.name));
        Ok(())
    }

    /// Tests the network from inside a running VM of the cage, the shared
    /// VM first: first a TCP connect to the proxy, then a request to `host`
    /// through it. All VMs of a cage have the same policy.
    pub async fn test_network(&self, id: &str, host: &str) -> Result<NetTest> {
        let host = cage::domain(host)?;
        if host.starts_with("*.") {
            return Err(Error::InvalidDomain(host, "test one host, not a wildcard".into()));
        }
        let vm = {
            let state = lock(&self.state);
            let running = |(key, vm): (&String, &Vm<R::Vm>)| match vm {
                Vm::Running { vm, applied, .. } if applied.1.id == id => Some((key != id, vm.clone())),
                _ => None,
            };
            state
                .vms
                .iter()
                .filter_map(running)
                .min_by_key(|(own, _)| *own)
                .map(|(_, vm)| vm)
                .ok_or(Error::NotRunning)?
        };
        let argv = ["node", "-e", NET_TEST, host.as_str()].map(str::to_owned);
        let mut pipe = self.runtime.open_pipe(&vm, &argv, WORKSPACE).await?;
        let mut out = Vec::new();
        let read = async {
            while let Some(event) = pipe.events.recv().await {
                match event {
                    PipeEvent::Stdout(bytes) => {
                        let room = NET_TEST_OUT.saturating_sub(out.len());
                        out.extend(bytes.into_iter().take(room));
                    }
                    PipeEvent::Stderr(_) => {}
                    PipeEvent::Exit(_) => break,
                }
            }
        };
        if tokio::time::timeout(NET_TEST_LIMIT, read).await.is_err() {
            let _ = pipe.input.try_send(PipeInput::Kill);
        }
        Ok(net_test(&String::from_utf8_lossy(&out)))
    }

    /// Puts the prior value back after a failed edit. The old hosts must
    /// never get the new value: if the old value cannot come back, the value
    /// is removed, and the secret is then missing. If both fail, the account
    /// stays unsettled until [`Self::settle`] sees the prior value.
    pub(crate) fn restore_value(&self, owner: &Owner, name: &str, prior: Option<&secrets::VaultEntry>) {
        let restored = match prior {
            Some(prior) => self.vault.put(owner, name, prior),
            None => self.vault.delete(owner, name),
        }
        .or_else(|_| self.vault.delete(owner, name));
        if restored.is_err() {
            lock(&self.unsettled).insert((owner.clone().into_owned(), name.to_owned()), prior.cloned());
        }
    }

    /// Fails while the vault may hold a value that the saved config does not
    /// expect. An unsettled account is checked, and restored again, here.
    pub(crate) fn settle(&self, owner: &Owner, name: &str) -> Result<()> {
        let account = (owner.clone().into_owned(), name.to_owned());
        let Some(prior) = lock(&self.unsettled).get(&account).cloned() else {
            return Ok(());
        };
        let held = self.vault.entry(owner, name)?;
        if held != prior {
            match &prior {
                Some(prior) => self.vault.put(owner, name, prior),
                None => self.vault.delete(owner, name),
            }
            .or_else(|_| self.vault.delete(owner, name))?;
        }
        lock(&self.unsettled).remove(&account);
        Ok(())
    }
}

/// An empty value is no value. A library credential gets its value in the library only.
fn check_value(key: &CredentialRef, value: Option<&SecretValue>) -> Result<()> {
    match value {
        Some(value) if value.expose().is_empty() => Err(Error::SecretValueRequired(key.name.clone())),
        Some(_) if key.source == Source::Library => Err(Error::InvalidSecret(
            key.name.clone(),
            "a library credential gets its value in Settings > Credential library".into(),
        )),
        _ => Ok(()),
    }
}

/// Puts `credential` in place of the one with its name. Returns the key of
/// the replaced credential if it had the other source.
fn replace(credentials: &mut Vec<Credential>, credential: &Credential) -> Vec<CredentialRef> {
    match credentials.iter_mut().find(|c| c.name == credential.name) {
        Some(current) => {
            let old = current.key();
            *current = credential.clone();
            if old == credential.key() { Vec::new() } else { vec![old] }
        }
        None => {
            credentials.push(credential.clone());
            Vec::new()
        }
    }
}

/// An entry of the credential library. It has no value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryCredential {
    pub name: String,
    pub saved_at: Option<i64>,
}

/// The network test in the guest. It prints one JSON line per stage.
/// The target stage passes only on an answer of the host: a response of the
/// proxy itself has [`proxy::BLOCKED_HEADER`] or [`proxy::ERROR_HEADER`].
pub(crate) const NET_TEST: &str = r#"const net = require("net");
const host = process.argv[1];
const say = (stage, ok, detail) => console.log(JSON.stringify({ stage, ok, detail }));
let proxy;
try { proxy = new URL(process.env.HTTPS_PROXY); } catch { say("proxy", false, "the cage has no proxy setting"); process.exit(0); }
const socket = net.connect(Number(proxy.port), proxy.hostname);
socket.setTimeout(5000);
socket.on("timeout", () => { say("proxy", false, "no answer from " + proxy.host); socket.destroy(); });
socket.on("error", (e) => say("proxy", false, e.message));
socket.on("connect", async () => {
  socket.end();
  say("proxy", true, "reached " + proxy.host);
  try {
    const res = await fetch("https://" + host + "/", { method: "HEAD", redirect: "manual", signal: AbortSignal.timeout(15000) });
    const blocked = res.headers.get("x-zwinger-blocked");
    const error = res.headers.get("x-zwinger-error");
    say("target", !blocked && !error, blocked ? "blocked: " + blocked : error ? "failed: " + error : "HTTP " + res.status);
  } catch (e) {
    say("target", false, String((e.cause && e.cause.message) || e.message));
  }
});
"#;

/// Reads the lines of [`NET_TEST`].
pub(crate) fn net_test(out: &str) -> NetTest {
    #[derive(Deserialize)]
    struct Line {
        stage: String,
        #[serde(flatten)]
        result: NetStage,
    }
    let lines: Vec<Line> = out.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let stage = |name: &str| lines.iter().find(|l| l.stage == name).map(|l| l.result.clone());
    let proxy = stage("proxy").unwrap_or(NetStage { ok: false, detail: "the test did not answer in time".into() });
    let target = if proxy.ok {
        Some(stage("target").unwrap_or(NetStage { ok: false, detail: "the host did not answer in time".into() }))
    } else {
        None
    };
    NetTest { proxy, target }
}

/// The names of the credentials, which the guest gets as placeholder
/// variables: one per name, also for a name of two sources.
pub(crate) fn placeholders(policy: &NetworkPolicy) -> Vec<String> {
    let mut names: Vec<String> = policy.credentials().into_iter().map(|c| c.name.clone()).collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use crate::cage;
    use crate::cage::testing::*;

    #[test]
    fn the_network_test_knows_the_responses_of_the_proxy() {
        for header in [proxy::BLOCKED_HEADER, proxy::ERROR_HEADER] {
            assert!(NET_TEST.contains(&format!("res.headers.get(\"{header}\")")), "{header}");
        }
        let out = r#"{"stage":"proxy","ok":true,"detail":"reached p"}
{"stage":"target","ok":false,"detail":"failed: cannot connect to a.io in time"}"#;
        let target = net_test(out).target.unwrap();
        assert_eq!((target.ok, target.detail.as_str()), (false, "failed: cannot connect to a.io in time"));
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
        manager.set_net_sink(Box::new(move |project, event| {
            let _ = tx.send((project.to_owned(), event.clone()));
        }));
        manager.start(&id).await.unwrap();

        let mut sent = Vec::new();
        for _ in 0..2 {
            let (project, event) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
            assert_eq!(project, id);
            sent.push(event);
        }
        assert_eq!(hosts(&sent), [(1, "google.com"), (2, "example.com")]);
        assert!(sent.iter().all(|e| e.reason == crate::proxy::log::DIRECT && !e.allowed));
        assert_eq!(manager.list_net_events(&id).unwrap(), sent);

        // The ring stays after a stop; a restart continues the numbers.
        manager.stop(&id, None).await.unwrap();
        net_closed(&runtime, 0).await;
        assert_eq!(manager.list_net_events(&id).unwrap().len(), 2);
        *runtime.net_script.lock().unwrap() = vec![dns_denied("2999-01-01T00:00:01Z", "google.com")];
        manager.start(&id).await.unwrap();
        let (_, event) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        assert_eq!(event.seq, 3);

        manager.stop(&id, None).await.unwrap();
        net_closed(&runtime, 1).await;
        manager.delete(&id).await.unwrap();
        manager.delete_cage(&id).unwrap();
        assert!(lock(&manager.state).net.is_empty());
    }

    #[tokio::test]
    async fn failed_stop_keeps_the_follow_task() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        *runtime.fail_stop.lock().unwrap() = true;
        assert!(manager.stop(&id, None).await.is_err());

        net_sender(&runtime, 0).send(dns_denied("2999-01-01T00:00:00Z", "google.com")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while manager.list_net_events(&id).unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the event did not arrive");

        *runtime.fail_stop.lock().unwrap() = false;
        manager.stop(&id, None).await.unwrap();
        net_closed(&runtime, 0).await;
    }

    #[tokio::test]
    async fn network_edits_reach_the_running_proxy_with_no_restart() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let net = runtime.net.lock().unwrap().clone().unwrap();
        let env: BTreeMap<_, _> = net.env.iter().cloned().collect();
        assert!(env["HTTPS_PROXY"].ends_with(&format!("@host.microsandbox.internal:{}", net.port)));
        assert_eq!(env["NODE_USE_ENV_PROXY"], "1");

        let rule = allow(&manager, &id, "Registry.npmjs.org");
        manager.set_network_default(&id, DefaultAction::Allow).unwrap();
        manager.set_offline(&id, true).unwrap();
        let policy = live(&manager, &id).snapshot().policy;
        assert_eq!(
            (policy.default, policy.offline, policy.rules[0].id.as_str()),
            (DefaultAction::Allow, true, rule.as_str())
        );
        assert_eq!(policy.rules[0].target, Target::Host { host: "registry.npmjs.org".into() });
        assert!(!manager.list().cages[0].shared().pending_restart);

        // A new credential name needs its placeholder variable in the guest.
        manager
            .set_rule_credential(&id, &rule, "NPM_TOKEN", bearer(), Source::Local, Some(SecretValue::new("t")))
            .unwrap();
        assert!(manager.list().cages[0].shared().pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values[&local("NPM_TOKEN")], SecretValue::new("t"));
        manager.stop(&id, None).await.unwrap();
        assert!(lock(&manager.state).live.is_empty());
        manager.start(&id).await.unwrap();
        assert!(!manager.list().cages[0].shared().pending_restart);
        let net = runtime.net.lock().unwrap().clone().unwrap();
        assert!(net.env.contains(&("NPM_TOKEN".to_owned(), "$MSB_NPM_TOKEN$".to_owned())));
        assert!(!net.env.iter().any(|(_, v)| v == "t"));
    }

    #[test]
    fn rule_edits_dedupe_and_validate() {
        let (manager, _, id) = manager();
        let first = allow(&manager, &id, "registry.npmjs.org");
        assert_eq!(manager.put_rule(&id, host_rule("Registry.NPMjs.org", Action::Block)).unwrap(), first);
        allow(&manager, &id, "*.anthropic.com");
        assert!(matches!(
            manager.put_rule(&id, host_rule("https://example.com", Action::Allow)),
            Err(Error::InvalidDomain(..))
        ));
        assert!(matches!(manager.put_rule("nope", host_rule("a.io", Action::Allow)), Err(Error::CageNotFound)));
        manager.remove_rule(&id, &first).unwrap();
        assert!(matches!(manager.remove_rule(&id, &first), Err(Error::RuleNotFound)));
        let rules = manager.list().cages[0].cage.network.rules.clone();
        assert_eq!(rules.iter().map(|r| r.label()).collect::<Vec<_>>(), ["*.anthropic.com"]);
    }

    #[tokio::test]
    async fn credential_values_stay_out_of_the_config() {
        let (manager, runtime, id, vault) = manager_with_vault();
        credential(&manager, &id, "api.openai.com", " OPENAI_API_KEY ", Some("sk-hidden")).unwrap();
        assert_eq!(stored(&vault, &id, "OPENAI_API_KEY").as_deref(), Some("sk-hidden"));
        let rules = manager.list().cages[0].cage.network.rules.clone();
        assert_eq!(rules[0].credentials[0].name, "OPENAI_API_KEY");

        let config = std::fs::read_to_string(manager.store.cages_path()).unwrap();
        let listing = serde_json::to_string(&manager.list()).unwrap();
        for text in [config, listing] {
            assert!(text.contains("OPENAI_API_KEY") && !text.contains("sk-hidden"), "{text}");
        }

        manager.start(&id).await.unwrap();
        let net = runtime.net.lock().unwrap().clone().unwrap();
        assert!(net.env.contains(&("OPENAI_API_KEY".to_owned(), "$MSB_OPENAI_API_KEY$".to_owned())));
        assert!(!format!("{net:?}").contains("sk-hidden"));
        assert_eq!(live(&manager, &id).snapshot().values[&local("OPENAI_API_KEY")], SecretValue::new("sk-hidden"));
    }

    #[test]
    fn a_credential_needs_a_value_only_when_new() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        let set = |name: &str, inject: Option<Inject>, value: Option<SecretValue>| {
            manager.set_rule_credential(&id, &rule, name, inject, Source::Local, value)
        };
        assert!(matches!(set("KEY", bearer(), None), Err(Error::SecretValueRequired(_))));
        assert!(matches!(set("KEY", bearer(), value("")), Err(Error::SecretValueRequired(_))));
        assert!(manager.list().cages[0].cage.network.rules[0].credentials.is_empty());

        set("KEY", bearer(), value("v1")).unwrap();
        let replace = Inject::Replace {
            places: secrets::Places { header_fields: vec!["x-api-key".into()], query: false, body: false },
        };
        set("KEY", Some(replace.clone()), None).unwrap();
        assert_eq!(manager.list().cages[0].cage.network.rules[0].credentials[0].inject, replace);
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v1"));

        assert!(matches!(set("PATH", bearer(), value("x")), Err(Error::InvalidSecret(..))));
        assert!(matches!(
            manager.set_rule_credential("nope", &rule, "KEY", bearer(), Source::Local, value("x")),
            Err(Error::CageNotFound)
        ));
        let wildcard = manager.put_rule(&id, host_rule("*.b.io", Action::Allow)).unwrap();
        assert!(matches!(
            manager.set_rule_credential(&id, &wildcard, "OTHER", bearer(), Source::Local, value("x")),
            Err(Error::InvalidSecret(..))
        ));
        assert_eq!(vault.values.lock().unwrap().len(), 1);
        // A rule with a credential cannot become a wildcard; it can become a Block rule.
        let moved =
            RuleDraft { id: Some(rule.clone()), target: Target::Host { host: "*.a.io".into() }, action: Action::Allow };
        assert!(matches!(manager.put_rule(&id, moved), Err(Error::InvalidSecret(..))));
        manager
            .put_rule(
                &id,
                RuleDraft { id: Some(rule), target: Target::Host { host: "a.io".into() }, action: Action::Block },
            )
            .unwrap();
        assert_eq!(manager.list().cages[0].cage.network.rules[0].credentials.len(), 1);
    }

    #[test]
    fn failed_save_restores_the_vault() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "OLD", bearer(), Source::Local, value("old")).unwrap();
        break_store(&manager);

        assert!(manager.set_rule_credential(&id, &rule, "OLD", bearer(), Source::Local, value("new")).is_err());
        assert_eq!(stored(&vault, &id, "OLD").as_deref(), Some("old"));

        let error =
            manager.set_rule_credential(&id, &rule, "NEW", bearer(), Source::Local, value("fresh")).unwrap_err();
        assert!(!error.to_string().contains("fresh"));
        assert_eq!(stored(&vault, &id, "NEW"), None);
        let names: Vec<_> = manager.list().cages[0].cage.network.credentials().iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, ["OLD"]);
    }

    #[test]
    fn a_removed_credential_saves_the_config_first_and_keeps_a_shared_value() {
        let (manager, _, id, vault) = manager_with_vault();
        let a = allow(&manager, &id, "a.io");
        let b = allow(&manager, &id, "b.io");
        manager.set_rule_credential(&id, &a, "KEY", bearer(), Source::Local, value("v")).unwrap();
        // The same name on a second rule shares the value.
        manager.set_rule_credential(&id, &b, "KEY", bearer(), Source::Local, None).unwrap();
        assert!(matches!(
            manager.set_rule_credential(&id, &a, "OTHER", None, Source::Local, None),
            Err(Error::SecretNotFound(_))
        ));

        manager.set_rule_credential(&id, &a, "KEY", None, Source::Local, None).unwrap();
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));

        break_store(&manager);
        assert!(manager.remove_rule(&id, &b).is_err());
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));
        assert_eq!(manager.list().cages[0].cage.network.credentials().len(), 1);

        let path = manager.store.cages_path();
        std::fs::remove_dir_all(&path).unwrap();
        manager.remove_rule(&id, &b).unwrap();
        assert_eq!(stored(&vault, &id, "KEY"), None);
        assert!(manager.list().cages[0].cage.network.credentials().is_empty());
    }

    #[tokio::test]
    async fn value_changes_while_running_reach_the_proxy_at_once() {
        let (manager, _, id) = manager();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v1")).unwrap();
        manager.start(&id).await.unwrap();

        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v2")).unwrap();
        assert!(!manager.list().cages[0].shared().pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values[&local("KEY")], SecretValue::new("v2"));

        // The guest still has the placeholder variable of a removed name.
        manager.set_rule_credential(&id, &rule, "KEY", None, Source::Local, None).unwrap();
        assert!(!live(&manager, &id).snapshot().values.contains_key(&local("KEY")));
        assert!(manager.list().cages[0].shared().pending_restart);
    }

    #[tokio::test]
    async fn a_value_change_during_start_reaches_the_proxy() {
        let (manager, runtime, id) = manager();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v1")).unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.start_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let starting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.start(&id).await }
        });
        gate.entered.notified().await;
        assert!(matches!(manager.list().cages[0].shared().status, Status::Starting { .. }));
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v2")).unwrap();
        gate.release.notify_one();

        starting.await.unwrap().unwrap();
        let view = &manager.list().cages[0];
        assert_eq!(view.shared().status, Status::Running);
        assert!(!view.shared().pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values[&local("KEY")], SecretValue::new("v2"));
    }

    /// A start during a value edit waits for the edit, so its proxy gets the new value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_during_a_credential_edit_uses_the_new_value() {
        let dir = temp_dir();
        let vault = Arc::new(GatedVault::default());
        let runtime = FakeRuntime::default();
        let manager = Arc::new(
            Manager::new(runtime.clone(), Store::new(dir.clone()), vault.clone(), temp_dir(), host()).unwrap(),
        );
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v1")).unwrap();

        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *vault.gate.lock().unwrap() = Some((entered_tx, release_rx));
        let putting = std::thread::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            move || manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v2"))
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

        assert_eq!(live(&manager, &id).snapshot().values[&local("KEY")], SecretValue::new("v2"));
        let view = &manager.list().cages[0];
        assert_eq!(view.shared().status, Status::Running);
        assert!(!view.shared().pending_restart);
    }

    #[tokio::test]
    async fn a_failed_restore_blocks_the_account_until_the_vault_holds_the_prior_value() {
        let (manager, _, id, vault) = manager_with_vault();
        credential(&manager, &id, "old.io", "KEY", Some("v1")).unwrap();
        let new = allow(&manager, &id, "new.io");

        // The set of v2 works, the save fails, and the restore and its fallback delete fail.
        break_store(&manager);
        *vault.writes_left.lock().unwrap() = Some(1);
        assert!(manager.set_rule_credential(&id, &new, "KEY", bearer(), Source::Local, value("v2")).is_err());
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v2"));
        std::fs::remove_dir_all(manager.store.cages_path()).unwrap();

        assert!(matches!(manager.start(&id).await, Err(Error::Vault(_))));
        assert_eq!(manager.list().cages[0].shared().status, Status::Stopped);
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), Source::Local, value("v3")),
            Err(Error::Vault(_))
        ));

        // The vault works again: the next start restores v1 for the old hosts.
        *vault.writes_left.lock().unwrap() = None;
        manager.start(&id).await.unwrap();
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v1"));
        assert_eq!(holders(&manager), ["old.io"]);
    }

    #[test]
    fn a_set_that_fails_after_its_write_restores_the_prior_value() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v1")).unwrap();

        vault.fail_after_write.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v2")),
            Err(Error::Vault(_))
        ));
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v1"));
    }

    #[test]
    fn stored_credentials_name_only_the_values_in_the_vault() {
        let (manager, _, id, vault) = manager_with_vault();
        credential(&manager, &id, "a.io", "A", Some("1")).unwrap();
        credential(&manager, &id, "b.io", "B", Some("2")).unwrap();
        let rule = allow(&manager, &id, "c.io");
        manager.set_rule_credential(&id, &rule, "A", bearer(), Source::Local, None).unwrap();
        // A value from before the file vault: the config has the name, the vault has no value.
        vault.delete(&Owner::cage(&id), "B").unwrap();
        assert_eq!(manager.stored_credentials(&id).unwrap(), ["A"]);
        assert!(matches!(manager.stored_credentials("nope"), Err(Error::CageNotFound)));

        // An old name needs a value again.
        let b = manager.list().cages[0].cage.network.rules[1].id.clone();
        assert!(matches!(
            manager.set_rule_credential(&id, &b, "B", bearer(), Source::Local, None),
            Err(Error::SecretValueRequired(n)) if n == "B"
        ));
        manager.set_rule_credential(&id, &b, "B", bearer(), Source::Local, value("new")).unwrap();
        assert_eq!(manager.stored_credentials(&id).unwrap(), ["A", "B"]);
    }

    #[test]
    fn claude_token_needs_explicit_allow_under_default_block() {
        let (manager, _, id, vault) = host_manager(FakeLogins::default(), NOW);
        assert!(matches!(
            manager.set_claude_token(&id, Some(SecretValue::new("token")), Source::Local, false, false),
            Err(Error::InvalidRule(message)) if message == "allow api.anthropic.com first"
        ));
        assert!(vault.get(&Owner::cage(&id), "CLAUDE_CODE_OAUTH_TOKEN").unwrap().is_none());
        manager.set_claude_token(&id, Some(SecretValue::new("token")), Source::Local, true, false).unwrap();
        let view = manager.list().cages.remove(0);
        assert!(view.claude_token.unwrap().stored);
        assert!(view.cage.host_agent(AgentKind::Claude).unwrap().login);
        let rule = &view.cage.network.rules[0];
        assert_eq!(rule.target, Target::Host { host: "api.anthropic.com".into() });
        assert_eq!(rule.action, Action::Allow);
        assert_eq!(
            rule.credentials[0].inject,
            Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() }
        );
    }

    #[test]
    fn claude_token_creates_an_exact_allow_rule_under_default_allow() {
        let (manager, _, id, _) = host_manager(FakeLogins::default(), NOW);
        manager.set_network_default(&id, DefaultAction::Allow).unwrap();
        manager.set_claude_token(&id, Some(SecretValue::new("token")), Source::Local, false, false).unwrap();
        assert_eq!(manager.list().cages[0].cage.network.rules[0].label(), "api.anthropic.com");
    }

    #[test]
    fn claude_token_does_not_silently_replace_an_exact_block() {
        let (manager, _, id, _) = host_manager(FakeLogins::default(), NOW);
        manager.set_network_default(&id, DefaultAction::Allow).unwrap();
        manager.put_rule(&id, host_rule("api.anthropic.com", Action::Block)).unwrap();
        assert!(matches!(
            manager.set_claude_token(&id, Some(SecretValue::new("token")), Source::Local, false, false),
            Err(Error::InvalidRule(..))
        ));
        manager.set_claude_token(&id, Some(SecretValue::new("token")), Source::Local, true, false).unwrap();
        assert_eq!(manager.list().cages[0].cage.network.rules[0].action, Action::Allow);
    }

    #[tokio::test]
    async fn claude_login_off_or_agent_removed_drops_the_token() {
        let off = HostAgentUse { kind: AgentKind::Claude, profile: false, login: false };
        let (manager, _, id, vault) = host_manager(FakeLogins::default(), NOW);
        manager.set_claude_token(&id, Some(SecretValue::new("token")), Source::Local, true, false).unwrap();
        manager.put_host_agent(&id, off, false).await.unwrap();
        assert!(manager.list().cages[0].cage.network.credentials().is_empty());
        assert!(vault.entry(&Owner::cage(&id), "CLAUDE_CODE_OAUTH_TOKEN").unwrap().is_none());

        manager.set_claude_token(&id, Some(SecretValue::new("token")), Source::Local, true, false).unwrap();
        manager.remove_host_agent(&id, AgentKind::Claude).unwrap();
        assert!(manager.list().cages[0].cage.network.credentials().is_empty());
        assert!(vault.entry(&Owner::cage(&id), "CLAUDE_CODE_OAUTH_TOKEN").unwrap().is_none());
    }

    #[test]
    fn claude_token_binding_is_checked_with_the_login_off() {
        let (manager, _, id, _) = host_manager(FakeLogins::default(), NOW);
        let rule = allow(&manager, &id, "other.example.com");
        assert!(matches!(
            manager.set_rule_credential(&id, &rule, "CLAUDE_CODE_OAUTH_TOKEN", bearer(), Source::Local, value("t")),
            Err(Error::InvalidSecret(..))
        ));
    }

    #[test]
    fn claude_token_rollback_preserves_the_prior_date() {
        let (manager, _, id, vault) = host_manager(FakeLogins::default(), NOW);
        manager.set_claude_token(&id, Some(SecretValue::new("old")), Source::Local, true, false).unwrap();
        let prior = vault.entry(&Owner::cage(&id), "CLAUDE_CODE_OAUTH_TOKEN").unwrap();
        break_store(&manager);
        assert!(manager.set_claude_token(&id, Some(SecretValue::new("new")), Source::Local, false, false).is_err());
        assert_eq!(vault.entry(&Owner::cage(&id), "CLAUDE_CODE_OAUTH_TOKEN").unwrap(), prior);
    }

    const SHARED: &str = "SHARED_KEY";

    fn shared() -> CredentialRef {
        CredentialRef::library(SHARED)
    }

    fn attach(manager: &Manager<FakeRuntime>, id: &str, host: &str) -> String {
        let rule = allow(manager, id, host);
        manager.set_rule_credential(id, &rule, SHARED, bearer(), Source::Library, None).unwrap();
        rule
    }

    fn value_of(manager: &Manager<FakeRuntime>, key: &str, credential: &CredentialRef) -> Option<String> {
        let live = lock(&manager.state).live.get(key).cloned().expect("the VM has a proxy");
        live.snapshot().values.get(credential).map(|v| v.expose().to_owned())
    }

    /// Creating a library entry attaches it nowhere. Two cages attach it,
    /// one cage also has a local value of the same name for another host,
    /// one rotation reaches each attached VM, and the delete is refused
    /// while a rule attaches it. Detach, rule removal, cage delete and a
    /// reload keep the library value and the other cage.
    #[tokio::test]
    async fn one_library_value_serves_two_cages_and_outlives_them() {
        let (manager, runtime, a, vault) = manager_with_vault();
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        let c = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        let rules_before = serde_json::to_string(&manager.list().cages).unwrap();
        manager.set_library_credential(SHARED, SecretValue::new("shared-fixture"), false).unwrap();
        assert_eq!(serde_json::to_string(&manager.list().cages).unwrap(), rules_before);
        let listed = manager.library_credentials().unwrap();
        assert_eq!((listed.len(), listed[0].name.as_str()), (1, SHARED));
        assert!(listed[0].saved_at.is_some());

        let a1 = attach(&manager, &a, "a1.io");
        credential(&manager, &a, "a2.io", SHARED, Some("local-fixture")).unwrap();
        attach(&manager, &b, "b1.io");
        allow(&manager, &c, "b1.io");
        assert_eq!(listed[0].saved_at, manager.library_credentials().unwrap()[0].saved_at);

        // A missing value is refused at attach, a value is refused for the library.
        let other = allow(&manager, &b, "b2.io");
        assert!(matches!(
            manager.set_rule_credential(&b, &other, "NONE", bearer(), Source::Library, None),
            Err(Error::LibraryCredentialNotFound(n)) if n == "NONE"
        ));
        assert!(matches!(
            manager.set_rule_credential(&b, &other, SHARED, bearer(), Source::Library, value("x")),
            Err(Error::InvalidSecret(..))
        ));

        // The shared VM and an own VM of A, and B.
        let own = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        manager.set_session_cage(&own, Some(&a)).unwrap();
        manager.set_session_vm(&own, true, None).await.unwrap();
        let own_key = format!("{a}-2");
        for id in [&a, &own, &b, &c] {
            manager.start(id).await.unwrap();
        }
        let env = runtime.net.lock().unwrap().clone().unwrap();
        assert!(!format!("{env:?}").contains("fixture"));
        for key in [&a, &own_key] {
            assert_eq!(value_of(&manager, key, &shared()).as_deref(), Some("shared-fixture"));
            assert_eq!(value_of(&manager, key, &local(SHARED)).as_deref(), Some("local-fixture"));
        }
        assert_eq!(placeholders(&manager.list().cages[0].cage.network), [SHARED]);
        assert_eq!(value_of(&manager, &b, &shared()).as_deref(), Some("shared-fixture"));
        assert_eq!(value_of(&manager, &c, &shared()), None);

        manager.set_library_credential(SHARED, SecretValue::new("rotated-fixture"), true).unwrap();
        for key in [&a, &own_key, &b] {
            assert_eq!(value_of(&manager, key, &shared()).as_deref(), Some("rotated-fixture"));
        }
        assert_eq!(value_of(&manager, &a, &local(SHARED)).as_deref(), Some("local-fixture"));
        assert_eq!(value_of(&manager, &c, &shared()), None);

        // The delete is refused and names the cages; the value and the rules stay.
        let names: Vec<String> = manager.list().cages.iter().map(|v| v.cage.name.clone()).collect();
        let Err(Error::LibraryCredentialInUse { cages, .. }) = manager.delete_library_credential(SHARED) else {
            panic!("the delete was not refused");
        };
        assert_eq!(cages, [names[0].clone(), names[1].clone()]);
        assert_eq!(in_library(&vault, SHARED).as_deref(), Some("rotated-fixture"));

        // Detach in A: B and the library value stay.
        manager.set_rule_credential(&a, &a1, SHARED, None, Source::Library, None).unwrap();
        assert_eq!(value_of(&manager, &a, &shared()), None);
        assert_eq!(value_of(&manager, &b, &shared()).as_deref(), Some("rotated-fixture"));
        assert_eq!(stored(&vault, &a, SHARED).as_deref(), Some("local-fixture"));
        // Removing the local rule removes the local value only.
        let a2 = manager.list().cages[0].cage.network.rules[1].id.clone();
        manager.remove_rule(&a, &a2).unwrap();
        assert_eq!(stored(&vault, &a, SHARED), None);
        assert_eq!(in_library(&vault, SHARED).as_deref(), Some("rotated-fixture"));

        // Delete A after its sessions.
        attach(&manager, &a, "a1.io");
        for id in [&a, &own] {
            manager.stop(id, None).await.unwrap();
            manager.delete(id).await.unwrap();
        }
        manager.delete_cage(&a).unwrap();
        assert_eq!(in_library(&vault, SHARED).as_deref(), Some("rotated-fixture"));
        assert_eq!(value_of(&manager, &b, &shared()).as_deref(), Some("rotated-fixture"));

        // A reload keeps the value for B, and keeps it with no cage.
        let reload = || {
            Manager::new(
                FakeRuntime::default(),
                Store::new(manager.store.dir().to_owned()),
                vault.clone(),
                temp_dir(),
                host(),
            )
            .unwrap()
        };
        let reloaded = reload();
        let cage_b = lock(&reloaded.state).cage(&b).unwrap().clone();
        assert_eq!(reloaded.vault_values(&cage_b).unwrap()[&shared()], SecretValue::new("rotated-fixture"));
        let b1 = cage_b.network.rules[0].id.clone();
        reloaded.set_rule_credential(&b, &b1, SHARED, None, Source::Library, None).unwrap();
        drop(reloaded);
        let reloaded = reload();
        assert_eq!(reloaded.library_credentials().unwrap().len(), 1);
        reloaded.delete_library_credential(SHARED).unwrap();
        assert!(reloaded.library_credentials().unwrap().is_empty());
    }

    /// A missing library value fails the start and never takes the local value.
    #[tokio::test]
    async fn a_missing_library_value_fails_the_start_and_is_not_replaced() {
        let (manager, _, id, vault) = manager_with_vault();
        manager.set_library_credential(SHARED, SecretValue::new("shared-fixture"), false).unwrap();
        credential(&manager, &id, "a2.io", SHARED, Some("local-fixture")).unwrap();
        attach(&manager, &id, "a1.io");
        vault.delete(&Owner::Library, SHARED).unwrap();
        assert!(matches!(manager.start(&id).await, Err(Error::SecretMissing(n)) if n == SHARED));
        assert!(lock(&manager.state).live.is_empty());
    }

    /// Switching a rule from the local value to the library drops the local
    /// value, and back needs a new local value.
    #[test]
    fn a_source_switch_drops_the_unused_local_value() {
        let (manager, _, id, vault) = manager_with_vault();
        manager.set_library_credential(SHARED, SecretValue::new("shared-fixture"), false).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, SHARED, bearer(), Source::Local, value("local-fixture")).unwrap();
        manager.set_rule_credential(&id, &rule, SHARED, bearer(), Source::Library, None).unwrap();
        assert_eq!(stored(&vault, &id, SHARED), None);
        assert_eq!(manager.stored_credentials(&id).unwrap(), Vec::<String>::new());
        assert!(matches!(
            manager.set_rule_credential(&id, &rule, SHARED, bearer(), Source::Local, None),
            Err(Error::SecretValueRequired(_))
        ));
        manager.set_rule_credential(&id, &rule, SHARED, bearer(), Source::Local, value("new-fixture")).unwrap();
        assert_eq!(in_library(&vault, SHARED).as_deref(), Some("shared-fixture"));
        assert_eq!(manager.stored_credentials(&id).unwrap(), [SHARED]);
    }

    /// One recipient cannot take one name from both sources.
    #[test]
    fn an_ambiguous_source_for_one_recipient_is_refused() {
        let (manager, _, id, _) = manager_with_vault();
        manager.set_library_credential(SHARED, SecretValue::new("shared-fixture"), false).unwrap();
        credential(&manager, &id, "api.openai.com", SHARED, Some("local-fixture")).unwrap();
        let service = manager
            .put_rule(
                &id,
                RuleDraft { id: None, target: Target::Service { id: "openai".into() }, action: Action::Allow },
            )
            .unwrap();
        assert!(matches!(
            manager.set_rule_credential(&id, &service, SHARED, bearer(), Source::Library, None),
            Err(Error::InvalidSecret(..))
        ));
        assert_eq!(manager.list().cages[0].cage.network.credentials().len(), 1);
    }

    /// A rotation that fails after its write restores the prior value. If
    /// the restore fails too, the library account stays unsettled: starts
    /// fail until a retry sees the vault work again.
    /// The backend checks the intent under the edit lock, so a stale
    /// listing cannot replace a value on add or create one on rotate.
    #[test]
    fn add_refuses_an_existing_name_and_rotate_refuses_a_missing_name() {
        let (manager, _, _, vault) = manager_with_vault();
        assert!(matches!(
            manager.set_library_credential(SHARED, SecretValue::new("v1"), true),
            Err(Error::LibraryCredentialNotFound(n)) if n == SHARED
        ));
        assert_eq!(in_library(&vault, SHARED), None);
        manager.set_library_credential(SHARED, SecretValue::new("v1"), false).unwrap();
        assert!(matches!(
            manager.set_library_credential(SHARED, SecretValue::new("v2"), false),
            Err(Error::LibraryCredentialExists(n)) if n == SHARED
        ));
        assert_eq!(in_library(&vault, SHARED).as_deref(), Some("v1"));
    }

    #[tokio::test]
    async fn a_failed_rotation_rolls_back_and_a_retry_settles() {
        let (manager, _, id, vault) = manager_with_vault();
        manager.set_library_credential(SHARED, SecretValue::new("v1"), false).unwrap();
        attach(&manager, &id, "a1.io");
        manager.start(&id).await.unwrap();

        vault.fail_after_write.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(manager.set_library_credential(SHARED, SecretValue::new("v2"), true).is_err());
        assert_eq!(in_library(&vault, SHARED).as_deref(), Some("v1"));
        assert_eq!(value_of(&manager, &id, &shared()).as_deref(), Some("v1"));

        *vault.writes_left.lock().unwrap() = Some(1);
        vault.fail_after_write.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(manager.set_library_credential(SHARED, SecretValue::new("v3"), true).is_err());
        assert!(lock(&manager.unsettled).contains_key(&(Owner::Library, SHARED.to_owned())));
        assert_eq!(value_of(&manager, &id, &shared()).as_deref(), Some("v1"));
        manager.stop(&id, None).await.unwrap();
        assert!(matches!(manager.start(&id).await, Err(Error::Vault(_))));

        *vault.writes_left.lock().unwrap() = None;
        manager.set_library_credential(SHARED, SecretValue::new("v4"), true).unwrap();
        assert!(lock(&manager.unsettled).is_empty());
        manager.start(&id).await.unwrap();
        assert_eq!(value_of(&manager, &id, &shared()).as_deref(), Some("v4"));
    }

    /// A cage with the old textual id `@library` has a local value with the
    /// name of a library value. List, add, attach, rotate, prune and a
    /// reload of the vault file keep the two values apart.
    #[tokio::test]
    async fn a_legacy_at_library_cage_and_a_library_value_stay_apart() {
        const LEGACY: &str = "@library";
        let (dir, data) = (temp_dir(), temp_dir());
        let load = |vault: Arc<dyn Vault>| {
            Manager::new(FakeRuntime::default(), Store::new(dir.clone()), vault, temp_dir(), host()).unwrap()
        };
        let vault: Arc<dyn Vault> = Arc::new(secrets::FileVault::new(&data).unwrap());
        let manager = load(vault.clone());
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        let local = allow(&manager, &id, "a1.io");
        manager.set_rule_credential(&id, &local, SHARED, bearer(), Source::Local, value("cage-fixture")).unwrap();
        drop(manager);
        // The cage gets the old id, in the config and in the vault.
        for file in [crate::store::CAGES, crate::store::SESSIONS] {
            let text = std::fs::read_to_string(dir.join(file)).unwrap();
            std::fs::write(dir.join(file), text.replace(&id, LEGACY)).unwrap();
        }
        vault.set(&Owner::cage(LEGACY), SHARED, &SecretValue::new("cage-fixture")).unwrap();
        vault.delete(&Owner::cage(&id), SHARED).unwrap();

        // The load prunes, and the cage value stays a cage value.
        let manager = load(vault.clone());
        assert!(manager.library_credentials().unwrap().is_empty());
        assert_eq!(manager.stored_credentials(LEGACY).unwrap(), [SHARED]);
        manager.set_library_credential(SHARED, SecretValue::new("library-fixture"), false).unwrap();
        let names: Vec<_> = manager.library_credentials().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, [SHARED]);
        attach(&manager, LEGACY, "a2.io");
        manager.start(LEGACY).await.unwrap();
        let own = CredentialRef { source: Source::Local, name: SHARED.into() };
        assert_eq!(value_of(&manager, LEGACY, &own).as_deref(), Some("cage-fixture"));
        assert_eq!(value_of(&manager, LEGACY, &shared()).as_deref(), Some("library-fixture"));

        manager.set_library_credential(SHARED, SecretValue::new("rotated-fixture"), true).unwrap();
        assert_eq!(value_of(&manager, LEGACY, &own).as_deref(), Some("cage-fixture"));
        assert_eq!(value_of(&manager, LEGACY, &shared()).as_deref(), Some("rotated-fixture"));
        manager.stop(LEGACY, None).await.unwrap();
        drop(manager);

        // A new vault reads the file again, and a new load prunes again.
        let vault: Arc<dyn Vault> = Arc::new(secrets::FileVault::new(&data).unwrap());
        let manager = load(vault.clone());
        let get = |owner: &Owner| vault.get(owner, SHARED).unwrap().map(|v| v.expose().to_owned());
        assert_eq!(get(&Owner::cage(LEGACY)).as_deref(), Some("cage-fixture"));
        assert_eq!(get(&Owner::Library).as_deref(), Some("rotated-fixture"));
        assert_eq!(manager.stored_credentials(LEGACY).unwrap(), [SHARED]);
        assert_eq!(manager.library_credentials().unwrap().len(), 1);
    }

    /// A start during a rotation waits for it, so its proxy gets the new value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_during_a_rotation_uses_the_new_value() {
        let dir = temp_dir();
        let vault = Arc::new(GatedVault::default());
        let runtime = FakeRuntime::default();
        let manager = Arc::new(
            Manager::new(runtime.clone(), Store::new(dir.clone()), vault.clone(), temp_dir(), host()).unwrap(),
        );
        let id = manager.create(dir.to_str().unwrap(), None).unwrap();
        manager.set_library_credential(SHARED, SecretValue::new("v1"), false).unwrap();
        attach(&manager, &id, "a1.io");

        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *vault.gate.lock().unwrap() = Some((entered_tx, release_rx));
        let rotating = std::thread::spawn({
            let manager = manager.clone();
            move || manager.set_library_credential(SHARED, SecretValue::new("v2"), true)
        });
        entered.recv().unwrap();
        let starting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.start(&id).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(runtime.log.lock().unwrap().is_empty());
        release.send(()).unwrap();
        rotating.join().unwrap().unwrap();
        starting.await.unwrap().unwrap();
        assert_eq!(live(&manager, &id).snapshot().values[&shared()], SecretValue::new("v2"));
    }

    /// The setup token from the library: no local account, the exact Allow
    /// binding, and the login off in one cage keeps the other cage.
    #[tokio::test]
    async fn a_library_claude_token_serves_two_cages() {
        let (manager, runtime, a, vault) = host_manager(FakeLogins::default(), NOW);
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        let token = || CredentialRef::library(cage::CLAUDE_TOKEN);
        assert!(matches!(
            manager.set_claude_token(&a, None, Source::Library, true, false),
            Err(Error::LibraryCredentialNotFound(_))
        ));
        assert!(matches!(
            manager.set_claude_token(&a, Some(SecretValue::new("x")), Source::Library, true, false),
            Err(Error::InvalidSecret(..))
        ));
        manager.set_library_credential(cage::CLAUDE_TOKEN, SecretValue::new("token-fixture"), false).unwrap();
        let saved_at = vault.entry(&Owner::Library, cage::CLAUDE_TOKEN).unwrap().unwrap().saved_at;
        assert!(matches!(
            manager.set_claude_token(&a, None, Source::Library, false, false),
            Err(Error::InvalidRule(m)) if m == "allow api.anthropic.com first"
        ));
        // A local token first: the switch to the library removes it.
        manager.set_claude_token(&a, Some(SecretValue::new("local-fixture")), Source::Local, true, false).unwrap();
        for id in [&a, &b] {
            manager.set_claude_token(id, None, Source::Library, true, false).unwrap();
        }
        assert_eq!(vault.get(&Owner::cage(&a), cage::CLAUDE_TOKEN).unwrap(), None);
        assert_eq!(vault.entry(&Owner::Library, cage::CLAUDE_TOKEN).unwrap().unwrap().saved_at, saved_at);
        let view = manager.list().cages.remove(0);
        let claude = view.claude_token.unwrap();
        assert!(claude.stored && claude.library);
        let rule = &view.cage.network.rules[0];
        assert_eq!(
            (rule.target.clone(), rule.action),
            (Target::Host { host: "api.anthropic.com".into() }, Action::Allow)
        );
        assert_eq!(rule.credentials[0].source, Source::Library);

        manager.start(&a).await.unwrap();
        let net = runtime.net.lock().unwrap().clone().unwrap();
        assert!(net.env.contains(&(cage::CLAUDE_TOKEN.to_owned(), format!("$MSB_{}$", cage::CLAUDE_TOKEN))));
        assert!(!format!("{net:?}").contains("token-fixture"));
        assert_eq!(value_of(&manager, &a, &token()).as_deref(), Some("token-fixture"));

        // The login off in A detaches A only.
        manager
            .put_host_agent(&a, HostAgentUse { kind: AgentKind::Claude, profile: false, login: false }, false)
            .await
            .unwrap();
        assert!(manager.list().cages[0].cage.network.credentials().is_empty());
        assert_eq!(value_of(&manager, &a, &token()), None);
        assert!(manager.list().cages[1].cage.network.uses(&token()));
        assert_eq!(in_library(&vault, cage::CLAUDE_TOKEN).as_deref(), Some("token-fixture"));
        manager.remove_host_agent(&b, AgentKind::Claude).unwrap();
        assert_eq!(in_library(&vault, cage::CLAUDE_TOKEN).as_deref(), Some("token-fixture"));
    }
}
