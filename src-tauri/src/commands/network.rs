use crate::cage;
use crate::cage::network::NetTest;
use crate::error::Result;
use crate::network::{self, DefaultAction, Inject, RuleDraft, Service};
use crate::proxy::log::Entry;
use crate::secrets::SecretValue;

use super::Projects;

#[tauri::command]
pub fn network_catalog() -> &'static [Service] {
    network::CATALOG
}

#[tauri::command]
pub fn set_network_default(projects: Projects<'_>, id: String, default: DefaultAction) -> Result<()> {
    projects.set_network_default(&id, default)
}

#[tauri::command]
pub fn set_offline(projects: Projects<'_>, id: String, offline: bool) -> Result<()> {
    projects.set_offline(&id, offline)
}

#[tauri::command]
pub fn put_rule(projects: Projects<'_>, id: String, rule: RuleDraft) -> Result<String> {
    projects.put_rule(&id, rule)
}

#[tauri::command]
pub fn wildcard_for(host: String) -> Option<String> {
    cage::wildcard_for(&host)
}

#[tauri::command]
pub fn list_net_events(projects: Projects<'_>, project_id: String) -> Result<Vec<Entry>> {
    projects.list_net_events(&project_id)
}

// The credential commands are async, so the Keychain calls do not block the main thread.

// The credential commands are async, so the Keychain calls do not block the main thread.

#[tauri::command]
pub async fn remove_rule(projects: Projects<'_>, id: String, rule_id: String) -> Result<()> {
    projects.remove_rule(&id, &rule_id)
}

/// `inject` `None` removes the credential. `value` `None` keeps the stored value.
#[tauri::command]
pub async fn set_rule_credential(
    projects: Projects<'_>,
    id: String,
    rule_id: String,
    name: String,
    inject: Option<Inject>,
    value: Option<SecretValue>,
) -> Result<()> {
    projects.set_rule_credential(&id, &rule_id, &name, inject, value)
}

#[tauri::command]
pub async fn test_network(projects: Projects<'_>, id: String, host: String) -> Result<NetTest> {
    projects.test_network(&id, &host).await
}
