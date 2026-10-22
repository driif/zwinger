use crate::error::Result;
use crate::shield::AppDraft;

use super::Shields;

#[tauri::command]
pub fn put_app(shields: Shields<'_>, id: String, app: AppDraft) -> Result<String> {
    shields.put_app(&id, app)
}

#[tauri::command]
pub fn remove_app(shields: Shields<'_>, id: String, app_id: String) -> Result<()> {
    shields.remove_app(&id, &app_id)
}

#[tauri::command]
pub fn reorder_apps(shields: Shields<'_>, id: String, app_ids: Vec<String>) -> Result<()> {
    shields.reorder_apps(&id, &app_ids)
}

#[tauri::command]
pub async fn start_app(shields: Shields<'_>, id: String, app_id: String) -> Result<String> {
    shields.start_app(&id, &app_id).await
}

#[tauri::command]
pub fn stop_app(shields: Shields<'_>, id: String, app_id: String) -> Result<()> {
    shields.stop_app(&id, &app_id)
}

#[tauri::command]
pub async fn restart_app(shields: Shields<'_>, id: String, app_id: String) -> Result<String> {
    shields.restart_app(&id, &app_id).await
}
