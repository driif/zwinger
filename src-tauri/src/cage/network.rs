use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cage::manager::{Manager, Vm, lock};
use crate::cage::{self, WORKSPACE};
use crate::error::{Error, Result};
use crate::network::{Credential, DefaultAction, Inject, NetworkPolicy, RuleDraft};
use crate::proxy::log::Entry;
use crate::runtime::{PipeEvent, PipeInput, SandboxRuntime};
use crate::secrets::{self, SecretValue};

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
    pub fn list_net_events(&self, id: &str) -> Result<Vec<Entry>> {
        let mut state = lock(&self.state);
        state.project_mut(id)?;
        Ok(state.net.get(id).map(|ring| ring.iter().cloned().collect()).unwrap_or_default())
    }

    /// Applies `edit` to the network policy and saves it. A running proxy
    /// uses the new policy from its next request.
    pub(crate) fn edit_network<T>(&self, id: &str, edit: impl FnOnce(&mut NetworkPolicy) -> Result<T>) -> Result<T> {
        let mut out = None;
        self.edit(id, |s| {
            out = Some(edit(&mut s.network)?);
            Ok(())
        })?;
        self.publish(id, None);
        Ok(out.expect("the edit ran"))
    }

    /// Sends the stored policy to the cage's proxy, if it has one, in one
    /// step with the new `value` of a credential.
    pub(crate) fn publish(&self, id: &str, value: Option<(&str, SecretValue)>) {
        let state = lock(&self.state);
        if let Some(live) = state.live.get(id)
            && let Some(project) = state.projects.iter().find(|s| s.id == id)
        {
            live.edit(|policy, values| {
                *policy = project.network.clone();
                if let Some((name, value)) = value {
                    values.insert(name.to_owned(), value);
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
        self.drop_unused(id, removed.credentials.iter().map(|c| c.name.as_str()))
    }

    /// Deletes the values of `names` that no credential of the project uses.
    pub(crate) fn drop_unused<'a>(&self, id: &str, names: impl Iterator<Item = &'a str>) -> Result<()> {
        let (used, live) = {
            let mut state = lock(&self.state);
            let used: Vec<String> =
                state.project_mut(id)?.network.credentials().into_iter().map(|c| c.name.clone()).collect();
            (used, state.live.get(id).cloned())
        };
        let mut result = Ok(());
        for name in names.filter(|n| !used.iter().any(|u| u == n)) {
            if let Some(live) = &live {
                live.set_value(name, None);
            }
            match self.vault.delete(id, name) {
                Ok(()) => {
                    lock(&self.unsettled).remove(&secrets::account(id, name));
                }
                Err(e) => result = Err(e),
            }
        }
        result
    }

    /// Adds, changes or (with `inject` `None`) removes the credential `name`
    /// of the rule. A new name needs a value; `None` keeps the current value.
    /// The vault is written before the config, and a failed config save
    /// restores the vault. A running proxy uses the value from its next request.
    pub fn set_rule_credential(
        &self,
        id: &str,
        rule_id: &str,
        name: &str,
        inject: Option<Inject>,
        value: Option<SecretValue>,
    ) -> Result<()> {
        let Some(inject) = inject else {
            let _edit = lock(&self.secret_edit);
            if self.vault.is_pending(id, name) {
                return Err(Error::SecretPending(name.to_owned()));
            }
            self.edit_network(id, |n| {
                let rule = n.rule_mut(rule_id)?;
                let before = rule.credentials.len();
                rule.credentials.retain(|c| c.name != name);
                if rule.credentials.len() == before {
                    return Err(Error::SecretNotFound(name.to_owned()));
                }
                Ok(())
            })?;
            return self.drop_unused(id, std::iter::once(name));
        };
        let credential = Credential { name: name.to_owned(), inject }.normalize()?;
        let name = credential.name.clone();
        if value.as_ref().is_some_and(|v| v.expose().is_empty()) {
            return Err(Error::SecretValueRequired(name));
        }
        let put = |n: &mut NetworkPolicy| -> Result<()> {
            let rule = n.rule_mut(rule_id)?;
            match rule.credentials.iter_mut().find(|c| c.name == credential.name) {
                Some(current) => *current = credential.clone(),
                None => rule.credentials.push(credential.clone()),
            }
            Ok(())
        };
        let _edit = lock(&self.secret_edit);
        self.settle(id, &name)?;
        let is_new = {
            let state = lock(&self.state);
            let mut project = state.projects.iter().find(|s| s.id == id).ok_or(Error::ProjectNotFound)?.clone();
            let is_new = !project.network.credentials().iter().any(|c| c.name == name);
            put(&mut project.network)?;
            project.check()?;
            is_new
        };
        let Some(value) = value else {
            if is_new {
                return Err(Error::SecretValueRequired(name));
            }
            return self.edit_network(id, put);
        };

        let prior = self.vault.get(id, &name)?;
        if let Err(e) = self.vault.set(id, &name, &value) {
            // The late write still runs; the restore runs after it.
            if matches!(e, Error::KeychainTimeout) {
                self.restore_value(id, &name, prior.as_ref());
            }
            return Err(e);
        }
        if let Err(e) = self.edit(id, |s| put(&mut s.network)) {
            self.restore_value(id, &name, prior.as_ref());
            return Err(e);
        }
        // A request must not see the new rule with the old value.
        self.publish(id, Some((&name, value)));
        Ok(())
    }

    /// Tests the network from inside the running cage: first a TCP connect
    /// to the proxy, then a request to `host` through it.
    pub async fn test_network(&self, id: &str, host: &str) -> Result<NetTest> {
        let host = cage::domain(host)?;
        if host.starts_with("*.") {
            return Err(Error::InvalidDomain(host, "test one host, not a wildcard".into()));
        }
        let vm = match lock(&self.state).vms.get(id) {
            Some(Vm::Running { vm, .. }) => vm.clone(),
            _ => return Err(Error::NotRunning),
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
    /// is removed, and the secret is then missing. If neither ends, the
    /// account stays unsettled until [`Self::settle`] sees the prior value.
    pub(crate) fn restore_value(&self, id: &str, name: &str, prior: Option<&SecretValue>) {
        let restored = match prior {
            Some(prior) => self.vault.set(id, name, prior),
            None => self.vault.delete(id, name),
        };
        let restored = match restored {
            // A restore past its deadline still runs, after the write.
            Err(e) if !matches!(e, Error::KeychainTimeout) => self.vault.delete(id, name),
            other => other,
        };
        if restored.is_err() {
            lock(&self.unsettled).insert(secrets::account(id, name), prior.cloned());
        }
    }

    /// Fails while a write of the account still runs, or while the vault may
    /// hold a value that the saved config does not expect. An unsettled
    /// account is checked, and restored again, here.
    pub(crate) fn settle(&self, id: &str, name: &str) -> Result<()> {
        if self.vault.is_pending(id, name) {
            return Err(Error::SecretPending(name.to_owned()));
        }
        let account = secrets::account(id, name);
        let Some(prior) = lock(&self.unsettled).get(&account).cloned() else {
            return Ok(());
        };
        let held = self.vault.get(id, name).map_err(|_| Error::SecretPending(name.to_owned()))?;
        let matches = held.as_ref().map(SecretValue::expose) == prior.as_ref().map(SecretValue::expose);
        let restored = matches
            || match &prior {
                Some(prior) => self.vault.set(id, name, prior),
                None => self.vault.delete(id, name),
            }
            .or_else(|_| self.vault.delete(id, name))
            .is_ok();
        if !restored {
            return Err(Error::SecretPending(name.to_owned()));
        }
        lock(&self.unsettled).remove(&account);
        Ok(())
    }
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

/// The names of the credentials, which the guest gets as placeholder variables.
pub(crate) fn placeholders(policy: &NetworkPolicy) -> Vec<String> {
    let mut names: Vec<String> = policy.credentials().into_iter().map(|c| c.name.clone()).collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
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
        manager.stop(&id).await.unwrap();
        net_closed(&runtime, 0).await;
        assert_eq!(manager.list_net_events(&id).unwrap().len(), 2);
        *runtime.net_script.lock().unwrap() = vec![dns_denied("2999-01-01T00:00:01Z", "google.com")];
        manager.start(&id).await.unwrap();
        let (_, event) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
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

        net_sender(&runtime, 0).send(dns_denied("2999-01-01T00:00:00Z", "google.com")).await.unwrap();
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
        assert!(!manager.list()[0].pending_restart);

        // A new credential name needs its placeholder variable in the guest.
        manager.set_rule_credential(&id, &rule, "NPM_TOKEN", bearer(), Some(SecretValue::new("t"))).unwrap();
        assert!(manager.list()[0].pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values["NPM_TOKEN"], SecretValue::new("t"));
        manager.stop(&id).await.unwrap();
        assert!(lock(&manager.state).live.is_empty());
        manager.start(&id).await.unwrap();
        assert!(!manager.list()[0].pending_restart);
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
        assert!(matches!(manager.put_rule("nope", host_rule("a.io", Action::Allow)), Err(Error::ProjectNotFound)));
        manager.remove_rule(&id, &first).unwrap();
        assert!(matches!(manager.remove_rule(&id, &first), Err(Error::RuleNotFound)));
        let rules = manager.list()[0].project.network.rules.clone();
        assert_eq!(rules.iter().map(|r| r.label()).collect::<Vec<_>>(), ["*.anthropic.com"]);
    }

    #[tokio::test]
    async fn credential_values_stay_out_of_the_config() {
        let (manager, runtime, id, vault) = manager_with_vault();
        credential(&manager, &id, "api.openai.com", " OPENAI_API_KEY ", Some("sk-hidden")).unwrap();
        assert_eq!(stored(&vault, &id, "OPENAI_API_KEY").as_deref(), Some("sk-hidden"));
        let rules = manager.list()[0].project.network.rules.clone();
        assert_eq!(rules[0].credentials[0].name, "OPENAI_API_KEY");

        let config = std::fs::read_to_string(manager.store.path()).unwrap();
        let projects = serde_json::to_string(&manager.list()).unwrap();
        for text in [config, projects] {
            assert!(text.contains("OPENAI_API_KEY") && !text.contains("sk-hidden"), "{text}");
        }

        manager.start(&id).await.unwrap();
        let net = runtime.net.lock().unwrap().clone().unwrap();
        assert!(net.env.contains(&("OPENAI_API_KEY".to_owned(), "$MSB_OPENAI_API_KEY$".to_owned())));
        assert!(!format!("{net:?}").contains("sk-hidden"));
        assert_eq!(live(&manager, &id).snapshot().values["OPENAI_API_KEY"], SecretValue::new("sk-hidden"));
    }

    #[test]
    fn a_credential_needs_a_value_only_when_new() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        let set = |name: &str, inject: Option<Inject>, value: Option<SecretValue>| {
            manager.set_rule_credential(&id, &rule, name, inject, value)
        };
        assert!(matches!(set("KEY", bearer(), None), Err(Error::SecretValueRequired(_))));
        assert!(matches!(set("KEY", bearer(), value("")), Err(Error::SecretValueRequired(_))));
        assert!(manager.list()[0].project.network.rules[0].credentials.is_empty());

        set("KEY", bearer(), value("v1")).unwrap();
        let replace = Inject::Replace {
            places: secrets::Places { header_fields: vec!["x-api-key".into()], query: false, body: false },
        };
        set("KEY", Some(replace.clone()), None).unwrap();
        assert_eq!(manager.list()[0].project.network.rules[0].credentials[0].inject, replace);
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v1"));

        assert!(matches!(set("PATH", bearer(), value("x")), Err(Error::InvalidSecret(..))));
        assert!(matches!(
            manager.set_rule_credential("nope", &rule, "KEY", bearer(), value("x")),
            Err(Error::ProjectNotFound)
        ));
        let wildcard = manager.put_rule(&id, host_rule("*.b.io", Action::Allow)).unwrap();
        assert!(matches!(
            manager.set_rule_credential(&id, &wildcard, "OTHER", bearer(), value("x")),
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
        assert_eq!(manager.list()[0].project.network.rules[0].credentials.len(), 1);
    }

    #[test]
    fn failed_save_restores_the_vault() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "OLD", bearer(), value("old")).unwrap();
        break_store(&manager);

        assert!(manager.set_rule_credential(&id, &rule, "OLD", bearer(), value("new")).is_err());
        assert_eq!(stored(&vault, &id, "OLD").as_deref(), Some("old"));

        let error = manager.set_rule_credential(&id, &rule, "NEW", bearer(), value("fresh")).unwrap_err();
        assert!(!error.to_string().contains("fresh"));
        assert_eq!(stored(&vault, &id, "NEW"), None);
        let names: Vec<_> = manager.list()[0].project.network.credentials().iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, ["OLD"]);
    }

    #[test]
    fn a_removed_credential_saves_the_config_first_and_keeps_a_shared_value() {
        let (manager, _, id, vault) = manager_with_vault();
        let a = allow(&manager, &id, "a.io");
        let b = allow(&manager, &id, "b.io");
        manager.set_rule_credential(&id, &a, "KEY", bearer(), value("v")).unwrap();
        // The same name on a second rule shares the value.
        manager.set_rule_credential(&id, &b, "KEY", bearer(), None).unwrap();
        assert!(matches!(manager.set_rule_credential(&id, &a, "OTHER", None, None), Err(Error::SecretNotFound(_))));

        manager.set_rule_credential(&id, &a, "KEY", None, None).unwrap();
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));

        break_store(&manager);
        assert!(manager.remove_rule(&id, &b).is_err());
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));
        assert_eq!(manager.list()[0].project.network.credentials().len(), 1);

        let path = manager.store.path().to_owned();
        std::fs::remove_dir_all(&path).unwrap();
        manager.remove_rule(&id, &b).unwrap();
        assert_eq!(stored(&vault, &id, "KEY"), None);
        assert!(manager.list()[0].project.network.credentials().is_empty());
    }

    #[tokio::test]
    async fn value_changes_while_running_reach_the_proxy_at_once() {
        let (manager, _, id) = manager();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v1")).unwrap();
        manager.start(&id).await.unwrap();

        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v2")).unwrap();
        assert!(!manager.list()[0].pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values["KEY"], SecretValue::new("v2"));

        // The guest still has the placeholder variable of a removed name.
        manager.set_rule_credential(&id, &rule, "KEY", None, None).unwrap();
        assert!(!live(&manager, &id).snapshot().values.contains_key("KEY"));
        assert!(manager.list()[0].pending_restart);
    }

    #[tokio::test]
    async fn a_value_change_during_start_reaches_the_proxy() {
        let (manager, runtime, id) = manager();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v1")).unwrap();
        let gate = Arc::new(PtyGate::default());
        *runtime.start_gate.lock().unwrap() = Some(gate.clone());

        let manager = Arc::new(manager);
        let starting = tokio::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            async move { manager.start(&id).await }
        });
        gate.entered.notified().await;
        assert!(matches!(manager.list()[0].status, Status::Starting { .. }));
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v2")).unwrap();
        gate.release.notify_one();

        starting.await.unwrap().unwrap();
        let view = &manager.list()[0];
        assert_eq!(view.status, Status::Running);
        assert!(!view.pending_restart);
        assert_eq!(live(&manager, &id).snapshot().values["KEY"], SecretValue::new("v2"));
    }

    /// A start during a value edit waits for the edit, so its proxy gets the new value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_during_a_credential_edit_uses_the_new_value() {
        let dir = temp_dir();
        let vault = Arc::new(GatedVault::default());
        let runtime = FakeRuntime::default();
        let manager = Arc::new(
            Manager::new(runtime.clone(), Store::new(dir.join("projects.json")), vault.clone(), temp_dir(), host())
                .unwrap(),
        );
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v1")).unwrap();

        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *vault.gate.lock().unwrap() = Some((entered_tx, release_rx));
        let putting = std::thread::spawn({
            let (manager, id) = (manager.clone(), id.clone());
            move || manager.set_rule_credential(&id, &rule, "KEY", bearer(), value("v2"))
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

        assert_eq!(live(&manager, &id).snapshot().values["KEY"], SecretValue::new("v2"));
        let view = &manager.list()[0];
        assert_eq!(view.status, Status::Running);
        assert!(!view.pending_restart);
    }

    #[tokio::test]
    async fn a_keychain_write_past_its_deadline_blocks_the_account_until_it_ends() {
        let dir = temp_dir();
        let slow = Arc::new(secrets::fake::SlowVault::default());
        let vault = Arc::new(secrets::Ordered::alone(slow.clone(), Duration::from_millis(50)));
        let manager = Manager::new(
            FakeRuntime::default(),
            Store::new(dir.join("projects.json")),
            vault.clone(),
            temp_dir(),
            host(),
        )
        .unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        credential(&manager, &id, "old.io", "KEY", Some("v1")).unwrap();
        let new = allow(&manager, &id, "new.io");

        // A new host and a new value: the set passes its deadline.
        slow.hold();
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), value("v2")),
            Err(Error::KeychainTimeout)
        ));
        assert!(matches!(manager.start(&id).await, Err(Error::SecretPending(n)) if n == "KEY"));
        assert_eq!(manager.list()[0].status, Status::Stopped);
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), value("v3")),
            Err(Error::SecretPending(_))
        ));
        assert!(matches!(manager.set_rule_credential(&id, &new, "KEY", None, None), Err(Error::SecretPending(_))));

        // The late set runs, then the restore: the old hosts keep the old value.
        slow.release();
        secrets::tests::until_settled(vault.as_ref(), &id, "KEY");
        let account = secrets::account(&id, "KEY");
        let writes: Vec<_> =
            slow.writes.lock().unwrap().iter().map(|(a, v)| (a == &account, v.clone().unwrap())).collect();
        assert_eq!(writes, [(true, "v1".to_owned()), (true, "v2".to_owned()), (true, "v1".to_owned())]);
        assert_eq!(holders(&manager), ["old.io"]);
        manager.start(&id).await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_restore_blocks_the_account_until_the_vault_holds_the_prior_value() {
        let dir = temp_dir();
        let slow = Arc::new(secrets::fake::SlowVault::default());
        let vault = Arc::new(secrets::Ordered::alone(slow.clone(), Duration::from_millis(50)));
        let manager = Manager::new(
            FakeRuntime::default(),
            Store::new(dir.join("projects.json")),
            vault.clone(),
            temp_dir(),
            host(),
        )
        .unwrap();
        let id = manager.create(dir.to_str().unwrap()).unwrap();
        credential(&manager, &id, "old.io", "KEY", Some("v1")).unwrap();
        let new = allow(&manager, &id, "new.io");

        // The late set of v2 runs; the restore and its fallback delete fail.
        slow.hold();
        *slow.fail_after.lock().unwrap() = Some(2);
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), value("v2")),
            Err(Error::KeychainTimeout)
        ));
        slow.release();
        secrets::tests::until_settled(vault.as_ref(), &id, "KEY");
        assert_eq!(vault.get(&id, "KEY").unwrap().unwrap().expose(), "v2");

        assert!(matches!(manager.start(&id).await, Err(Error::SecretPending(n)) if n == "KEY"));
        assert!(matches!(
            manager.set_rule_credential(&id, &new, "KEY", bearer(), value("v3")),
            Err(Error::SecretPending(_))
        ));

        // The vault works again: the next start restores v1 for the old hosts.
        *slow.fail_after.lock().unwrap() = None;
        manager.start(&id).await.unwrap();
        assert_eq!(vault.get(&id, "KEY").unwrap().unwrap().expose(), "v1");
        assert_eq!(holders(&manager), ["old.io"]);
    }
}
