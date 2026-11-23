//! The live parts of a VM's network policy (the pending asks and the rules
//! until stop), the network profiles and the JSON view of the policy.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cage::manager::{Manager, lock};
use crate::error::{Error, Result};
use crate::host_agents::login;
use crate::network::{self, Action, DefaultAction, NetworkPolicy, Rule, RuleDraft, Target};
use crate::proxy::Live;
use crate::proxy::ask::{self, Ask};
use crate::runtime::SandboxRuntime;
use crate::store;

/// The profiles file in the app data dir.
pub(crate) const PROFILES: &str = "network-profiles.json";
/// The most profiles.
const PROFILE_LIMIT: usize = 100;
/// The longest profile name, in characters.
const NAME_LENGTH: usize = 64;

/// The answer of the user to a pending ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AskAnswer {
    /// Allows this request, and the later requests of its connection.
    Once,
    /// Adds an Allow rule for the host until the VM stops.
    UntilStop,
    /// Saves an Allow rule for the host in the cage.
    Always,
    Block,
}

/// A saved rule of a profile. A profile has no credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRule {
    pub target: Target,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<String>>,
}

/// A named copy of a default and a rule list. Apply copies it into a cage;
/// a later change of the profile does not change that cage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub name: String,
    pub default: DefaultAction,
    pub rules: Vec<ProfileRule>,
}

impl<R: SandboxRuntime> Manager<R> {
    /// The proxy of VM `key` and the id of its cage.
    fn live_of(&self, key: &str) -> Result<(Arc<Live>, String)> {
        let state = lock(&self.state);
        let live = state.live.get(key).cloned().ok_or(Error::NotRunning)?;
        let cage = state.vms.get(key).ok_or(Error::NotRunning)?.cage().to_owned();
        Ok((live, cage))
    }

    /// Tells the app that the asks or the rules until stop of VM `key` changed.
    fn live_changed(&self, key: &str) {
        if let Some(sink) = self.ask_sink.get() {
            sink(key);
        }
    }

    /// The pending asks of VM `key`, the oldest first. None if it does not run.
    pub fn list_asks(&self, key: &str) -> Vec<Ask> {
        self.live_of(key).map(|(live, _)| live.asks.list()).unwrap_or_default()
    }

    /// Answers a pending ask of VM `key`. Fails if the ask is no longer pending.
    pub fn answer_ask(&self, key: &str, ask_id: &str, answer: AskAnswer) -> Result<()> {
        let (live, cage) = self.live_of(key)?;
        let host = live.asks.get(ask_id)?.host;
        match answer {
            AskAnswer::Once => live.asks.answer(ask_id, ask::Answer::Allow),
            AskAnswer::Block => live.asks.answer(ask_id, ask::Answer::Block),
            AskAnswer::UntilStop => {
                live.asks.answer(ask_id, ask::Answer::Allow)?;
                live.allow_until_stop(Target::Host { host })?;
                self.live_changed(key);
                Ok(())
            }
            AskAnswer::Always => {
                let draft = RuleDraft { id: None, target: Target::Host { host }, action: Action::Allow, methods: None };
                self.put_rule(&cage, draft)?;
                // The new rule can already have released the request.
                match live.asks.answer(ask_id, ask::Answer::Allow) {
                    Err(Error::AskNotFound) => Ok(()),
                    other => other,
                }
            }
        }
    }

    /// The Allow rules until stop of VM `key`. None if it does not run.
    pub fn temporary_rules(&self, key: &str) -> Vec<Rule> {
        self.live_of(key).map(|(live, _)| live.temporary()).unwrap_or_default()
    }

    /// Allows `host`, or a `*.` wildcard, in VM `key` until it stops.
    pub fn add_temporary_rule(&self, key: &str, host: &str) -> Result<Rule> {
        let (live, _) = self.live_of(key)?;
        let target = network::normalize_target(Target::Host { host: host.to_owned() })?;
        let rule = live.allow_until_stop(target)?;
        self.live_changed(key);
        Ok(rule)
    }

    pub fn remove_temporary_rule(&self, key: &str, rule_id: &str) -> Result<()> {
        let (live, _) = self.live_of(key)?;
        live.remove_temporary(rule_id)?;
        self.live_changed(key);
        Ok(())
    }

