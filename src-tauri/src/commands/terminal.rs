use tauri::ipc::{Channel, InvokeResponseBody};

use crate::error::Result;
use crate::runtime::{PtyEvent, TerminalKind};
use crate::session::PaneNode;

use super::Managed;

#[tauri::command]
pub fn reorder_terminals(manager: Managed<'_>, session_id: String, terminal_ids: Vec<String>) -> Result<()> {
    manager.reorder_terminals(&session_id, &terminal_ids)
}

/// Saves the split tree of the session's terminal view. It needs no restart.
#[tauri::command]
pub fn set_pane_layout(manager: Managed<'_>, session_id: String, layout: Option<PaneNode>) -> Result<()> {
    manager.set_pane_layout(&session_id, layout)
}

/// Output goes as raw bytes, the exit as `{"exit": code}`.
#[tauri::command]
pub fn attach_session(manager: Managed<'_>, id: String, on_event: Channel<InvokeResponseBody>) -> Result<u64> {
    manager.attach_session(&id, move |event| {
        let body = match event {
            PtyEvent::Output(bytes) => InvokeResponseBody::Raw(bytes),
            PtyEvent::Exit(code) => InvokeResponseBody::Json(format!(r#"{{"exit":{code}}}"#)),
        };
        // The webview may have closed the channel; the process keeps running.
        let _ = on_event.send(body);
    })
}

#[tauri::command]
pub fn detach_session(manager: Managed<'_>, id: String, token: u64) {
    manager.detach_session(&id, token);
}

#[tauri::command]
pub fn ack_session(manager: Managed<'_>, id: String, token: u64) {
    manager.ack_session(&id, token);
}

#[tauri::command]
pub async fn open_session(
    manager: Managed<'_>,
    session_id: String,
    kind: TerminalKind,
    rows: u16,
    cols: u16,
) -> Result<String> {
    manager.open_session(&session_id, kind, rows, cols).await
}

#[tauri::command]
pub async fn reopen_terminal(
    manager: Managed<'_>,
    session_id: String,
    terminal_id: String,
    rows: u16,
    cols: u16,
) -> Result<String> {
    manager.reopen_terminal(&session_id, &terminal_id, rows, cols).await
}

#[tauri::command]
pub fn write_session(manager: Managed<'_>, id: String, data: String) -> Result<()> {
    manager.write(&id, data.into_bytes())
}

#[tauri::command]
pub fn resize_session(manager: Managed<'_>, id: String, rows: u16, cols: u16) -> Result<()> {
    manager.resize(&id, rows, cols)
}

#[tauri::command]
pub fn close_terminal(manager: Managed<'_>, session_id: String, terminal_id: String) -> Result<()> {
    manager.close_terminal(&session_id, &terminal_id)
}
