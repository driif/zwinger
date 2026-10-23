use crate::cage::AppDraft;
use crate::error::Result;

use super::Projects;

#[tauri::command]
pub fn put_app(projects: Projects<'_>, id: String, app: AppDraft) -> Result<String> {
    projects.put_app(&id, app)
}

#[tauri::command]
pub fn remove_app(projects: Projects<'_>, id: String, app_id: String) -> Result<()> {
    projects.remove_app(&id, &app_id)
}

#[tauri::command]
pub fn reorder_apps(projects: Projects<'_>, id: String, app_ids: Vec<String>) -> Result<()> {
    projects.reorder_apps(&id, &app_ids)
}

#[tauri::command]
pub async fn start_app(projects: Projects<'_>, id: String, app_id: String) -> Result<String> {
    projects.start_app(&id, &app_id).await
}

#[tauri::command]
pub fn stop_app(projects: Projects<'_>, id: String, app_id: String) -> Result<()> {
    projects.stop_app(&id, &app_id)
}

#[tauri::command]
pub async fn restart_app(projects: Projects<'_>, id: String, app_id: String) -> Result<String> {
    projects.restart_app(&id, &app_id).await
}
