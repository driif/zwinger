use crate::agent::AgentKind;
use crate::cage::HostAgentUse;
use crate::error::Result;
use crate::secrets::SecretValue;

use super::Managed;

#[tauri::command]
pub fn add_mount(manager: Managed<'_>, cage_id: String, host: String, guest: String, readonly: bool) -> Result<()> {
    manager.add_mount(&cage_id, &host, &guest, readonly)
}

#[tauri::command]
pub fn remove_mount(manager: Managed<'_>, cage_id: String, guest: String) -> Result<()> {
    manager.remove_mount(&cage_id, &guest)
}

#[tauri::command]
pub fn set_mount_readonly(manager: Managed<'_>, cage_id: String, guest: String, readonly: bool) -> Result<()> {
    manager.set_mount_readonly(&cage_id, &guest, readonly)
}

#[tauri::command]
pub async fn start_cage(manager: Managed<'_>, cage_id: String) -> Result<()> {
    manager.start(&cage_id).await
}

#[tauri::command]
pub async fn stop_cage(manager: Managed<'_>, cage_id: String) -> Result<()> {
    manager.stop(&cage_id).await
}

#[tauri::command]
pub async fn put_host_agent(
    manager: Managed<'_>,
    cage_id: String,
    agent: HostAgentUse,
    allow_hosts: bool,
) -> Result<()> {
    manager.put_host_agent(&cage_id, agent, allow_hosts).await
}

#[tauri::command]
pub fn set_claude_token(
    manager: Managed<'_>,
    cage_id: String,
    value: String,
    allow: bool,
    profile: bool,
) -> Result<()> {
    manager.set_claude_token(&cage_id, SecretValue::new(value), allow, profile)
}

#[tauri::command]
pub fn remove_host_agent(manager: Managed<'_>, cage_id: String, agent: AgentKind) -> Result<()> {
    manager.remove_host_agent(&cage_id, agent)
}

#[tauri::command]
pub fn remove_guest_login(manager: Managed<'_>, cage_id: String, agent: AgentKind) -> Result<()> {
    manager.remove_guest_login(&cage_id, agent)
}

#[tauri::command]
pub fn set_agents_unprompted(manager: Managed<'_>, cage_id: String, on: bool) -> Result<()> {
    manager.set_agents_unprompted(&cage_id, on)
}
