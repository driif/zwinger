use tauri::ipc::{Channel, InvokeResponseBody};

use crate::error::Result;
use crate::runtime::{PtyEvent, SessionKind};

use super::Shields;

#[tauri::command]
pub fn reorder_terminals(shields: Shields<'_>, id: String, terminal_ids: Vec<String>) -> Result<()> {
    shields.reorder_terminals(&id, &terminal_ids)
}

/// Output goes as raw bytes, the exit as `{"exit": code}`.
#[tauri::command]
pub fn attach_session(shields: Shields<'_>, session_id: String, on_event: Channel<InvokeResponseBody>) -> Result<u64> {
    shields.attach_session(&session_id, move |event| {
        let body = match event {
            PtyEvent::Output(bytes) => InvokeResponseBody::Raw(bytes),
            PtyEvent::Exit(code) => InvokeResponseBody::Json(format!(r#"{{"exit":{code}}}"#)),
        };
        // The webview may have closed the channel; the session keeps running.
        let _ = on_event.send(body);
    })
}

#[tauri::command]
pub fn detach_session(shields: Shields<'_>, session_id: String, token: u64) {
    shields.detach_session(&session_id, token);
}

#[tauri::command]
pub fn ack_session(shields: Shields<'_>, session_id: String, token: u64) {
    shields.ack_session(&session_id, token);
}

#[tauri::command]
pub async fn open_session(
    shields: Shields<'_>,
    shield_id: String,
    kind: SessionKind,
    rows: u16,
    cols: u16,
) -> Result<String> {
    shields.open_session(&shield_id, kind, rows, cols).await
}

#[tauri::command]
pub async fn reopen_terminal(
    shields: Shields<'_>,
    shield_id: String,
    terminal_id: String,
    rows: u16,
    cols: u16,
) -> Result<String> {
    shields.reopen_terminal(&shield_id, &terminal_id, rows, cols).await
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
pub fn close_terminal(shields: Shields<'_>, shield_id: String, terminal_id: String) -> Result<()> {
    shields.close_terminal(&shield_id, &terminal_id)
}
