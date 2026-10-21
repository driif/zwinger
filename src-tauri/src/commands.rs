use std::collections::BTreeMap;

use tauri::State;
use tauri::ipc::Channel;

use serde_json::Value;

use crate::agent::{AgentCommand, AgentKind, Chat};
use crate::error::Result;
use crate::netlog::NetEvent;
use crate::runtime::{Microsandbox, PtyEvent, SessionKind};
use crate::runtimes::{Progress, RuntimeView, Runtimes};
use crate::secrets::{Secret, SecretValue};
use crate::sessions::{Manager, SecretView, ShieldView};
use crate::shield::{self, AppDraft, NetworkMode, Pos};

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
pub fn attach_app(shields: Shields<'_>, session_id: String, on_event: Channel<PtyEvent>) -> Result<u64> {
    shields.attach_app(&session_id, move |event| {
        // The webview may have closed the channel; the app keeps running.
        let _ = on_event.send(event);
    })
}

#[tauri::command]
pub fn detach_app(shields: Shields<'_>, session_id: String, token: u64) {
    shields.detach_app(&session_id, token);
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
pub async fn put_secret(
    shields: Shields<'_>,
    id: String,
    secret: Secret,
    value: Option<SecretValue>,
) -> Result<()> {
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

#[tauri::command]
pub async fn open_agent(
    shields: Shields<'_>,
    shield_id: String,
    agent: AgentKind,
    on_event: Channel<Value>,
) -> Result<String> {
    shields
        .open_agent(
            &shield_id,
            agent,
            Box::new(move |record| {
                // The webview may have closed the channel; the agent keeps running.
                let _ = on_event.send(record);
            }),
        )
        .await
}

#[tauri::command]
pub async fn agent_call(shields: Shields<'_>, id: String, command: AgentCommand) -> Result<Value> {
    shields.agent_call(&id, command).await
}

#[tauri::command]
pub async fn close_agent(shields: Shields<'_>, id: String) -> Result<()> {
    shields.close_agent(&id).await;
    Ok(())
}

#[tauri::command]
pub async fn list_chats(shields: Shields<'_>, shield_id: String) -> Result<Vec<Chat>> {
    shields.list_chats(&shield_id).await
}

#[tauri::command]
pub async fn delete_chat(shields: Shields<'_>, shield_id: String, agent: AgentKind, chat: String) -> Result<()> {
    shields.delete_chat(&shield_id, agent, &chat).await
}

#[tauri::command]
pub async fn rename_chat(
    shields: Shields<'_>,
    shield_id: String,
    agent: AgentKind,
    chat: String,
    title: String,
) -> Result<()> {
    shields.rename_chat(&shield_id, agent, &chat, &title).await
}

#[tauri::command]
pub fn list_runtimes(runtimes: State<'_, Runtimes>) -> Result<Vec<RuntimeView>> {
    runtimes.list()
}

#[tauri::command]
pub async fn install_runtime(runtimes: State<'_, Runtimes>, name: String, on_progress: Channel<Progress>) -> Result<()> {
    runtimes
        .install(&name, move |progress| {
            // The webview may have closed the channel; the install goes on.
            let _ = on_progress.send(progress);
        })
        .await
}

#[tauri::command]
pub fn cancel_runtime_install(runtimes: State<'_, Runtimes>, name: String) {
    runtimes.cancel(&name);
}
