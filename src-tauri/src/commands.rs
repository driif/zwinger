use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};
use tokio::task::AbortHandle;

use crate::agent::AgentKind;
use crate::error::Result;
use crate::host_agents::HostAgents;
use crate::host_agents::detect::Detection;
use crate::netlog::NetEvent;
use crate::runtime::{Microsandbox, PtyEvent, SessionKind};
use crate::runtimes::{Progress, RuntimeView, Runtimes};
use crate::secrets::{Secret, SecretValue};
use crate::sessions::{Manager, RunSummary, SecretView, ShieldView};
use crate::shield::{self, AppDraft, HostAgentUse, NetworkMode, Pos, TaskDraft};
use crate::task_artifacts::{self, Room};
use crate::task_runs::{Control, LiveRecord, Status};

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
pub fn rename_shield(shields: Shields<'_>, id: String, name: String) -> Result<()> {
    shields.rename(&id, &name)
}

#[tauri::command]
pub fn reorder_shields(shields: Shields<'_>, ids: Vec<String>) -> Result<()> {
    shields.reorder_shields(&ids)
}

#[tauri::command]
pub fn set_layout(shields: Shields<'_>, id: String, layout: Option<BTreeMap<String, Pos>>) -> Result<()> {
    shields.set_layout(&id, layout)
}

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
pub fn resolve_folder(path: String) -> Result<String> {
    shield::resolve_folder(&path)
}

#[tauri::command]
pub fn set_harness(shields: Shields<'_>, id: String, harness: Option<String>) -> Result<()> {
    shields.set_harness(&id, harness.as_deref())
}

#[tauri::command]
pub fn list_folders(shields: Shields<'_>, id: String) -> Result<Vec<String>> {
    shields.list_folders(&id)
}

#[tauri::command]
pub fn add_mount(shields: Shields<'_>, id: String, host: String, guest: String, readonly: bool) -> Result<()> {
    shields.add_mount(&id, &host, &guest, readonly)
}

#[tauri::command]
pub fn remove_mount(shields: Shields<'_>, id: String, guest: String) -> Result<()> {
    shields.remove_mount(&id, &guest)
}

#[tauri::command]
pub fn set_mount_readonly(shields: Shields<'_>, id: String, guest: String, readonly: bool) -> Result<()> {
    shields.set_mount_readonly(&id, &guest, readonly)
}

#[tauri::command]
pub fn set_network_mode(shields: Shields<'_>, id: String, mode: NetworkMode) -> Result<()> {
    shields.set_network_mode(&id, mode)
}

#[tauri::command]
pub fn add_domain(shields: Shields<'_>, id: String, domain: String) -> Result<()> {
    shields.add_domain(&id, &domain)
}

#[tauri::command]
pub fn allow_host(shields: Shields<'_>, shield_id: String, host: String) -> Result<()> {
    shields.allow_host(&shield_id, &host)
}

#[tauri::command]
pub fn wildcard_for(host: String) -> Option<String> {
    shield::wildcard_for(&host)
}

#[tauri::command]
pub fn list_net_events(shields: Shields<'_>, shield_id: String) -> Result<Vec<NetEvent>> {
    shields.list_net_events(&shield_id)
}

#[tauri::command]
pub fn remove_domain(shields: Shields<'_>, id: String, domain: String) -> Result<()> {
    shields.remove_domain(&id, &domain)
}

// The secret commands are async, so the Keychain calls do not block the main thread.

#[tauri::command]
pub async fn put_secret(shields: Shields<'_>, id: String, secret: Secret, value: Option<SecretValue>) -> Result<()> {
    shields.put_secret(&id, secret, value)
}

#[tauri::command]
pub async fn remove_secret(shields: Shields<'_>, id: String, name: String) -> Result<()> {
    shields.remove_secret(&id, &name)
}

