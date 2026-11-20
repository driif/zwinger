//! Undo receipts: a removed rule or mount can come back for a short time.
//!
//! A removal that can be undone returns a [`Removal`] with a random token.
//! The [`Undo`] store keeps what the removal took until the deadline: the
//! rule with the exact vault entries of its local credentials, or the mount
//! with its stored folder identity. The kept values stay in process memory
//! only. They are never saved, logged or sent to the UI.
//!
//! Lock order: `secret_edit`, then `undo`, then `state`. `undo` is never
//! taken while `state` is held.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Serialize;
use uuid::Uuid;

use crate::agent::AgentKind;
use crate::cage::manager::{Manager, lock};
use crate::cage::{self, CLAUDE_TOKEN, Mount};
use crate::error::{Error, Result};
use crate::network::{RULE_LIMIT, Rule};
use crate::runtime::SandboxRuntime;
use crate::secrets::{Owner, VaultEntry};

/// How long a removal can be undone.
pub(crate) const UNDO_WINDOW: Duration = Duration::from_secs(10);

/// The most receipts that the store keeps. A new one evicts the oldest.
pub(crate) const UNDO_CAPACITY: usize = 32;

/// The receipt of a removal that can be undone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Removal {
    pub token: String,
    pub expires_in_ms: u64,
}

/// The stored value of a local credential of a removed rule. `None`: the
/// credential had no value (Not set).
pub(crate) struct Kept {
    pub(crate) name: String,
    pub(crate) entry: Option<VaultEntry>,
}

/// A removed rule, its index, and the values of its local credentials.
pub(crate) struct RemovedRule {
    pub(crate) rule: Rule,
    pub(crate) index: usize,
    /// The Claude login of the cage was on at the removal. A Claude token
    /// of the rule then came from the login, not from a manual rule.
    pub(crate) claude_login: bool,
    pub(crate) values: Vec<Kept>,
}

pub(crate) enum Payload {
    Rule(RemovedRule),
    Mount { mount: Mount, index: usize },
}

pub(crate) struct Entry {
    token: Uuid,
    cage: String,
    deadline: Instant,
    payload: Payload,
}

/// The receipts that can still be undone, oldest first.
#[derive(Default)]
pub(crate) struct Undo {
    entries: VecDeque<Entry>,
}

impl Undo {
    pub(crate) fn push(&mut self, cage: &str, payload: Payload, now: Instant) -> Removal {
        if self.entries.len() >= UNDO_CAPACITY {
            self.entries.pop_front();
        }
        let token = Uuid::new_v4();
        self.entries.push_back(Entry { token, cage: cage.to_owned(), deadline: now + UNDO_WINDOW, payload });
        Removal { token: token.to_string(), expires_in_ms: UNDO_WINDOW.as_millis() as u64 }
    }

    /// Drops the expired receipts and the values that they keep.
    pub(crate) fn sweep(&mut self, now: Instant) {
        self.entries.retain(|e| e.deadline > now);
    }

    /// Drops all receipts of the cage.
    pub(crate) fn purge(&mut self, cage: &str) {
        self.entries.retain(|e| e.cage != cage);
    }

    /// Takes the receipt out of the store, so that it cannot be used twice.
    /// A receipt of a different cage stays.
    pub(crate) fn claim(&mut self, cage: &str, token: &str, now: Instant) -> Result<Entry> {
        self.sweep(now);
        let token = Uuid::parse_str(token).map_err(|_| Error::UndoExpired)?;
        let i = self.entries.iter().position(|e| e.token == token).ok_or(Error::UndoExpired)?;
        if self.entries[i].cage != cage {
            return Err(Error::UndoConflict("the removal is of a different cage".into()));
        }
        Ok(self.entries.remove(i).expect("the index is in the store"))
    }

    /// Puts a claimed receipt back with its own deadline, after a failed undo.
    pub(crate) fn put_back(&mut self, entry: Entry) {
        let i = self.entries.iter().position(|e| e.deadline > entry.deadline).unwrap_or(self.entries.len());
        self.entries.insert(i, entry);
        if self.entries.len() > UNDO_CAPACITY {
            self.entries.pop_front();
        }
    }
}

