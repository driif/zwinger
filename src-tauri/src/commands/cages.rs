use crate::agent::AgentKind;
use crate::cage::HostAgentUse;
use crate::cage::lifecycle::Confirmed;
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

/// Starts the VM of the session's effective cage.
#[tauri::command]
pub async fn start_cage(manager: Managed<'_>, session_id: String) -> Result<()> {
    manager.start(&session_id).await
}

/// Restarts the running VM of the session's effective cage with the
/// session as one more member. `members` and `generation` come from the
/// `RestartToAdd` error that the user confirmed.
#[tauri::command]
pub async fn restart_cage_to_add(
    manager: Managed<'_>,
    session_id: String,
    members: Vec<String>,
    generation: u64,
) -> Result<()> {
    manager.restart_to_add(&session_id, &members, generation).await
}

/// Stops the VM that runs with the session as a member. A VM with other
/// members fails with `StopShared` unless `confirmed` has the members and
/// the generation of that error.
#[tauri::command]
pub async fn stop_cage(manager: Managed<'_>, session_id: String, confirmed: Option<Confirmed>) -> Result<()> {
    manager.stop(&session_id, confirmed.as_ref()).await
}

/// Restarts the VM that runs with the session as a member, with all its
/// members. `confirmed` works as for `stop_cage`.
#[tauri::command]
pub async fn restart_cage(manager: Managed<'_>, session_id: String, confirmed: Option<Confirmed>) -> Result<()> {
    manager.restart(&session_id, confirmed.as_ref()).await
}

#[tauri::command]
pub fn create_cage(manager: Managed<'_>, name: String) -> Result<String> {
    manager.create_cage(&name)
}

#[tauri::command]
pub fn rename_cage(manager: Managed<'_>, cage_id: String, name: String) -> Result<()> {
    manager.rename_cage(&cage_id, &name)
}

#[tauri::command]
pub fn delete_cage(manager: Managed<'_>, cage_id: String) -> Result<()> {
    manager.delete_cage(&cage_id)
}

/// `None`: the session inherits the default cage of its workspace.
#[tauri::command]
pub fn set_session_cage(manager: Managed<'_>, session_id: String, cage_id: Option<String>) -> Result<()> {
    manager.set_session_cage(&session_id, cage_id.as_deref())
}

#[tauri::command]
pub fn set_workspace_cage(manager: Managed<'_>, workspace_id: String, cage_id: Option<String>) -> Result<()> {
    manager.set_workspace_cage(&workspace_id, cage_id.as_deref())
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
