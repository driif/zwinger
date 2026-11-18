use crate::cage;
use crate::cage::network::NetTest;
use crate::error::Result;
use crate::network::{self, DefaultAction, Inject, RuleDraft, Service};
use crate::proxy::log::Entry;
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

// The credential commands are async, so the vault file I/O does not block the main thread.

#[tauri::command]
pub async fn remove_rule(manager: Managed<'_>, cage_id: String, rule_id: String) -> Result<()> {
    manager.remove_rule(&cage_id, &rule_id)
}

/// `inject` `None` removes the credential. `value` `None` keeps the stored value.
#[tauri::command]
pub async fn set_rule_credential(
    manager: Managed<'_>,
    cage_id: String,
    rule_id: String,
    name: String,
    inject: Option<Inject>,
    value: Option<SecretValue>,
) -> Result<()> {
    manager.set_rule_credential(&cage_id, &rule_id, &name, inject, value)
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