impl<R: SandboxRuntime> Manager<R> {
    /// Removes the rule as [`Self::take_rule`] does, and keeps a receipt
    /// for [`Self::undo_removal`].
    pub fn remove_rule(&self, id: &str, rule_id: &str) -> Result<Removal> {
        let _edit = lock(&self.secret_edit);
        let removed = self.take_rule(id, rule_id)?;
        Ok(lock(&self.undo).push(id, Payload::Rule(removed), Instant::now()))
    }

    /// Removes the mount, and keeps a receipt for [`Self::undo_removal`].
    /// The change applies on the next start.
    pub fn remove_mount(&self, cage_id: &str, guest: &str) -> Result<Removal> {
        // A concurrent delete of the cage then purges the receipt after it is stored.
        let _edit = lock(&self.secret_edit);
        let mut removed = None;
        self.edit_cage(cage_id, |c, sessions| {
            let index = c.mounts.iter().position(|m| m.guest == guest).ok_or(Error::MountNotFound)?;
            let mount = c.mounts[index].clone();
            c.remove_mount(sessions, guest)?;
            removed = Some((mount, index));
            Ok(())
        })?;
        let (mount, index) = removed.expect("the edit ran");
        Ok(lock(&self.undo).push(cage_id, Payload::Mount { mount, index }, Instant::now()))
    }

    /// Restores the removal of `token` in cage `cage_id`. An unknown, expired
    /// or used token fails with [`Error::UndoExpired`]. A failed restore
    /// changes nothing and keeps the receipt until its deadline.
    pub fn undo_removal(&self, cage_id: &str, token: &str) -> Result<()> {
        let _edit = lock(&self.secret_edit);
        let entry = lock(&self.undo).claim(cage_id, token, Instant::now())?;
        let restored = match &entry.payload {
            Payload::Rule(removed) => self.restore_rule(cage_id, removed),
            Payload::Mount { mount, index } => self.restore_mount(cage_id, mount, *index),
        };
        if restored.is_err() {
            lock(&self.undo).put_back(entry);
        }
        restored
    }

    /// Drops the expired receipts. It takes only the undo lock.
    pub fn sweep_undo(&self) {
        lock(&self.undo).sweep(Instant::now());
    }

    /// Puts the rule back at its index, under `secret_edit`. Each local
    /// account must be empty (the kept value is written) or hold the kept
    /// entry, with the same save time. A different value or a new save of
    /// the same value is a conflict and stays. The values are written before
    /// the config; a failed save deletes the written values.
    fn restore_rule(&self, id: &str, removed: &RemovedRule) -> Result<()> {
        let RemovedRule { rule, index, claude_login, values } = removed;
        for credential in &rule.credentials {
            self.settle(&credential.key().vault_key(id), &credential.name)?;
        }
        {
            let state = lock(&self.state);
            let cage = state.cage(id)?;
            if let Some(other) = cage.network.rules.iter().find(|r| r.id == rule.id || r.target == rule.target) {
                return Err(Error::UndoConflict(format!("{} is in the list again", other.label())));
            }
            if cage.network.rules.len() >= RULE_LIMIT {
                return Err(full());
            }
            // A token of the login does not come back without the login. A
            // manual token gets the checks of the config save.
            let login_now = cage.host_agent(AgentKind::Claude).is_some_and(|a| a.login);
            if *claude_login && !login_now && rule.credentials.iter().any(|c| c.name == CLAUDE_TOKEN) {
                return Err(Error::UndoConflict("the Claude Code login of the cage is off".into()));
            }
        }
        let mut live = Vec::new();
        for credential in rule.credentials.iter().filter(|c| !c.source.is_local()) {
            let entry = self.vault.entry(&Owner::Library, &credential.name)?;
            let entry = entry.ok_or_else(|| Error::LibraryCredentialNotFound(credential.name.clone()))?;
            live.push((credential.key(), entry.value));
        }
        let owner = Owner::cage(id);
        let mut written: Vec<&str> = Vec::new();
        let roll_back = |written: &[&str]| {
            for name in written {
                self.restore_value(&owner, name, None);
            }
        };
        for credential in rule.credentials.iter().filter(|c| c.source.is_local()) {
            let name = credential.name.as_str();
            let Some(kept) = values.iter().find(|k| k.name == name) else {
                roll_back(&written);
                return Err(Error::UndoConflict(format!("credential {name} was not read at the removal")));
            };
            let held = match self.vault.entry(&owner, name) {
                Ok(held) => held,
                Err(e) => {
                    roll_back(&written);
                    return Err(e);
                }
            };
            match (&kept.entry, held) {
                // The credential had no value, and it has none now.
                (None, None) => {}
                (Some(entry), None) => {
                    // A failed put can still have written the value, so it is rolled back too.
                    written.push(name);
                    if let Err(e) = self.vault.put(&owner, name, entry) {
                        roll_back(&written);
                        return Err(e);
                    }
                    live.push((credential.key(), entry.value.clone()));
                }
                (Some(entry), Some(held)) if held == *entry => {
                    live.push((credential.key(), entry.value.clone()));
                }
                _ => {
                    roll_back(&written);
                    return Err(Error::UndoConflict(format!("credential {name} changed after the removal")));
                }
            }
        }
        let save = self.edit_cage(id, |c, _| {
            if c.network.rules.len() >= RULE_LIMIT {
                return Err(full());
            }
            c.network.rules.insert((*index).min(c.network.rules.len()), rule.clone());
            Ok(())
        });
        if let Err(e) = save {
            roll_back(&written);
            return Err(e);
        }
        self.publish(id, &live);
        Ok(())
    }

