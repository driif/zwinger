use std::collections::BTreeMap;

use tauri::ipc::Channel;
use tauri::{State, Webview};

use crate::cage::manager::Listing;
use crate::cage::{self, FolderList};
use crate::error::Result;
use crate::folder_watch::FolderWatches;
use crate::session::{Pos, Viewport};
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
pub fn set_viewport(manager: Managed<'_>, session_id: String, viewport: Option<Viewport>) -> Result<()> {
    manager.set_viewport(&session_id, viewport)
}

#[tauri::command]
pub fn resolve_folder(path: String) -> Result<String> {
    cage::resolve_folder(&path)
}

#[tauri::command]
pub fn set_harness(manager: Managed<'_>, session_id: String, harness: Option<String>) -> Result<()> {
    manager.set_harness(&session_id, harness.as_deref())
}

/// `None` lets the session follow the app default editor.
#[tauri::command]
pub fn set_project_editor(manager: Managed<'_>, session_id: String, editor: Option<String>) -> Result<()> {
    manager.set_editor(&session_id, editor.as_deref())
}

/// `None` lets the project follow the theme color.
#[tauri::command]
pub fn set_project_accent(manager: Managed<'_>, session_id: String, accent: Option<String>) -> Result<()> {
    manager.set_accent(&session_id, accent.as_deref())
}

#[tauri::command]
pub fn list_folders(manager: Managed<'_>, session_id: String) -> Result<FolderList> {
    manager.list_folders(&session_id)
}

/// Sends the folders of the project now and on each change, until
/// `unwatch_folders`, a new page in the webview, or its close. The reason
/// while the project folder cannot be read.
#[tauri::command]
pub fn watch_folders(
    manager: Managed<'_>,
    watches: State<'_, FolderWatches>,
    webview: Webview,
    session_id: String,
    on_event: Channel<FolderList>,
) -> Result<u64> {
    let root = manager.session_folder(&session_id)?;
    Ok(watches.watch(webview.label(), root, move |folders| on_event.send(folders).is_ok()))
}

#[tauri::command]
pub fn unwatch_folders(watches: State<'_, FolderWatches>, token: u64) {
    watches.remove(token);
}

/// Reads the folders of a watch again now. An error if the watch has ended.
#[tauri::command]
pub fn rescan_folders(watches: State<'_, FolderWatches>, token: u64) -> Result<()> {
    watches.rescan(token)
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
