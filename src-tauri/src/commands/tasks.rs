use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tauri::State;
use tauri::ipc::Channel;
use tokio::task::AbortHandle;

use crate::a2a::ShareInfo;
use crate::cage;
use crate::cage::tasks::RunSummary;
use crate::error::Result;
use crate::external_live::Tail;
use crate::session::TaskDraft;
use crate::task_artifacts::{self, Room};
use crate::task_runs::{Control, Status};

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

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RoomEvent {
    /// The whole room, each time a file of the round changes. `run` is the
    /// state of an in-app run of this app session.
    Room { generation: u64, room: Box<Room>, run: Option<Status> },
    /// Clears only one participant's temporary live output.
    Reset { generation: u64, sequence: u64, agent: String },
    /// A live record of the active stage.
    Live { generation: u64, sequence: u64, agent: String, stage: String, record: serde_json::Value },
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
    manager: Managed<'_>,
    watches: State<'_, Watches>,
    source: RoomSource,
    on_event: Channel<RoomEvent>,
) -> Result<u64> {
    let token = watches.next.fetch_add(1, Ordering::Relaxed);
    let generation = Arc::new(AtomicU64::new((token + 1) << 32));
    let sequence = Arc::new(AtomicU64::new(0));
    let (dir, run, folder) = match source {
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
    let mut tasks = Vec::new();
    if let Some(run) = &run {
        let (recent, mut rx) = run.subscribe();
        let channel = on_event.clone();
        let live_generation = generation.clone();
        let live_sequence = sequence.clone();
        tasks.push(
            tokio::spawn(async move {
                for record in recent {
                    let _ = channel.send(RoomEvent::Live {
                        generation: live_generation.load(Ordering::SeqCst),
                        sequence: live_sequence.fetch_add(1, Ordering::SeqCst),
                        agent: record.agent,
                        stage: record.stage,
                        record: record.record,
                    });
                }
                loop {
                    match rx.recv().await {
                        Ok(record) => {
                            let _ = channel.send(RoomEvent::Live {
                                generation: live_generation.load(Ordering::SeqCst),
                                sequence: live_sequence.fetch_add(1, Ordering::SeqCst),
                                agent: record.agent,
                                stage: record.stage,
                                record: record.record,
                            });
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
    if folder {
        let (room_tx, room_rx) = tokio::sync::watch::channel::<Option<Room>>(None);
        let closed_tx = room_tx.clone();
        let watch_dir = dir.clone();
        tasks.push(
            tokio::spawn(async move {
                tokio::select! {
                    _ = task_artifacts::watch(watch_dir, status, move |room, _| {
                        room_tx.send_replace(Some(room));
                        !room_tx.is_closed()
                    }) => {},
                    _ = closed_tx.closed() => {},
                }
            })
            .abort_handle(),
        );
        tasks.push(
            tokio::spawn(external_events(dir, room_rx, generation, sequence, move |event| {
                on_event.send(event).is_ok()
            }))
            .abort_handle(),
        );
    } else {
        tasks.push(
            tokio::spawn(task_artifacts::watch(dir, status, move |room, run| {
                on_event
                    .send(RoomEvent::Room {
                        generation: generation.load(Ordering::SeqCst),
                        room: Box::new(room),
                        run: run.clone(),
                    })
                    .is_ok()
            }))
            .abort_handle(),
        );
    }
    watches.tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(token, tasks);
    Ok(token)
}

async fn external_events(
    dir: PathBuf,
    mut room_rx: tokio::sync::watch::Receiver<Option<Room>>,
    generation: Arc<AtomicU64>,
    sequence: Arc<AtomicU64>,
    mut send: impl FnMut(RoomEvent) -> bool,
) {
    let mut tail = Tail::new();
    let mut current: Option<Room> = None;
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        let changed = tokio::select! {
            next = room_rx.changed() => {
                if next.is_err() { break; }
                current = room_rx.borrow_and_update().clone();
                true
            }
            _ = poll.tick() => false,
        };
        let Some(room) = current.as_ref() else { continue };
        let tick = tail.tick(&dir, room);
        if tick.reset {
            generation.fetch_add(1, Ordering::SeqCst);
            sequence.store(0, Ordering::SeqCst);
        }
        let current_generation = generation.load(Ordering::SeqCst);
        if (changed || tick.reset)
            && !send(RoomEvent::Room { generation: current_generation, room: Box::new(room.clone()), run: None })
        {
            break;
        }
        for agent in tick.reset_agents {
            if !send(RoomEvent::Reset {
                generation: current_generation,
                sequence: sequence.fetch_add(1, Ordering::SeqCst),
                agent,
            }) {
                return;
            }
        }
        let Some(stage) = room.stages.iter().find(|s| s.state == "active") else { continue };
        for (agent, record) in tick.records {
            if !send(RoomEvent::Live {
                generation: current_generation,
                sequence: sequence.fetch_add(1, Ordering::SeqCst),
                agent,
                stage: stage.id.clone(),
                record,
            }) {
                return;
            }
        }
    }
}

#[tauri::command]
pub fn unwatch_room(watches: State<'_, Watches>, token: u64) {
    let tasks = watches.tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&token);
    for task in tasks.into_iter().flatten() {
        task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn external_room_watch_delivers_late_file_and_resets_on_stage_change() {
        let dir = std::env::temp_dir().join(format!("zwinger-live-watch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        std::fs::write(dir.join("turns/plan-opus.prompt.md"), "public").unwrap();
        std::fs::write(dir.join("turns/plan-sol.prompt.md"), "public").unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            b"{\"at\":\"2026-01-01T00:00:00Z\",\"kind\":\"stage\",\"detail\":\"plan\"}\n",
        )
        .unwrap();
        let (room_tx, room_rx) = tokio::sync::watch::channel(None);
        let watch = tokio::spawn(task_artifacts::watch(
            dir.clone(),
            || None::<()>,
            move |room, _| {
                room_tx.send_replace(Some(room));
                true
            },
        ));
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let dispatch = tokio::spawn(external_events(
            dir.clone(),
            room_rx,
            Arc::new(AtomicU64::new(1)),
            Arc::new(AtomicU64::new(0)),
            move |event| event_tx.send(event).is_ok(),
        ));
        let first = tokio::time::timeout(std::time::Duration::from_secs(3), event_rx.recv()).await.unwrap().unwrap();
        let first_generation = match first {
            RoomEvent::Room { generation, room, .. } => {
                assert_eq!(room.stages[0].state, "active");
                generation
            }
            _ => panic!("first event was not the room"),
        };
        let line = b"{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Public live text\"}]},\"parent_tool_use_id\":null}\n";
        std::fs::write(dir.join("turns/plan-opus.raw.err"), line).unwrap();
        let live = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = event_rx.recv().await.expect("watch closed before live output");
                if matches!(&event, RoomEvent::Live { .. }) {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        match live {
            RoomEvent::Live { generation, sequence, agent, record, .. } => {
                assert_eq!(generation, first_generation);
                assert_eq!(sequence, 0);
                assert_eq!(agent, "opus");
                assert_eq!(record["message"]["content"][0]["text"], "Public live text");
            }
            _ => unreachable!(),
        }
        let sol = dir.join("turns/plan-sol.raw.err");
        std::fs::write(&sol, "exec\nfirst\n").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if matches!(event_rx.recv().await, Some(RoomEvent::Live { ref agent, .. }) if agent == "sol") {
                    break;
                }
            }
        })
        .await
        .unwrap();
        std::fs::write(&sol, "exec\nsecond\n").unwrap();
        let reset = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = event_rx.recv().await.expect("watch closed before participant reset");
                if matches!(&event, RoomEvent::Reset { agent, .. } if agent == "sol") {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(reset, RoomEvent::Reset { generation, agent, .. } if generation == first_generation && agent == "sol")
        );
        let mut file = std::fs::OpenOptions::new().append(true).open(dir.join("events.jsonl")).unwrap();
        file.write_all(b"{\"at\":\"2026-01-01T00:00:01Z\",\"kind\":\"stage\",\"detail\":\"gates\"}\n").unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = event_rx.recv().await.expect("watch closed before stage change");
                if matches!(&event, RoomEvent::Room { generation, .. } if *generation > first_generation) {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        match next {
            RoomEvent::Room { generation, room, .. } => {
                assert!(generation > first_generation);
                assert_eq!(room.stages.iter().find(|s| s.state == "active").unwrap().id, "gates");
            }
            _ => unreachable!(),
        }
        watch.abort();
        dispatch.abort();
        let _ = watch.await;
        let _ = dispatch.await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