#[tauri::command]
pub async fn list_secrets(shields: Shields<'_>, id: String) -> Result<Vec<SecretView>> {
    shields.list_secrets(&id)
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

#[tauri::command]
pub fn list_runtimes(runtimes: State<'_, Runtimes>) -> Result<Vec<RuntimeView>> {
    runtimes.list()
}

/// `version` `None` installs the pinned version. An unpinned version needs
/// `confirm` once.
#[tauri::command]
pub async fn install_runtime(
    runtimes: State<'_, Runtimes>,
    name: String,
    version: Option<String>,
    confirm: bool,
    on_progress: Channel<Progress>,
) -> Result<()> {
    runtimes
        .install(&name, version.as_deref(), confirm, move |progress| {
            // The webview may have closed the channel; the install goes on.
            let _ = on_progress.send(progress);
        })
        .await
}

#[tauri::command]
pub fn cancel_runtime_install(runtimes: State<'_, Runtimes>, name: String) {
    runtimes.cancel(&name);
}

type Host<'a> = State<'a, Arc<HostAgents>>;

/// The last detection of the host agents, else a new one.
#[tauri::command]
pub async fn host_agents(host: Host<'_>) -> Result<Detection> {
    let host = host.inner().clone();
    tokio::task::spawn_blocking(move || host.last()).await.map_err(|e| std::io::Error::other(e).into())
}

/// Looks for the host agents again.
#[tauri::command]
pub async fn detect_host_agents(host: Host<'_>) -> Result<Detection> {
    let host = host.inner().clone();
    tokio::task::spawn_blocking(move || host.detect()).await.map_err(|e| std::io::Error::other(e).into())
}

#[tauri::command]
pub async fn put_host_agent(shields: Shields<'_>, id: String, agent: HostAgentUse, allow_hosts: bool) -> Result<()> {
    shields.put_host_agent(&id, agent, allow_hosts).await
}

#[tauri::command]
pub fn remove_host_agent(shields: Shields<'_>, id: String, agent: AgentKind) -> Result<()> {
    shields.remove_host_agent(&id, agent)
}

#[tauri::command]
pub fn remove_guest_login(shields: Shields<'_>, id: String, agent: AgentKind) -> Result<()> {
    shields.remove_guest_login(&id, agent)
}

#[tauri::command]
pub fn confirm_paths(shields: Shields<'_>, id: String) -> Result<()> {
    shields.confirm_paths(&id)
}

/// Each notice comes once, so a reload does not show it again.
#[tauri::command]
pub fn take_app_notices(shields: Shields<'_>) -> Vec<String> {
    shields.take_app_notices()
}

#[tauri::command]
pub fn set_agents_unprompted(shields: Shields<'_>, id: String, on: bool) -> Result<()> {
    shields.set_agents_unprompted(&id, on)
}

#[tauri::command]
pub fn set_auto_renew(shields: Shields<'_>, id: String, on: bool) -> Result<()> {
    shields.set_auto_renew(&id, on)
}

#[tauri::command]
pub fn put_task(shields: Shields<'_>, id: String, task: TaskDraft) -> Result<String> {
    shields.put_task(&id, task)
}

#[tauri::command]
pub fn remove_task(shields: Shields<'_>, id: String, task_id: String) -> Result<()> {
    shields.remove_task(&id, &task_id)
}

// Async, so the run's task starts on the tokio runtime.
#[tauri::command]
pub async fn start_task_run(shields: Shields<'_>, shield_id: String, task_id: String) -> Result<String> {
    shields.start_task_run(&shield_id, &task_id)
}

#[tauri::command]
pub fn control_task_run(
    shields: Shields<'_>,
    shield_id: String,
    task_id: String,
    run: String,
    control: Control,
) -> Result<()> {
    shields.control_task_run(&shield_id, &task_id, &run, control)
}

#[tauri::command]
pub async fn list_task_runs(shields: Shields<'_>, shield_id: String) -> Result<Vec<RunSummary>> {
    shields.list_task_runs(&shield_id).await
}

#[tauri::command]
pub async fn run_diff(shields: Shields<'_>, shield_id: String, task_id: String, run: String) -> Result<String> {
    shields.run_diff(&shield_id, &task_id, &run).await
}

/// True if the folder looks like a duo round, so a drop opens its room.
#[tauri::command]
pub fn is_round_folder(path: String) -> bool {
    shield::host_dir(&path).is_ok_and(|dir| task_artifacts::is_round(&dir))
}

/// The round that a room shows: a run of a task, or a duo round folder on
/// the host, which the room only reads.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RoomSource {
    Run { shield_id: String, task_id: String, run: String },
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
    shields: Shields<'_>,
    watches: State<'_, Watches>,
    source: RoomSource,
    on_event: Channel<RoomEvent>,
) -> Result<u64> {
    let (dir, run) = match source {
        RoomSource::Run { shield_id, task_id, run } => {
            (shields.round_dir(&shield_id, &task_id, &run)?, shields.run_handle(&shield_id, &task_id, &run))
        }
        RoomSource::Folder { path } => {
            let dir = shield::host_dir(&path)?;
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