    /// Puts the mount back at its index, with the checks of a new mount. The
    /// host folder must still resolve to the stored path and identity. A
    /// mount with no stored identity comes back with none, so it stays
    /// unverified. The change applies on the next start.
    fn restore_mount(&self, id: &str, mount: &Mount, index: usize) -> Result<()> {
        let display = mount.host.display().to_string();
        let (now, now_id) = cage::host_dir_with_id(&display)?;
        if now != mount.host || mount.id.is_some_and(|stored| stored != now_id) {
            return Err(Error::PathChanged(display));
        }
        self.put_mount(id, |c, sessions| {
            let index = index.min(c.mounts.len());
            c.insert_mount(sessions, mount.clone(), index)?;
            Ok(index)
        })
    }
}

fn full() -> Error {
    Error::UndoConflict(format!("the project has {RULE_LIMIT} rules, the most it can have"))
}

#[cfg(test)]
mod tests {
    use super::{Payload, UNDO_CAPACITY, UNDO_WINDOW, Undo};
    use crate::cage::CLAUDE_TOKEN;
    use crate::cage::testing::*;

    type At = std::time::Instant;

    fn rules(manager: &Manager<FakeRuntime>, id: &str) -> Vec<String> {
        let cage = manager.list().cages.into_iter().find(|c| c.cage.id == id).unwrap().cage;
        cage.network.rules.iter().map(|r| r.label()).collect()
    }

    fn mounts(manager: &Manager<FakeRuntime>, id: &str) -> Vec<crate::cage::Mount> {
        manager.list().cages.into_iter().find(|c| c.cage.id == id).unwrap().cage.mounts
    }

    fn unbreak_store(manager: &Manager<FakeRuntime>) {
        std::fs::remove_dir_all(manager.store.cages_path()).unwrap();
    }

    /// A cage with rules a.io, b.io and c.io. a.io and b.io share the local `TOKEN`.
    fn shared_token() -> (Manager<FakeRuntime>, String, Arc<MemoryVault>, String, String) {
        let (manager, _, id, vault) = manager_with_vault();
        let a = allow(&manager, &id, "a.io");
        let b = allow(&manager, &id, "b.io");
        allow(&manager, &id, "c.io");
        manager.set_rule_credential(&id, &a, "TOKEN", bearer(), Source::Local, value("first-value")).unwrap();
        manager.set_rule_credential(&id, &b, "TOKEN", bearer(), Source::Local, None).unwrap();
        (manager, id, vault, a, b)
    }

