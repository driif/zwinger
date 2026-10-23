use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tauri::State;
use tauri::ipc::Channel;
use tokio::task::AbortHandle;

use crate::cage::tasks::RunSummary;
use crate::cage::{self, TaskDraft};
use crate::error::Result;
use crate::task_artifacts::{self, Room};
use crate::task_runs::{Control, LiveRecord, Status};

use super::Projects;

#[tauri::command]
pub fn put_task(projects: Projects<'_>, id: String, task: TaskDraft) -> Result<String> {
    projects.put_task(&id, task)
}

#[tauri::command]
pub fn remove_task(projects: Projects<'_>, id: String, task_id: String) -> Result<()> {
    projects.remove_task(&id, &task_id)
}

// Async, so the run's task starts on the tokio runtime.
#[tauri::command]
pub async fn start_task_run(projects: Projects<'_>, project_id: String, task_id: String) -> Result<String> {
    projects.start_task_run(&project_id, &task_id)
}

#[tauri::command]
pub fn control_task_run(
    projects: Projects<'_>,
    project_id: String,
    task_id: String,
    run: String,
    control: Control,
) -> Result<()> {
    projects.control_task_run(&project_id, &task_id, &run, control)
}

#[tauri::command]
pub async fn list_task_runs(projects: Projects<'_>, project_id: String) -> Result<Vec<RunSummary>> {
    projects.list_task_runs(&project_id).await
}

#[tauri::command]
pub async fn run_diff(projects: Projects<'_>, project_id: String, task_id: String, run: String) -> Result<String> {
    projects.run_diff(&project_id, &task_id, &run).await
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
    Run { project_id: String, task_id: String, run: String },
    Folder { path: String },
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RoomEvent {
    /// The whole room, each time a file of the round changes. `run` is the
    /// state of an in-app run of this app session.
    Room { room: Box<Room>, run: Option<Status> },
    /// A live record of the active stage.
    Live(LiveRecord),
}

/// The rooms that the webview watches, by token.
#[derive(Default)]
pub struct Watches {
    next: AtomicU64,
    tasks: Mutex<HashMap<u64, Vec<AbortHandle>>>,
}

/// Sends the room now and on each change, until `unwatch_room`. Async, so
/// its tasks start on the tokio runtime.
#[tauri::command]
pub async fn watch_room(
    projects: Projects<'_>,
    watches: State<'_, Watches>,
    source: RoomSource,
    on_event: Channel<RoomEvent>,
) -> Result<u64> {
    let (dir, run) = match source {
        RoomSource::Run { project_id, task_id, run } => {
            (projects.round_dir(&project_id, &task_id, &run)?, projects.run_handle(&project_id, &task_id, &run))
        }
        RoomSource::Folder { path } => {
            let dir = cage::host_dir(&path)?;
            if !task_artifacts::is_round(&dir) {
                return Err(crate::error::Error::NotRound(path));
            }
            (dir, None)
        }
    };
    let mut tasks = Vec::new();
    if let Some(run) = &run {
        let (recent, mut rx) = run.subscribe();
        let channel = on_event.clone();
        tasks.push(
            tokio::spawn(async move {
                for record in recent {
                    let _ = channel.send(RoomEvent::Live(record));
                }
                loop {
                    match rx.recv().await {
                        Ok(record) => {
                            let _ = channel.send(RoomEvent::Live(record));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
            .abort_handle(),
        );
    }
    let status = move || run.as_ref().map(|r| r.status());
    tasks.push(
        tokio::spawn(task_artifacts::watch(dir, status, move |room, run| {
            on_event.send(RoomEvent::Room { room: Box::new(room), run: run.clone() }).is_ok()
        }))
        .abort_handle(),
    );
    let token = watches.next.fetch_add(1, Ordering::Relaxed);
    watches.tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(token, tasks);
    Ok(token)
}

#[tauri::command]
pub fn unwatch_room(watches: State<'_, Watches>, token: u64) {
    let tasks = watches.tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&token);
    for task in tasks.into_iter().flatten() {
        task.abort();
    }
}
