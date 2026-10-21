use tauri::State;
use tauri::ipc::Channel;

use crate::error::Result;
use crate::runtime::{Microsandbox, PtyEvent, SessionKind};
use crate::sessions::{Manager, ShieldView};
use crate::shield::NetworkPolicy;

type Shields<'a> = State<'a, Manager<Microsandbox>>;

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
pub fn add_mount(
    shields: Shields<'_>,
    id: String,
    host: String,
    guest: String,
    readonly: bool,
) -> Result<()> {
    shields.add_mount(&id, &host, &guest, readonly)
}

#[tauri::command]
pub fn remove_mount(shields: Shields<'_>, id: String, guest: String) -> Result<()> {
    shields.remove_mount(&id, &guest)
}

#[tauri::command]
pub fn set_mount_readonly(
    shields: Shields<'_>,
    id: String,
    guest: String,
    readonly: bool,
) -> Result<()> {
    shields.set_mount_readonly(&id, &guest, readonly)
}

#[tauri::command]
pub fn set_network_policy(shields: Shields<'_>, id: String, policy: NetworkPolicy) -> Result<()> {
    shields.set_network(&id, policy)
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
pub async fn open_session(
    shields: Shields<'_>,
    shield_id: String,
    kind: SessionKind,
    rows: u16,
    cols: u16,
    on_event: Channel<PtyEvent>,
) -> Result<String> {
    shields
        .open_session(&shield_id, kind, rows, cols, move |event| {
            // The webview may have closed the channel; the session keeps running.
            let _ = on_event.send(event);
        })
        .await
}

#[tauri::command]
pub fn write_session(shields: Shields<'_>, id: String, data: String) -> Result<()> {
    shields.write(&id, data.into_bytes())
}

#[tauri::command]
pub fn resize_session(shields: Shields<'_>, id: String, rows: u16, cols: u16) -> Result<()> {
    shields.resize(&id, rows, cols)
}

#[tauri::command]
pub fn close_session(shields: Shields<'_>, id: String) -> Result<()> {
    shields.close_session(&id)
}