    #[test]
    fn a_removed_rule_comes_back_at_its_index_with_its_value() {
        let (manager, _, id, vault) = manager_with_vault();
        allow(&manager, &id, "a.io");
        let b = allow(&manager, &id, "b.io");
        allow(&manager, &id, "c.io");
        manager.set_rule_credential(&id, &b, "KEY", bearer(), Source::Local, value("v")).unwrap();
        let before = manager.list().cages[0].cage.network.clone();
        let date = vault.entry(&Owner::cage(&id), "KEY").unwrap().unwrap().saved_at;

        let removal = manager.remove_rule(&id, &b).unwrap();
        assert_eq!(removal.expires_in_ms, 10_000);
        assert_eq!(rules(&manager, &id), ["a.io", "c.io"]);
        assert_eq!(stored(&vault, &id, "KEY"), None);

        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(manager.list().cages[0].cage.network, before);
        let entry = vault.entry(&Owner::cage(&id), "KEY").unwrap().unwrap();
        assert_eq!((entry.value.expose(), entry.saved_at), ("v", date));

        // A used token does not restore twice.
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::UndoExpired)));
        assert_eq!(rules(&manager, &id), ["a.io", "b.io", "c.io"]);
    }

    #[tokio::test]
    async fn an_undo_sends_the_rule_and_its_values_to_the_proxy_in_one_step() {
        let (manager, _, id, vault) = manager_with_vault();
        vault.set(&Owner::Library, "LIB", &SecretValue::new("lib")).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v")).unwrap();
        let header = Some(Inject::Set { header: "x-api-key".into(), template: "{secret}".into() });
        manager.set_rule_credential(&id, &rule, "LIB", header, Source::Library, None).unwrap();
        manager.start(&id).await.unwrap();

        let removal = manager.remove_rule(&id, &rule).unwrap();
        let snapshot = live(&manager, &id).snapshot();
        assert!(snapshot.policy.rules.is_empty() && snapshot.values.is_empty());
        // The library value stays in the library.
        assert_eq!(in_library(&vault, "LIB").as_deref(), Some("lib"));

        manager.undo_removal(&id, &removal.token).unwrap();
        let snapshot = live(&manager, &id).snapshot();
        assert_eq!(snapshot.policy.rules[0].id, rule);
        assert_eq!(snapshot.values[&local("KEY")], SecretValue::new("v"));
        assert_eq!(snapshot.values[&CredentialRef::library("LIB")], SecretValue::new("lib"));
    }

    #[test]
    fn a_shared_value_stays_and_comes_back_unchanged() {
        let (manager, id, vault, a, _) = shared_token();
        let removal = manager.remove_rule(&id, &a).unwrap();
        // b.io still uses the value, so the removal kept it.
        assert_eq!(stored(&vault, &id, "TOKEN").as_deref(), Some("first-value"));
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(rules(&manager, &id), ["a.io", "b.io", "c.io"]);
        assert_eq!(stored(&vault, &id, "TOKEN").as_deref(), Some("first-value"));
    }

    #[test]
    fn a_missing_library_value_stops_the_undo() {
        let (manager, _, id, vault) = manager_with_vault();
        vault.set(&Owner::Library, "LIB", &SecretValue::new("lib")).unwrap();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "LIB", bearer(), Source::Library, None).unwrap();
        let removal = manager.remove_rule(&id, &rule).unwrap();
        manager.delete_library_credential("LIB").unwrap();

        assert!(
            matches!(manager.undo_removal(&id, &removal.token), Err(Error::LibraryCredentialNotFound(n)) if n == "LIB")
        );
        assert!(rules(&manager, &id).is_empty());
    }

    #[test]
    fn a_rule_with_a_not_set_value_comes_back_with_no_value() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v")).unwrap();
        vault.delete(&Owner::cage(&id), "KEY").unwrap();
        let before = manager.list().cages[0].cage.network.clone();

        let removal = manager.remove_rule(&id, &rule).unwrap();
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(manager.list().cages[0].cage.network, before);
        assert_eq!(stored(&vault, &id, "KEY"), None);
    }

    #[test]
    fn a_value_set_after_a_not_set_removal_is_a_conflict() {
        let (manager, _, id, vault) = manager_with_vault();
        let a = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &a, "KEY", bearer(), Source::Local, value("v")).unwrap();
        vault.delete(&Owner::cage(&id), "KEY").unwrap();
        let removal = manager.remove_rule(&id, &a).unwrap();
        let b = allow(&manager, &id, "b.io");
        manager.set_rule_credential(&id, &b, "KEY", bearer(), Source::Local, value("new")).unwrap();

        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::UndoConflict(_))));
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("new"));
        assert_eq!(rules(&manager, &id), ["b.io"]);
    }

    #[test]
    fn an_undo_does_not_go_above_the_rule_limit() {
        let (manager, _, id, _) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        let removal = manager.remove_rule(&id, &rule).unwrap();
        manager
            .edit_cage(&id, |c, _| {
                for i in 0..crate::network::RULE_LIMIT {
                    let target = Target::Host { host: format!("h{i}.io") };
                    c.network.rules.push(crate::network::Rule::new(target, Action::Allow));
                }
                Ok(())
            })
            .unwrap();

        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::UndoConflict(_))));
        assert_eq!(rules(&manager, &id).len(), crate::network::RULE_LIMIT);
        // The receipt stays: after one rule goes, the undo works.
        let first = manager.list().cages[0].cage.network.rules[0].id.clone();
        manager.remove_rule(&id, &first).unwrap();
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(rules(&manager, &id)[0], "a.io");
    }

    #[test]
    fn shared_token_removed_twice_comes_back_with_the_kept_value() {
        let (manager, id, vault, a, b) = shared_token();
        let first = manager.remove_rule(&id, &a).unwrap();
        let second = manager.remove_rule(&id, &b).unwrap();
        assert_eq!(stored(&vault, &id, "TOKEN"), None);

        manager.undo_removal(&id, &first.token).unwrap();
        assert_eq!(rules(&manager, &id), ["a.io", "c.io"]);
        assert_eq!(stored(&vault, &id, "TOKEN").as_deref(), Some("first-value"));
        // The second undo finds the same value and writes nothing. b.io was
        // first in the list when it was removed, so it is first again.
        manager.undo_removal(&id, &second.token).unwrap();
        assert_eq!(rules(&manager, &id), ["b.io", "a.io", "c.io"]);
        assert_eq!(stored(&vault, &id, "TOKEN").as_deref(), Some("first-value"));
    }

    #[test]
    fn a_rotated_value_is_a_conflict_and_is_not_overwritten() {
        let (manager, id, vault, a, b) = shared_token();
        let removal = manager.remove_rule(&id, &a).unwrap();
        manager.set_rule_credential(&id, &b, "TOKEN", bearer(), Source::Local, value("second-value")).unwrap();

        let error = manager.undo_removal(&id, &removal.token).unwrap_err();
        assert!(matches!(&error, Error::UndoConflict(m) if m.contains("TOKEN")), "{error}");
        assert!(!error.to_string().contains("first-value") && !error.to_string().contains("second-value"));
        assert_eq!(stored(&vault, &id, "TOKEN").as_deref(), Some("second-value"));
        assert_eq!(rules(&manager, &id), ["b.io", "c.io"]);
    }

    #[test]
    fn a_failed_restore_rolls_back_and_can_be_retried() {
        let (manager, id, vault, a, b) = shared_token();
        let removal = manager.remove_rule(&id, &a).unwrap();
        manager.remove_rule(&id, &b).unwrap();

        // The vault write fails: nothing changes.
        *vault.writes_left.lock().unwrap() = Some(0);
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::Vault(_))));
        *vault.writes_left.lock().unwrap() = None;
        assert_eq!(stored(&vault, &id, "TOKEN"), None);

        // The save fails: the written value goes again.
        break_store(&manager);
        assert!(manager.undo_removal(&id, &removal.token).is_err());
        assert_eq!(stored(&vault, &id, "TOKEN"), None);
        unbreak_store(&manager);
        assert_eq!(rules(&manager, &id), ["c.io"]);

        // A later value of the account is not overwritten by the rollback or the retry.
        let c = manager.list().cages[0].cage.network.rules[0].id.clone();
        manager.set_rule_credential(&id, &c, "TOKEN", bearer(), Source::Local, value("later")).unwrap();
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::UndoConflict(_))));
        assert_eq!(stored(&vault, &id, "TOKEN").as_deref(), Some("later"));

        manager.set_rule_credential(&id, &c, "TOKEN", None, Source::Local, None).unwrap();
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(rules(&manager, &id), ["a.io", "c.io"]);
        assert_eq!(stored(&vault, &id, "TOKEN").as_deref(), Some("first-value"));
    }

    #[test]
    fn a_failed_removal_changes_nothing_and_gives_no_receipt() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v")).unwrap();

        break_store(&manager);
        assert!(manager.remove_rule(&id, &rule).is_err());
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));
        assert_eq!(rules(&manager, &id), ["a.io"]);
        assert!(lock(&manager.undo).entries.is_empty());
    }

    #[tokio::test]
    async fn a_failed_delete_after_the_save_keeps_the_value_and_a_valid_receipt() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v")).unwrap();
        let kept = vault.entry(&Owner::cage(&id), "KEY").unwrap();
        manager.start(&id).await.unwrap();

        // The delete fails after the save: the removal is done, and the value stays unused.
        *vault.writes_left.lock().unwrap() = Some(0);
        let removal = manager.remove_rule(&id, &rule).unwrap();
        *vault.writes_left.lock().unwrap() = None;
        assert!(rules(&manager, &id).is_empty());
        let snapshot = live(&manager, &id).snapshot();
        assert!(snapshot.policy.rules.is_empty() && snapshot.values.is_empty());
        assert_eq!(vault.entry(&Owner::cage(&id), "KEY").unwrap(), kept);
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(rules(&manager, &id), ["a.io"]);
        assert_eq!(vault.entry(&Owner::cage(&id), "KEY").unwrap(), kept);
        assert_eq!(live(&manager, &id).snapshot().values[&local("KEY")], SecretValue::new("v"));

        // The start prune deletes the unused value; the undo writes the kept entry back.
        *vault.writes_left.lock().unwrap() = Some(0);
        let removal = manager.remove_rule(&id, &rule).unwrap();
        *vault.writes_left.lock().unwrap() = None;
        prune_vault(&*vault, &[manager.list().cages[0].cage.clone()]);
        assert_eq!(stored(&vault, &id, "KEY"), None);
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(vault.entry(&Owner::cage(&id), "KEY").unwrap(), kept);
    }

    #[test]
    fn a_failed_vault_write_of_the_undo_changes_nothing() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "a.io");
        manager.set_rule_credential(&id, &rule, "KEY", bearer(), Source::Local, value("v")).unwrap();
        let removal = manager.remove_rule(&id, &rule).unwrap();

        // The put changes the value and then fails: the written value goes again.
        vault.fail_after_write.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::Vault(_))));
        assert_eq!(stored(&vault, &id, "KEY"), None);
        assert!(rules(&manager, &id).is_empty());
        assert!(lock(&manager.unsettled).is_empty());

        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(stored(&vault, &id, "KEY").as_deref(), Some("v"));
    }

    #[test]
    fn a_new_save_of_the_same_value_is_a_conflict() {
        let (manager, id, vault, a, _) = shared_token();
        let removal = manager.remove_rule(&id, &a).unwrap();
        let owner = Owner::cage(&id);
        let saved = vault.entry(&owner, "TOKEN").unwrap().unwrap();
        let again = secrets::VaultEntry { value: saved.value.clone(), saved_at: Some(saved.saved_at.unwrap() + 1) };
        vault.put(&owner, "TOKEN", &again).unwrap();

        assert!(
            matches!(manager.undo_removal(&id, &removal.token), Err(Error::UndoConflict(m)) if m.contains("TOKEN"))
        );
        assert_eq!(vault.entry(&owner, "TOKEN").unwrap(), Some(again));
        assert_eq!(rules(&manager, &id), ["b.io", "c.io"]);
    }

    #[test]
    fn a_receipt_expires_by_the_sweep() {
        let (manager, _, id) = manager();
        let rule = allow(&manager, &id, "a.io");
        let removal = manager.remove_rule(&id, &rule).unwrap();
        lock(&manager.undo).sweep(At::now() + UNDO_WINDOW);
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::UndoExpired)));
        assert!(rules(&manager, &id).is_empty());
    }

    #[test]
    fn the_store_expires_on_claim_and_evicts_the_oldest() {
        let mut undo = Undo::default();
        let now = At::now();
        let mount = || Payload::Mount {
            mount: crate::cage::Mount { host: "/x".into(), id: None, guest: "/mnt".into(), readonly: true },
            index: 0,
        };
        let first = undo.push("c", mount(), now);
        let tokens: Vec<String> = (0..UNDO_CAPACITY).map(|_| undo.push("c", mount(), now).token).collect();
        assert_eq!(undo.entries.len(), UNDO_CAPACITY);
        assert!(matches!(undo.claim("c", &first.token, now), Err(Error::UndoExpired)));
        assert!(undo.claim("c", &tokens[0], now).is_ok());
        assert!(matches!(undo.claim("c", &tokens[1], now + UNDO_WINDOW), Err(Error::UndoExpired)));
        assert!(undo.entries.is_empty());
    }

    #[test]
    fn unknown_and_wrong_cage_tokens_fail_and_change_nothing() {
        let (manager, _, id) = manager();
        let other = manager.create_cage("other").unwrap();
        let rule = allow(&manager, &id, "a.io");
        let removal = manager.remove_rule(&id, &rule).unwrap();

        for token in ["nope", &uuid::Uuid::new_v4().to_string()] {
            assert!(matches!(manager.undo_removal(&id, token), Err(Error::UndoExpired)));
        }
        assert!(matches!(manager.undo_removal(&other, &removal.token), Err(Error::UndoConflict(_))));
        assert!(rules(&manager, &id).is_empty());
        // The receipt stays for its own cage.
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(rules(&manager, &id), ["a.io"]);
    }

    #[test]
    fn a_deleted_cage_does_not_come_back() {
        let (manager, _, _) = manager();
        let other = manager.create_cage("other").unwrap();
        let rule = allow(&manager, &other, "a.io");
        let removal = manager.remove_rule(&other, &rule).unwrap();
        manager.delete_cage(&other).unwrap();
        assert!(lock(&manager.undo).entries.is_empty());
        assert!(matches!(manager.undo_removal(&other, &removal.token), Err(Error::UndoExpired)));
        assert!(manager.list().cages.iter().all(|c| c.cage.id != other));
    }

    #[test]
    fn a_claude_token_does_not_come_back_without_the_login() {
        let (manager, _, id, vault) = host_manager(FakeLogins::default(), NOW);
        manager.set_claude_token(&id, value("token"), Source::Local, true, false).unwrap();
        let rule = manager.list().cages[0].cage.network.rules[0].id.clone();
        let removal = manager.remove_rule(&id, &rule).unwrap();
        manager.remove_host_agent(&id, AgentKind::Claude).unwrap();

        let error = manager.undo_removal(&id, &removal.token).unwrap_err();
        assert!(matches!(&error, Error::UndoConflict(m) if m.contains("Claude")), "{error}");
        assert_eq!(stored(&vault, &id, "CLAUDE_CODE_OAUTH_TOKEN"), None);
        assert!(rules(&manager, &id).is_empty());
    }

    #[test]
    fn a_manual_claude_token_comes_back_without_a_login() {
        let (manager, _, id, vault) = manager_with_vault();
        let rule = allow(&manager, &id, "api.anthropic.com");
        manager.set_rule_credential(&id, &rule, CLAUDE_TOKEN, bearer(), Source::Local, value("manual")).unwrap();
        assert!(manager.list().cages[0].cage.host_agent(AgentKind::Claude).is_none());
        let before = manager.list().cages[0].cage.network.clone();

        let removal = manager.remove_rule(&id, &rule).unwrap();
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(manager.list().cages[0].cage.network, before);
        assert_eq!(stored(&vault, &id, CLAUDE_TOKEN).as_deref(), Some("manual"));
    }

    #[test]
    fn a_rule_id_or_target_in_the_list_again_is_a_conflict() {
        let (manager, _, id) = manager();
        let rule = allow(&manager, &id, "a.io");
        let removal = manager.remove_rule(&id, &rule).unwrap();
        let again = allow(&manager, &id, "a.io");
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::UndoConflict(_))));
        assert_eq!(rules(&manager, &id), ["a.io"]);

        // The receipt stays: after the new rule goes, the retry works.
        manager.remove_rule(&id, &again).unwrap();
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(manager.list().cages[0].cage.network.rules[0].id, rule);
    }

    #[test]
    fn a_removed_mount_comes_back_at_its_index() {
        let (manager, _, id) = manager();
        let (one, two) = (temp_dir(), temp_dir());
        manager.add_mount(&id, one.to_str().unwrap(), "/mnt/one", true).unwrap();
        manager.add_mount(&id, two.to_str().unwrap(), "/mnt/two", false).unwrap();
        let before = mounts(&manager, &id);

        let removal = manager.remove_mount(&id, "/mnt/one").unwrap();
        assert_eq!(mounts(&manager, &id).len(), 1);
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(mounts(&manager, &id), before);
    }

    #[test]
    fn a_replaced_folder_does_not_come_back() {
        let (manager, _, id) = manager();
        let dir = temp_dir();
        manager.add_mount(&id, dir.to_str().unwrap(), "/mnt/data", true).unwrap();
        let removal = manager.remove_mount(&id, "/mnt/data").unwrap();
        std::fs::remove_dir(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();

        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::PathChanged(_))));
        assert!(mounts(&manager, &id).is_empty());
    }

    #[test]
    fn a_mount_with_no_identity_comes_back_unverified() {
        let (manager, _, id) = manager();
        let dir = temp_dir();
        manager.add_mount(&id, dir.to_str().unwrap(), "/mnt/data", true).unwrap();
        manager
            .edit_cage(&id, |c, _| {
                c.mounts[0].id = None;
                Ok(())
            })
            .unwrap();
        let removal = manager.remove_mount(&id, "/mnt/data").unwrap();
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(mounts(&manager, &id)[0].id, None);
    }

    #[test]
    fn a_restored_mount_has_the_checks_of_a_new_mount() {
        let (manager, _, id) = manager();
        let dir = temp_dir();
        manager.add_mount(&id, dir.to_str().unwrap(), "/mnt/data", false).unwrap();

        // The guest path is taken again.
        let removal = manager.remove_mount(&id, "/mnt/data").unwrap();
        let other = temp_dir();
        manager.add_mount(&id, other.to_str().unwrap(), "/mnt/data", true).unwrap();
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::DuplicateMount(_))));

        // A writable folder that nests with the new mount overlaps.
        manager.remove_mount(&id, "/mnt/data").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        manager.add_mount(&id, dir.join("sub").to_str().unwrap(), "/mnt/sub", true).unwrap();
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::MountOverlap(..))));
        manager.remove_mount(&id, "/mnt/sub").unwrap();
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(mounts(&manager, &id)[0].guest, "/mnt/data");
    }

    #[test]
    fn a_restored_mount_cannot_reach_the_app_data() {
        let (manager, _, id) = manager();
        // An old config can hold such a mount: the add check is newer.
        let data = manager.data.to_str().unwrap().to_owned();
        manager.edit_cage(&id, |c, sessions| c.add_mount(sessions, &data, "/mnt/data", true)).unwrap();
        let removal = manager.remove_mount(&id, "/mnt/data").unwrap();
        assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::AppDataOverlap(..))));
        assert!(mounts(&manager, &id).is_empty());
    }

    /// Stores a mount of a new folder at `guest` with no checks, as an old
    /// config can hold it.
    fn seed(manager: &Manager<FakeRuntime>, id: &str, guest: &str, readonly: bool) -> crate::cage::Mount {
        let (host, folder_id) = crate::cage::host_dir_with_id(temp_dir().to_str().unwrap()).unwrap();
        let mount = crate::cage::Mount { host, id: Some(folder_id), guest: guest.into(), readonly };
        let stored = mount.clone();
        manager
            .edit_cage(id, |c, _| {
                c.mounts.push(stored);
                Ok(())
            })
            .unwrap();
        mount
    }

    #[test]
    fn a_restored_mount_has_the_guest_path_checks_of_a_new_mount() {
        let rejected = [
            "/workspace",
            "/workspace/x",
            "/opt",
            "/opt/zwinger",
            "/opt/zwinger/x",
            "/",
            "data",
            "/mnt/./data",
            "/mnt/../data",
            "/mnt/data/",
        ];
        for guest in rejected {
            let (manager, _, id) = manager();
            seed(&manager, &id, guest, true);
            let removal = manager.remove_mount(&id, guest).unwrap();
            for _ in 0..2 {
                let error = manager.undo_removal(&id, &removal.token).unwrap_err();
                assert!(
                    matches!(
                        error,
                        Error::WorkspaceOverlap(_) | Error::ReservedGuestPath(_) | Error::InvalidGuestPath(_)
                    ),
                    "{guest}: {error}"
                );
                assert!(mounts(&manager, &id).is_empty(), "{guest}");
            }
        }
    }

    #[test]
    fn a_restored_mount_collides_with_a_legacy_alias() {
        let (manager, _, id) = manager();
        seed(&manager, &id, "/mnt/a", true);
        seed(&manager, &id, "/mnt/a/", true);
        let removal = manager.remove_mount(&id, "/mnt/a").unwrap();
        let after = mounts(&manager, &id);
        for _ in 0..2 {
            assert!(matches!(manager.undo_removal(&id, &removal.token), Err(Error::DuplicateMount(_))));
            assert_eq!(mounts(&manager, &id), after);
        }
    }

    #[test]
    fn a_restored_mount_keeps_its_fields_and_index() {
        let (manager, _, id) = manager();
        let control = seed(&manager, &id, "/__zwinger", true);
        seed(&manager, &id, "/mnt/two", false);
        let before = mounts(&manager, &id);
        let removal = manager.remove_mount(&id, "/__zwinger").unwrap();
        manager.undo_removal(&id, &removal.token).unwrap();
        assert_eq!(mounts(&manager, &id), before);
        assert_eq!(mounts(&manager, &id)[0], control);
    }
}
