use crate::error::Result;
use crate::network::{self, DefaultAction, Inject, RuleDraft, Service};
use crate::proxy::log::Entry;
use crate::secrets::SecretValue;
use crate::shield;
use crate::shield::network::NetTest;

use super::Shields;

#[tauri::command]
pub fn network_catalog() -> &'static [Service] {
    network::CATALOG
}

#[tauri::command]
pub fn set_network_default(shields: Shields<'_>, id: String, default: DefaultAction) -> Result<()> {
    shields.set_network_default(&id, default)
}

#[tauri::command]
pub fn set_offline(shields: Shields<'_>, id: String, offline: bool) -> Result<()> {
    shields.set_offline(&id, offline)
}

#[tauri::command]
pub fn put_rule(shields: Shields<'_>, id: String, rule: RuleDraft) -> Result<String> {
    shields.put_rule(&id, rule)
}

#[tauri::command]
pub fn wildcard_for(host: String) -> Option<String> {
    shield::wildcard_for(&host)
}

#[tauri::command]
pub fn list_net_events(shields: Shields<'_>, shield_id: String) -> Result<Vec<Entry>> {
    shields.list_net_events(&shield_id)
}

// The credential commands are async, so the Keychain calls do not block the main thread.

// The credential commands are async, so the Keychain calls do not block the main thread.

#[tauri::command]
pub async fn remove_rule(shields: Shields<'_>, id: String, rule_id: String) -> Result<()> {
    shields.remove_rule(&id, &rule_id)
}

/// `inject` `None` removes the credential. `value` `None` keeps the stored value.
#[tauri::command]
pub async fn set_rule_credential(
    shields: Shields<'_>,
    id: String,
    rule_id: String,
    name: String,
    inject: Option<Inject>,
    value: Option<SecretValue>,
) -> Result<()> {
    shields.set_rule_credential(&id, &rule_id, &name, inject, value)
}

#[tauri::command]
pub async fn test_network(shields: Shields<'_>, id: String, host: String) -> Result<NetTest> {
    shields.test_network(&id, &host).await
}