    /// The saved profiles, by name. A missing file has none.
    pub fn network_profiles(&self) -> Result<Vec<Profile>> {
        let _edit = lock(&self.profiles);
        self.read_profiles()
    }

    fn read_profiles(&self) -> Result<Vec<Profile>> {
        match std::fs::read(self.store.dir().join(PROFILES)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| Error::InvalidProfile(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Saves the default and the rules of the cage as profile `name`. A
    /// profile with this name is replaced. Credentials are not copied.
    pub fn save_network_profile(&self, cage_id: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > NAME_LENGTH {
            return Err(Error::InvalidProfile(format!("a name has 1 to {NAME_LENGTH} characters")));
        }
        let network = lock(&self.state).cage(cage_id)?.network.clone();
        let rules = network
            .rules
            .into_iter()
            .map(|r| ProfileRule { target: r.target, action: r.action, methods: r.methods })
            .collect();
        let profile = Profile { name: name.to_owned(), default: network.default, rules };
        let _edit = lock(&self.profiles);
        let mut profiles = self.read_profiles()?;
        profiles.retain(|p| p.name != profile.name);
        if profiles.len() >= PROFILE_LIMIT {
            return Err(Error::InvalidProfile(format!("the app has at most {PROFILE_LIMIT} profiles")));
        }
        profiles.push(profile);
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        store::write(&self.store.dir().join(PROFILES), &profiles)
    }

    pub fn delete_network_profile(&self, name: &str) -> Result<()> {
        let _edit = lock(&self.profiles);
        let mut profiles = self.read_profiles()?;
        let i = profiles.iter().position(|p| p.name == name).ok_or(Error::ProfileNotFound)?;
        profiles.remove(i);
        store::write(&self.store.dir().join(PROFILES), &profiles)
    }

    /// Copies profile `name` into the cage: its default and its rules
    /// replace the cage's. A rule of the cage that holds a credential stays,
    /// with the profile's action and methods if the profile has its target.
    /// Offline and the host port grant stay.
    pub fn apply_network_profile(&self, cage_id: &str, name: &str) -> Result<()> {
        let profile = self.network_profiles()?.into_iter().find(|p| p.name == name).ok_or(Error::ProfileNotFound)?;
        let _edit = lock(&self.secret_edit);
        self.edit_network(cage_id, |n| {
            *n = applied(n, &profile)?;
            Ok(())
        })
    }

    /// The effective policy of the cage as JSON, for the user to read: the
    /// saved policy, the rules until stop of each VM and the fixed floor.
    /// It has credential names and templates, never a value.
    pub fn policy_json(&self, cage_id: &str) -> Result<serde_json::Value> {
        let state = lock(&self.state);
        let cage = state.cage(cage_id)?;
        let until_stop: serde_json::Map<String, serde_json::Value> = state
            .live
            .iter()
            .filter(|(key, _)| state.vms.get(*key).is_some_and(|vm| vm.cage() == cage_id))
            .map(|(key, live)| (key.clone(), serde_json::json!(live.temporary())))
            .collect();
        Ok(serde_json::json!({
            "cage": cage.name,
            "order": ["offline", "alwaysBlocked", "block rules", "allow rules and untilStop", "default"],
            "offline": cage.network.offline,
            "alwaysBlocked": {
                "addresses": "private, loopback, link-local and cloud metadata addresses, local names",
                "refreshHosts": login::denied_hosts(&cage.logins),
            },
            "hostPort": cage.network.host_port,
            "rules": cage.network.rules,
            "untilStop": until_stop,
            "default": cage.network.default,
            "ask": {
                "timeoutSeconds": ask::DEADLINE.as_secs(),
                "pendingPerVm": ask::VM_PENDING,
                "pendingPerApp": ask::APP_PENDING,
            },
        }))
    }
}

/// The policy `current` after `profile` is applied to it.
fn applied(current: &NetworkPolicy, profile: &Profile) -> Result<NetworkPolicy> {
    let mut next = NetworkPolicy { default: profile.default, rules: Vec::new(), ..current.clone() };
    for rule in &profile.rules {
        let draft =
            RuleDraft { id: None, target: rule.target.clone(), action: rule.action, methods: rule.methods.clone() };
        next.put_rule(draft)?;
    }
    for old in current.rules.iter().filter(|r| !r.credentials.is_empty()) {
        match next.rules.iter_mut().find(|r| r.target == old.target) {
            Some(rule) => {
                rule.id = old.id.clone();
                rule.credentials = old.credentials.clone();
            }
            None => next.rules.push(old.clone()),
        }
    }
    next.check()?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cage::testing::*;
    use crate::network::{Credential, Inject, Source};

    fn host(host: &str) -> Target {
        Target::Host { host: host.into() }
    }

    #[test]
    fn a_profile_replaces_the_rules_and_keeps_the_credential_rules() {
        let mut with_key = Rule::new(host("api.io"), Action::Allow);
        with_key.credentials.push(Credential {
            name: "KEY".into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
            source: Source::Local,
        });
        let current = NetworkPolicy {
            default: DefaultAction::Allow,
            offline: true,
            rules: vec![Rule::new(host("old.io"), Action::Allow), with_key.clone()],
            ..Default::default()
        };
        let profile = Profile {
            name: "p".into(),
            default: DefaultAction::Ask,
            rules: vec![
                ProfileRule { target: host("new.io"), action: Action::Block, methods: None },
                ProfileRule { target: host("api.io"), action: Action::Allow, methods: Some(vec!["get".into()]) },
            ],
        };
        let next = applied(&current, &profile).unwrap();
        assert_eq!((next.default, next.offline), (DefaultAction::Ask, true));
        let targets: Vec<&Target> = next.rules.iter().map(|r| &r.target).collect();
        assert_eq!(targets, [&host("new.io"), &host("api.io")]);
        let api = &next.rules[1];
        assert_eq!((api.id.as_str(), api.methods.as_deref()), (with_key.id.as_str(), Some(&["GET".to_owned()][..])));
        assert_eq!(api.credentials, with_key.credentials);
    }

    #[test]
    fn profiles_save_apply_and_delete() {
        let (manager, _runtime, a) = manager();
        manager.put_rule(&a, host_rule("a.io", Action::Allow)).unwrap();
        manager.set_network_default(&a, DefaultAction::Ask).unwrap();
        manager.save_network_profile(&a, " Strict ").unwrap();
        assert!(manager.save_network_profile(&a, " ").is_err());
        assert_eq!(manager.network_profiles().unwrap()[0].name, "Strict");

        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        manager.apply_network_profile(&b, "Strict").unwrap();
        let network = |id: &str| lock(&manager.state).cage(id).unwrap().network.clone();
        assert_eq!(network(&b).default, DefaultAction::Ask);
        assert_eq!(network(&b).rules[0].target, host("a.io"));
        // No live link: a later change of the first cage stays there.
        manager.put_rule(&a, host_rule("b.io", Action::Allow)).unwrap();
        assert_eq!(network(&b).rules.len(), 1);

        manager.delete_network_profile("Strict").unwrap();
        assert!(manager.network_profiles().unwrap().is_empty());
        assert!(matches!(manager.apply_network_profile(&b, "Strict"), Err(Error::ProfileNotFound)));
    }

    #[tokio::test]
    async fn rules_until_stop_reach_the_proxy_and_end_with_the_vm() {
        let (manager, _runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let rule = manager.add_temporary_rule(&id, " A.io ").unwrap();
        assert_eq!(rule.target, host("a.io"));
        assert_eq!(live(&manager, &id).snapshot().temporary[0].id, rule.id);
        assert_eq!(manager.temporary_rules(&id), std::slice::from_ref(&rule));
        // Never saved.
        assert!(lock(&manager.state).cage(&id).unwrap().network.rules.is_empty());
        let json = manager.policy_json(&id).unwrap();
        assert_eq!(json["untilStop"][&id][0]["id"], rule.id.as_str());

        manager.remove_temporary_rule(&id, &rule.id).unwrap();
        assert!(manager.temporary_rules(&id).is_empty());
        manager.add_temporary_rule(&id, "b.io").unwrap();
        manager.stop(&id, None).await.unwrap();
        manager.start(&id).await.unwrap();
        assert!(manager.temporary_rules(&id).is_empty());
        assert!(matches!(manager.answer_ask(&id, "none", AskAnswer::Once), Err(Error::AskNotFound)));
    }
}
