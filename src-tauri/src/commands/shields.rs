use std::collections::BTreeMap;

use crate::agent::AgentKind;
use crate::error::Result;
use crate::shield::manager::ShieldView;
use crate::shield::{self, HostAgentUse, Pos};

use super::Shields;

#[tauri::command]
pub fn list_shields(shields: Shields<'_>) -> Vec<ShieldView> {
    shields.list()
}

#[tauri::command]
pub fn create_shield(shields: Shields<'_>, folder: String) -> Result<String> {
    shields.create(&folder)
}

#[tauri::command]
pub async fn delete_shield(shields: Shields<'_>, id: String) -> Result<()> {
    shields.delete(&id).await
}

#[tauri::command]
pub fn rename_shield(shields: Shields<'_>, id: String, name: String) -> Result<()> {
    shields.rename(&id, &name)
}

#[tauri::command]
pub fn reorder_shields(shields: Shields<'_>, ids: Vec<String>) -> Result<()> {
    shields.reorder_shields(&ids)
}

#[tauri::command]
pub fn set_layout(shields: Shields<'_>, id: String, layout: Option<BTreeMap<String, Pos>>) -> Result<()> {
    shields.set_layout(&id, layout)
}

#[tauri::command]
pub fn resolve_folder(path: String) -> Result<String> {
    shield::resolve_folder(&path)
}

#[tauri::command]
pub fn set_harness(shields: Shields<'_>, id: String, harness: Option<String>) -> Result<()> {
    shields.set_harness(&id, harness.as_deref())
}

#[tauri::command]
pub fn list_folders(shields: Shields<'_>, id: String) -> Result<Vec<String>> {
    shields.list_folders(&id)
}

#[tauri::command]
pub fn add_mount(shields: Shields<'_>, id: String, host: String, guest: String, readonly: bool) -> Result<()> {
    shields.add_mount(&id, &host, &guest, readonly)
}

#[tauri::command]
pub fn remove_mount(shields: Shields<'_>, id: String, guest: String) -> Result<()> {
    shields.remove_mount(&id, &guest)
}

#[tauri::command]
pub fn set_mount_readonly(shields: Shields<'_>, id: String, guest: String, readonly: bool) -> Result<()> {
    shields.set_mount_readonly(&id, &guest, readonly)
}

#[tauri::command]
pub async fn start_shield(shields: Shields<'_>, id: String) -> Result<()> {
    shields.start(&id).await
}

#[tauri::command]
pub async fn stop_shield(shields: Shields<'_>, id: String) -> Result<()> {
    shields.stop(&id).await
}

#[tauri::command]
pub async fn put_host_agent(shields: Shields<'_>, id: String, agent: HostAgentUse, allow_hosts: bool) -> Result<()> {
    shields.put_host_agent(&id, agent, allow_hosts).await
}

#[tauri::command]
pub fn remove_host_agent(shields: Shields<'_>, id: String, agent: AgentKind) -> Result<()> {
    shields.remove_host_agent(&id, agent)
}

#[tauri::command]
pub fn remove_guest_login(shields: Shields<'_>, id: String, agent: AgentKind) -> Result<()> {
    shields.remove_guest_login(&id, agent)
}

#[tauri::command]
pub fn confirm_paths(shields: Shields<'_>, id: String) -> Result<()> {
    shields.confirm_paths(&id)
}

/// Each notice comes once, so a reload does not show it again.
#[tauri::command]
pub fn take_app_notices(shields: Shields<'_>) -> Vec<String> {
    shields.take_app_notices()
}

#[tauri::command]
pub fn set_agents_unprompted(shields: Shields<'_>, id: String, on: bool) -> Result<()> {
    shields.set_agents_unprompted(&id, on)
}
