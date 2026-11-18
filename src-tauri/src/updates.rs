use std::process::{Command, Stdio};

use serde::Serialize;
use tauri::{AppHandle, Manager as _};
use tauri_plugin_updater::UpdaterExt as _;

use crate::cage::manager::Manager;
use crate::error::{Error, Result};
use crate::instance::{self, InstanceLock};
use crate::runtime::Microsandbox;

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

/// The app restarts only when no cage is live: a restart would end its VM
/// and the work of its agents.
pub fn restart_guard(live: usize) -> Result<()> {
    if live > 0 { Err(Error::CagesRunning(live)) } else { Ok(()) }
}

fn live(app: &AppHandle) -> usize {
    app.try_state::<Manager<Microsandbox>>().map_or(0, |m| m.live())
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>> {
    let update = app.updater()?.check().await?;
    Ok(update.map(|u| UpdateInfo { version: u.version, notes: u.body }))
}

/// Fails with `CagesRunning` while a cage is live, unless `stop_cages`.
/// Then it stops them, and restarts only if all of them stopped. If not, the
/// update applies at the next start.
#[tauri::command]
pub async fn install_update(app: AppHandle, stop_cages: bool) -> Result<()> {
    if !stop_cages {
        restart_guard(live(&app))?;
    }
    let update = app.updater()?.check().await?.ok_or_else(|| Error::Update("no update is available now".into()))?;
    update.download_and_install(|_, _| {}, || {}).await?;
    if stop_cages && let Some(manager) = app.try_state::<Manager<Microsandbox>>() {
        manager.stop_all(crate::STOP_ALL_LIMIT).await;
    }
    restart_guard(live(&app))?;
    let exe = std::env::current_exe().map_err(|e| Error::Update(format!("could not find the updated app: {e}")))?;
    let bundle = instance::bundle_path(&exe).map_err(|e| Error::Update(e.to_string()))?;
    let helper_exe = instance::bundle_executable(bundle).unwrap_or_else(|_| exe.clone());
    let mut helper = Command::new(&helper_exe)
        .arg(instance::UPDATE_HELPER_ARG)
        .arg(app.state::<InstanceLock>().path())
        .arg(bundle)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Update(format!("could not start the update helper: {e}")))?;
    let Some(writer) = helper.stdin.take() else {
        let _ = helper.kill();
        return Err(Error::Update("the update helper has no handoff pipe".into()));
    };
    // The helper sees EOF only when this process exits, after its cleanup.
    std::mem::forget(writer);
    app.exit(0);
    Ok(())
}
