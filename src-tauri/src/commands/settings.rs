use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager as _, State};

use crate::bootstrap::{self, Bootstrap, Legacy, Startup};
use crate::error::Result;
use crate::host_agents::HostAgents;
use crate::host_agents::detect::Detection;
use crate::preferences::{Preferences, Prefs};
use crate::runtimes::{Progress, RuntimeView, Runtimes};

#[tauri::command]
pub fn startup_status(bootstrap: State<'_, Bootstrap>) -> Startup {
    bootstrap.get()
}

#[tauri::command]
pub fn get_preferences(preferences: State<'_, Preferences>) -> Prefs {
    preferences.get()
}

#[tauri::command]
pub fn set_preferences(preferences: State<'_, Preferences>, prefs: Prefs) -> Result<()> {
    preferences.set(prefs)
}

/// Sizes in bytes. `None`: unknown.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageView {
    app_data: PathBuf,
    images: Option<u64>,
    runtimes: Option<u64>,
}

/// Does not follow links. `None` if a part cannot be read.
fn dir_size(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() {
        return Some(meta.len());
    }
    std::fs::read_dir(path).ok()?.map(|entry| dir_size(&entry.ok()?.path())).sum()
}

#[tauri::command]
pub async fn storage(app: AppHandle) -> Result<StorageView> {
    let app_data = app.path().app_data_dir().map_err(|e| std::io::Error::other(e.to_string()))?;
    let images = microsandbox::Storage::usage().await?.images.logical_bytes;
    let dir = app_data.join("runtimes");
    let runtimes = tokio::task::spawn_blocking(move || dir_size(&dir)).await.ok().flatten();
    Ok(StorageView { app_data, images, runtimes })
}

/// Removes the cached images that no shield uses. Resolves to the freed bytes, if known.
#[tauri::command]
pub async fn clean_images() -> Result<Option<u64>> {
    Ok(microsandbox::Image::prune().await?.bytes_reclaimed)
}

/// A microsandbox home of an earlier zwinger build. `size` in bytes, `None`: unknown.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyView {
    legacy: Legacy,
    path: PathBuf,
    size: Option<u64>,
}

fn legacy_homes(app: &AppHandle) -> Result<Vec<(Legacy, PathBuf)>> {
    let data = app.path().app_data_dir().map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(bootstrap::legacy_homes(&data, &dirs::home_dir().unwrap_or_default()))
}

#[tauri::command]
pub async fn legacy_storage(app: AppHandle) -> Result<Vec<LegacyView>> {
    let homes = legacy_homes(&app)?;
    let views = tokio::task::spawn_blocking(move || {
        homes.into_iter().map(|(legacy, path)| LegacyView { legacy, size: dir_size(&path), path }).collect()
    });
    Ok(views.await.unwrap_or_default())
}

/// Removes zwinger's VMs and image from a legacy home. Deletes the home only if zwinger owns it.
#[tauri::command]
pub async fn remove_legacy_runtime(app: AppHandle, legacy: Legacy) -> Result<()> {
    match legacy_homes(&app)?.into_iter().find(|(l, _)| *l == legacy) {
        Some((legacy, path)) => bootstrap::remove_legacy(legacy, &path, crate::REAP_LIMIT).await,
        None => Ok(()),
    }
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
