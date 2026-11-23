use tauri::{AppHandle, Manager as _};
use tauri_plugin_dialog::DialogExt as _;

use crate::cage;
use crate::cage::manager::Manager;
use crate::cage::network::{LibraryCredential, NetTest};
use crate::cage::policy::{AskAnswer, Profile};
use crate::cage::undo::Removal;
use crate::cage::vm_state;
use crate::error::{Error, Result};
use crate::network::{self, DefaultAction, Inject, Rule, RuleDraft, Service, Source};
use crate::proxy::ask::Ask;
use crate::proxy::log::Entry;
use crate::runtime::Microsandbox;
use crate::secrets::SecretValue;

use super::Managed;

/// The SHA-256 of the proxy CA certificate, as colon hex. Public data only.
#[tauri::command]
pub fn proxy_ca_fingerprint(manager: Managed<'_>) -> String {
    manager.ca.fingerprint()
}

#[tauri::command]
pub fn network_catalog() -> &'static [Service] {
    network::CATALOG
}

#[tauri::command]
pub fn set_network_default(manager: Managed<'_>, cage_id: String, default: DefaultAction) -> Result<()> {
    manager.set_network_default(&cage_id, default)
}

#[tauri::command]
pub fn set_offline(manager: Managed<'_>, cage_id: String, offline: bool) -> Result<()> {
    manager.set_offline(&cage_id, offline)
}

/// `None` removes the host port grant.
#[tauri::command]
pub fn set_host_port(manager: Managed<'_>, cage_id: String, port: Option<u16>) -> Result<()> {
    manager.set_host_port(&cage_id, port)
}

#[tauri::command]
pub fn put_rule(manager: Managed<'_>, cage_id: String, rule: RuleDraft) -> Result<String> {
    manager.put_rule(&cage_id, rule)
}

#[tauri::command]
pub fn wildcard_for(host: String) -> Option<String> {
    cage::wildcard_for(&host)
}

#[tauri::command]
pub fn list_net_events(manager: Managed<'_>, vm_key: String) -> Result<Vec<Entry>> {
    manager.list_net_events(&vm_key)
}

