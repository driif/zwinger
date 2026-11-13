use std::collections::BTreeMap;

use crate::cage;
use crate::cage::manager::Listing;
use crate::error::Result;
use crate::session::Pos;
use crate::workspaces::{Arrangement, Workspace};

use super::Managed;

/// All sessions and cages, read in one step.
#[tauri::command]
pub fn list_sessions(manager: Managed<'_>) -> Listing {
    manager.list()
}

#[tauri::command]
pub fn create_session(manager: Managed<'_>, folder: String, workspace: Option<String>) -> Result<String> {
    manager.create(&folder, workspace.as_deref())
}

#[tauri::command]
pub async fn delete_session(manager: Managed<'_>, session_id: String) -> Result<()> {
    manager.delete(&session_id).await
}

#[tauri::command]
pub fn rename_session(manager: Managed<'_>, session_id: String, name: String) -> Result<()> {
    manager.rename(&session_id, &name)
}

#[tauri::command]
pub fn list_workspaces(manager: Managed<'_>) -> Vec<Workspace> {
    manager.workspaces()
}

#[tauri::command]
pub fn create_workspace(manager: Managed<'_>) -> Result<String> {
    manager.create_workspace()
}

#[tauri::command]
pub fn rename_workspace(manager: Managed<'_>, id: String, name: String) -> Result<()> {
    manager.rename_workspace(&id, &name)
}

#[tauri::command]
pub fn delete_workspace(manager: Managed<'_>, id: String) -> Result<()> {
    manager.delete_workspace(&id)
}

#[tauri::command]
pub fn arrange_workspaces(manager: Managed<'_>, workspaces: Vec<Arrangement>) -> Result<()> {
    manager.arrange_workspaces(workspaces)
}

#[tauri::command]
pub fn set_layout(manager: Managed<'_>, session_id: String, layout: Option<BTreeMap<String, Pos>>) -> Result<()> {
    manager.set_layout(&session_id, layout)
}

#[tauri::command]
pub fn resolve_folder(path: String) -> Result<String> {
    cage::resolve_folder(&path)
}

#[tauri::command]
pub fn set_harness(manager: Managed<'_>, session_id: String, harness: Option<String>) -> Result<()> {
    manager.set_harness(&session_id, harness.as_deref())
}

#[tauri::command]
pub fn list_folders(manager: Managed<'_>, session_id: String) -> Result<Vec<String>> {
    manager.list_folders(&session_id)
}

/// Confirms the session folder and the mounts of its cage.
#[tauri::command]
pub fn confirm_paths(manager: Managed<'_>, session_id: String) -> Result<()> {
    manager.confirm_paths(&session_id)
}

/// Each notice comes once, so a reload does not show it again.
#[tauri::command]
pub fn take_app_notices(manager: Managed<'_>) -> Vec<String> {
    manager.take_app_notices()
}
