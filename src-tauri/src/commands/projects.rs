use std::collections::BTreeMap;

use crate::agent::AgentKind;
use crate::cage::manager::ProjectView;
use crate::cage::{self, HostAgentUse, Pos};
use crate::error::Result;

use super::Projects;

#[tauri::command]
pub fn list_projects(projects: Projects<'_>) -> Vec<ProjectView> {
    projects.list()
}

#[tauri::command]
pub fn create_project(projects: Projects<'_>, folder: String) -> Result<String> {
    projects.create(&folder)
}

#[tauri::command]
pub async fn delete_project(projects: Projects<'_>, id: String) -> Result<()> {
    projects.delete(&id).await
}

#[tauri::command]
pub fn rename_project(projects: Projects<'_>, id: String, name: String) -> Result<()> {
    projects.rename(&id, &name)
}

#[tauri::command]
pub fn reorder_projects(projects: Projects<'_>, ids: Vec<String>) -> Result<()> {
    projects.reorder_projects(&ids)
}

#[tauri::command]
pub fn set_layout(projects: Projects<'_>, id: String, layout: Option<BTreeMap<String, Pos>>) -> Result<()> {
    projects.set_layout(&id, layout)
}

#[tauri::command]
pub fn resolve_folder(path: String) -> Result<String> {
    cage::resolve_folder(&path)
}

#[tauri::command]
pub fn set_harness(projects: Projects<'_>, id: String, harness: Option<String>) -> Result<()> {
    projects.set_harness(&id, harness.as_deref())
}

#[tauri::command]
pub fn list_folders(projects: Projects<'_>, id: String) -> Result<Vec<String>> {
    projects.list_folders(&id)
}

#[tauri::command]
pub fn add_mount(projects: Projects<'_>, id: String, host: String, guest: String, readonly: bool) -> Result<()> {
    projects.add_mount(&id, &host, &guest, readonly)
}

#[tauri::command]
pub fn remove_mount(projects: Projects<'_>, id: String, guest: String) -> Result<()> {
    projects.remove_mount(&id, &guest)
}

#[tauri::command]
pub fn set_mount_readonly(projects: Projects<'_>, id: String, guest: String, readonly: bool) -> Result<()> {
    projects.set_mount_readonly(&id, &guest, readonly)
}

#[tauri::command]
pub async fn start_cage(projects: Projects<'_>, id: String) -> Result<()> {
    projects.start(&id).await
}

#[tauri::command]
pub async fn stop_cage(projects: Projects<'_>, id: String) -> Result<()> {
    projects.stop(&id).await
}

#[tauri::command]
pub async fn put_host_agent(projects: Projects<'_>, id: String, agent: HostAgentUse, allow_hosts: bool) -> Result<()> {
    projects.put_host_agent(&id, agent, allow_hosts).await
}

#[tauri::command]
pub fn remove_host_agent(projects: Projects<'_>, id: String, agent: AgentKind) -> Result<()> {
    projects.remove_host_agent(&id, agent)
}

#[tauri::command]
pub fn remove_guest_login(projects: Projects<'_>, id: String, agent: AgentKind) -> Result<()> {
    projects.remove_guest_login(&id, agent)
}

#[tauri::command]
pub fn confirm_paths(projects: Projects<'_>, id: String) -> Result<()> {
    projects.confirm_paths(&id)
}

/// Each notice comes once, so a reload does not show it again.
#[tauri::command]
pub fn take_app_notices(projects: Projects<'_>) -> Vec<String> {
    projects.take_app_notices()
}

#[tauri::command]
pub fn set_agents_unprompted(projects: Projects<'_>, id: String, on: bool) -> Result<()> {
    projects.set_agents_unprompted(&id, on)
}