/// Asks for a place and writes the network log of VM `vm_key` there.
/// `false`: the user cancelled.
#[tauri::command]
pub async fn export_net_events(app: AppHandle, vm_key: String) -> Result<bool> {
    if !vm_state::vm_key(&vm_key) {
        return Err(Error::BadVmKey(vm_key));
    }
    tokio::task::spawn_blocking(move || {
        let Some(file) =
            app.dialog().file().add_filter("JSON", &["json"]).set_file_name(export_name(&vm_key)).blocking_save_file()
        else {
            return Ok(false);
        };
        let dest = file.into_path().map_err(std::io::Error::other)?;
        app.state::<Manager<Microsandbox>>().export_net_events(&vm_key, &dest).map(|()| true)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// The suggested file name of the export of VM `vm_key`. A key over 64
/// bytes is cut and gets a short hash, so the name stays short enough for
/// the file system and two long keys still get different names.
fn export_name(vm_key: &str) -> String {
    const PREFIX: usize = 64;
    if vm_key.len() <= PREFIX {
        return format!("{vm_key}-network.json");
    }
    let mut end = PREFIX;
    while !vm_key.is_char_boundary(end) {
        end -= 1;
    }
    let hash = &cage::activity::file_id(vm_key)[..8];
    format!("{}-{hash}-network.json", &vm_key[..end])
}

// The credential commands are async, so the vault file I/O does not block the main thread.

/// The receipt can undo the removal for a short time.
#[tauri::command]
pub async fn remove_rule(manager: Managed<'_>, cage_id: String, rule_id: String) -> Result<Removal> {
    manager.remove_rule(&cage_id, &rule_id)
}

/// `inject` `None` removes the credential. `value` `None` keeps the stored
/// value. A library credential takes no value.
#[tauri::command]
pub async fn set_rule_credential(
    manager: Managed<'_>,
    cage_id: String,
    rule_id: String,
    name: String,
    inject: Option<Inject>,
    source: Source,
    value: Option<SecretValue>,
) -> Result<()> {
    manager.set_rule_credential(&cage_id, &rule_id, &name, inject, source, value)
}

/// The names and dates of the library entries. Returns no values.
#[tauri::command]
pub async fn library_credentials(manager: Managed<'_>) -> Result<Vec<LibraryCredential>> {
    manager.library_credentials()
}

/// Adds a library value, or with `rotate` replaces one. It attaches it to no rule.
#[tauri::command]
pub async fn set_library_credential(
    manager: Managed<'_>,
    name: String,
    value: SecretValue,
    rotate: bool,
) -> Result<()> {
    manager.set_library_credential(&name, value, rotate)
}

/// Refused while a rule of a cage attaches the entry.
#[tauri::command]
pub async fn delete_library_credential(manager: Managed<'_>, name: String) -> Result<()> {
    manager.delete_library_credential(&name)
}

/// The names of the cage's credentials that have a stored value. Returns no values.
#[tauri::command]
pub async fn stored_credentials(manager: Managed<'_>, cage_id: String) -> Result<Vec<String>> {
    manager.stored_credentials(&cage_id)
}

#[tauri::command]
pub async fn test_network(manager: Managed<'_>, cage_id: String, host: String) -> Result<NetTest> {
    manager.test_network(&cage_id, &host).await
}

/// The pending asks of VM `vm_key`. None if it does not run.
#[tauri::command]
pub fn list_asks(manager: Managed<'_>, vm_key: String) -> Vec<Ask> {
    manager.list_asks(&vm_key)
}

#[tauri::command]
pub fn answer_ask(manager: Managed<'_>, vm_key: String, ask_id: String, answer: AskAnswer) -> Result<()> {
    manager.answer_ask(&vm_key, &ask_id, answer)
}

/// The Allow rules until stop of VM `vm_key`. None if it does not run.
#[tauri::command]
pub fn temporary_rules(manager: Managed<'_>, vm_key: String) -> Vec<Rule> {
    manager.temporary_rules(&vm_key)
}

#[tauri::command]
pub fn add_temporary_rule(manager: Managed<'_>, vm_key: String, host: String) -> Result<Rule> {
    manager.add_temporary_rule(&vm_key, &host)
}

#[tauri::command]
pub fn remove_temporary_rule(manager: Managed<'_>, vm_key: String, rule_id: String) -> Result<()> {
    manager.remove_temporary_rule(&vm_key, &rule_id)
}

#[tauri::command]
pub async fn network_profiles(manager: Managed<'_>) -> Result<Vec<Profile>> {
    manager.network_profiles()
}

#[tauri::command]
pub async fn save_network_profile(manager: Managed<'_>, cage_id: String, name: String) -> Result<()> {
    manager.save_network_profile(&cage_id, &name)
}

#[tauri::command]
pub async fn apply_network_profile(manager: Managed<'_>, cage_id: String, name: String) -> Result<()> {
    manager.apply_network_profile(&cage_id, &name)
}

#[tauri::command]
pub async fn delete_network_profile(manager: Managed<'_>, name: String) -> Result<()> {
    manager.delete_network_profile(&name)
}

/// The effective policy of the cage, read-only. It has no credential values.
#[tauri::command]
pub fn policy_json(manager: Managed<'_>, cage_id: String) -> Result<serde_json::Value> {
    manager.policy_json(&cage_id)
}

#[cfg(test)]
mod tests {
    use super::export_name;

    #[test]
    fn the_export_name_of_a_long_key_is_short_and_unique() {
        assert_eq!(export_name("cage-a"), "cage-a-network.json");
        let (a, b) = ("a".repeat(255), format!("{}b", "a".repeat(254)));
        assert!(export_name(&a).len() <= 64 + 22);
        assert!(export_name(&a).starts_with(&"a".repeat(64)));
        assert_ne!(export_name(&a), export_name(&b));
        // A cut never splits a character.
        assert!(export_name(&format!("{}é{}", "a".repeat(63), "x".repeat(10))).starts_with(&"a".repeat(63)));
    }
}
