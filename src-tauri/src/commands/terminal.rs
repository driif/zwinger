use tauri::ipc::{Channel, InvokeResponseBody};

use crate::cage::PaneNode;
use crate::error::Result;
use crate::runtime::{PtyEvent, SessionKind};

use super::Projects;

#[tauri::command]
pub fn reorder_terminals(projects: Projects<'_>, id: String, terminal_ids: Vec<String>) -> Result<()> {
    projects.reorder_terminals(&id, &terminal_ids)
}

/// Saves the split tree of the project's terminal view. It needs no restart.
#[tauri::command]
pub fn set_pane_layout(projects: Projects<'_>, id: String, layout: Option<PaneNode>) -> Result<()> {
    projects.set_pane_layout(&id, layout)
}

/// Output goes as raw bytes, the exit as `{"exit": code}`.
#[tauri::command]
pub fn attach_session(
    projects: Projects<'_>,
    session_id: String,
    on_event: Channel<InvokeResponseBody>,
) -> Result<u64> {
    projects.attach_session(&session_id, move |event| {
        let body = match event {
            PtyEvent::Output(bytes) => InvokeResponseBody::Raw(bytes),
            PtyEvent::Exit(code) => InvokeResponseBody::Json(format!(r#"{{"exit":{code}}}"#)),
        };
        // The webview may have closed the channel; the session keeps running.
        let _ = on_event.send(body);
    })
}

#[tauri::command]
pub fn detach_session(projects: Projects<'_>, session_id: String, token: u64) {
    projects.detach_session(&session_id, token);
}

#[tauri::command]
pub fn ack_session(projects: Projects<'_>, session_id: String, token: u64) {
    projects.ack_session(&session_id, token);
}

#[tauri::command]
pub async fn open_session(
    projects: Projects<'_>,
    project_id: String,
    kind: SessionKind,
    rows: u16,
    cols: u16,
) -> Result<String> {
    projects.open_session(&project_id, kind, rows, cols).await
}

#[tauri::command]
pub async fn reopen_terminal(
    projects: Projects<'_>,
    project_id: String,
    terminal_id: String,
    rows: u16,
    cols: u16,
) -> Result<String> {
    projects.reopen_terminal(&project_id, &terminal_id, rows, cols).await
}

#[tauri::command]
pub fn write_session(projects: Projects<'_>, id: String, data: String) -> Result<()> {
    projects.write(&id, data.into_bytes())
}

#[tauri::command]
pub fn resize_session(projects: Projects<'_>, id: String, rows: u16, cols: u16) -> Result<()> {
    projects.resize(&id, rows, cols)
}

#[tauri::command]
pub fn close_terminal(projects: Projects<'_>, project_id: String, terminal_id: String) -> Result<()> {
    projects.close_terminal(&project_id, &terminal_id)
}
