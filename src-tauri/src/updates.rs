use serde::Serialize;
use tauri::{AppHandle, Manager as _};
use tauri_plugin_updater::UpdaterExt as _;

use crate::error::{Error, Result};
use crate::runtime::Microsandbox;
use crate::sessions::Manager;

#[derive(Serialize)]
pub struct UpdateInfo {
    version: String,
    notes: Option<String>,
}

impl From<tauri_plugin_updater::Error> for Error {
    fn from(e: tauri_plugin_updater::Error) -> Self {
        Error::Update(e.to_string())
    }
}

/// The app restarts only when no shield is live: a restart would end its VM
/// and the work of its agents.
pub fn restart_guard(live: usize) -> Result<()> {
    if live > 0 { Err(Error::ShieldsRunning(live)) } else { Ok(()) }
}

fn live(app: &AppHandle) -> usize {
    app.try_state::<Manager<Microsandbox>>().map_or(0, |m| m.live())
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>> {
    let update = app.updater()?.check().await?;
    Ok(update.map(|u| UpdateInfo { version: u.version, notes: u.body }))
}

/// Fails with `ShieldsRunning` while a shield is live, unless `stop_shields`.
/// Then it stops them, and restarts only if all of them stopped. If not, the
/// update applies at the next start.
#[tauri::command]
pub async fn install_update(app: AppHandle, stop_shields: bool) -> Result<()> {
    if !stop_shields {
        restart_guard(live(&app))?;
    }
    let update = app.updater()?.check().await?.ok_or_else(|| Error::Update("no update is available now".into()))?;
    update.download_and_install(|_, _| {}, || {}).await?;
    if stop_shields && let Some(manager) = app.try_state::<Manager<Microsandbox>>() {
        manager.stop_all(crate::STOP_ALL_LIMIT).await;
    }
    restart_guard(live(&app))?;
    app.restart()
}
