use serde::Deserialize;
use tauri::ipc::Channel;
use tauri::{State, Webview};

use crate::a2a::ShareInfo;
use crate::cage;
use crate::cage::tasks::RunSummary;
use crate::error::Result;
use crate::room_watch::{RoomEvent, Watches};
use crate::run_git::{Operation, Request, Review};
use crate::session::TaskDraft;
use crate::task_artifacts;
use crate::task_runs::Control;

use super::Managed;

#[tauri::command]
pub fn put_task(manager: Managed<'_>, session_id: String, task: TaskDraft) -> Result<String> {
    manager.put_task(&session_id, task)
}

#[tauri::command]
pub fn remove_task(manager: Managed<'_>, session_id: String, task_id: String) -> Result<()> {
    manager.remove_task(&session_id, &task_id)
}

// Async, so the run's task starts on the tokio runtime.
#[tauri::command]
pub async fn start_task_run(manager: Managed<'_>, session_id: String, task_id: String) -> Result<String> {
    manager.start_task_run(&session_id, &task_id)
}

/// Continues an ended run in a new run. Resolves to the new run name.
/// Async, so the run's task starts on the tokio runtime.
#[tauri::command]
pub async fn resume_task_run(manager: Managed<'_>, session_id: String, task_id: String, run: String) -> Result<String> {
    manager.resume_task_run(&session_id, &task_id, &run)
}

#[tauri::command]
pub fn control_task_run(
    manager: Managed<'_>,
    session_id: String,
    task_id: String,
    run: String,
    control: Control,
) -> Result<()> {
    manager.control_task_run(&session_id, &task_id, &run, control)
}

#[tauri::command]
pub async fn list_task_runs(manager: Managed<'_>, session_id: String) -> Result<Vec<RunSummary>> {
    manager.list_task_runs(&session_id).await
}

#[tauri::command]
pub async fn run_diff(manager: Managed<'_>, session_id: String, task_id: String, run: String) -> Result<String> {
    manager.run_diff(&session_id, &task_id, &run).await
}

/// Reviews the change of a shipped run for a commit or a PR. A review that
/// can be approved has a token, which `apply_run_git` takes.
#[tauri::command]
pub async fn review_run_git(
    manager: Managed<'_>,
    session_id: String,
    task_id: String,
    run: String,
    request: Request,
) -> Result<Review> {
    manager.review_run_git(&session_id, &task_id, &run, request).await
}

/// Runs the operation that the user approved with `token`, or returns its recorded state.
#[tauri::command]
pub async fn apply_run_git(
    manager: Managed<'_>,
    session_id: String,
    task_id: String,
    run: String,
    request: Request,
    token: String,
) -> Result<Operation> {
    manager.apply_run_git(&session_id, &task_id, &run, request, &token).await
}

#[tauri::command]
pub async fn run_git_operation(
    manager: Managed<'_>,
    session_id: String,
    task_id: String,
    run: String,
    token: String,
) -> Result<Option<Operation>> {
    manager.run_git_operation(&session_id, &task_id, &run, &token).await
}

/// The unfinished Git operations of the run: a commit whose push or PR did
/// not finish, or a result that is not known.
#[tauri::command]
pub async fn run_git_unfinished(
    manager: Managed<'_>,
    session_id: String,
    task_id: String,
    run: String,
) -> Result<Vec<Operation>> {
    manager.run_git_unfinished(&session_id, &task_id, &run).await
}

/// Starts an A2A share of the run, or returns the share that runs. Async, so
/// its listener starts on the tokio runtime.
#[tauri::command]
pub async fn share_task_run(
    manager: Managed<'_>,
    session_id: String,
    task_id: String,
    run: String,
) -> Result<ShareInfo> {
    manager.share_task_run(&session_id, &task_id, &run)
}

#[tauri::command]
pub fn task_run_share(manager: Managed<'_>, session_id: String, task_id: String, run: String) -> Option<ShareInfo> {
    manager.shares.get(&session_id, &task_id, &run)
}

/// Ends the share: its credential and its port stop at once.
#[tauri::command]
pub fn stop_task_run_share(manager: Managed<'_>, session_id: String, task_id: String, run: String) {
    manager.shares.stop(&session_id, &task_id, &run);
}

/// True if the folder looks like a duo round, so a drop opens its room.
#[tauri::command]
pub fn is_round_folder(path: String) -> bool {
    cage::host_dir(&path).is_ok_and(|dir| task_artifacts::is_round(&dir))
}

/// The round that a room shows: a run of a task, or a duo round folder on
/// the host, which the room only reads.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RoomSource {
    Run { session_id: String, task_id: String, run: String },
    Folder { path: String },
}

/// Sends the room now and on each change, until `unwatch_room`, a new page
/// in the webview, or its close. Async, so its tasks start on the tokio runtime.
#[tauri::command]
pub async fn watch_room(
    manager: Managed<'_>,
    watches: State<'_, Watches>,
    webview: Webview,
    source: RoomSource,
    on_event: Channel<RoomEvent>,
) -> Result<u64> {
    let (dir, run, external) = match source {
        RoomSource::Run { session_id, task_id, run } => {
            (manager.round_dir(&session_id, &task_id, &run)?, manager.run_handle(&session_id, &task_id, &run), false)
        }
        RoomSource::Folder { path } => {
            let dir = cage::host_dir(&path)?;
            if !task_artifacts::is_round(&dir) {
                return Err(crate::error::Error::NotRound(path));
            }
            (dir, None, true)
        }
    };
    Ok(watches.watch(webview.label(), dir, run, external, move |event| on_event.send(event).is_ok()))
}

#[tauri::command]
pub fn unwatch_room(watches: State<'_, Watches>, token: u64) {
    watches.retire(token);
}
