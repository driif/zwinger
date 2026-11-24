use crate::agent::AgentKind;
use crate::cage::HostAgentUse;
use crate::cage::lifecycle::Confirmed;
use crate::cage::undo::Removal;
use crate::error::Result;
use crate::network::Source;
use crate::secrets::SecretValue;

use super::Managed;

#[tauri::command]
pub fn add_mount(manager: Managed<'_>, cage_id: String, host: String, guest: String, readonly: bool) -> Result<()> {
    manager.add_mount(&cage_id, &host, &guest, readonly)
}

/// The receipt can undo the removal for a short time.
#[tauri::command]
pub fn remove_mount(manager: Managed<'_>, cage_id: String, guest: String) -> Result<Removal> {
    manager.remove_mount(&cage_id, &guest)
}

/// Restores a rule or mount removal of the cage, before its receipt expires.
#[tauri::command]
pub async fn undo_removal(manager: Managed<'_>, cage_id: String, token: String) -> Result<()> {
    manager.undo_removal(&cage_id, &token)
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

/// Puts the session in a VM of its own, or back in the shared VM of its
/// cage. A VM with other members fails with `StopShared` unless `confirmed`
/// has the members and the generation of that error.
#[tauri::command]
pub async fn set_session_vm(
    manager: Managed<'_>,
    session_id: String,
    own: bool,
    confirmed: Option<Confirmed>,
) -> Result<()> {
    manager.set_session_vm(&session_id, own, confirmed.as_ref()).await
}

#[tauri::command]
pub fn create_cage(manager: Managed<'_>, name: String) -> Result<String> {
    manager.create_cage(&name)
}

/// A copy of the cage with no vault values. Returns its id.
#[tauri::command]
pub fn duplicate_cage(manager: Managed<'_>, cage_id: String, name: String) -> Result<String> {
    manager.duplicate_cage(&cage_id, &name)
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

/// `value` is for a local token only; a library token comes from the library.
/// A running VM of the cage gets the change at once.
#[tauri::command]
pub async fn set_claude_token(
    manager: Managed<'_>,
    cage_id: String,
    value: Option<SecretValue>,
    source: Source,
    allow: bool,
    profile: bool,
) -> Result<()> {
    manager.set_claude_token(&cage_id, value, source, allow, profile)?;
    manager.refresh_agents(Some(&cage_id)).await;
    Ok(())
}

#[tauri::command]
pub async fn remove_host_agent(manager: Managed<'_>, cage_id: String, agent: AgentKind) -> Result<()> {
    manager.remove_host_agent(&cage_id, agent)?;
    manager.refresh_agents(Some(&cage_id)).await;
    Ok(())
}

#[tauri::command]
pub fn remove_guest_login(manager: Managed<'_>, vm_key: String, agent: AgentKind) -> Result<()> {
    manager.remove_guest_login(&vm_key, agent)
}

#[tauri::command]
pub fn set_cage_resources(manager: Managed<'_>, cage_id: String, cpus: u32, memory_mib: u32) -> Result<()> {
    manager.set_resources(&cage_id, cpus, memory_mib)
}

#[tauri::command]
pub fn set_agents_unprompted(manager: Managed<'_>, cage_id: String, on: bool) -> Result<()> {
    manager.set_agents_unprompted(&cage_id, on)
}
