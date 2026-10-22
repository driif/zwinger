use std::ffi::OsStr;
use std::path::PathBuf;

use serde::Serialize;
use tauri::State;

use crate::editor::{self, Editor, Editors};
use crate::error::{Error, Result};
use crate::preferences::Preferences;

use super::Shields;

fn detected() -> Vec<Editor> {
    editor::detect(&editor::app_dirs(&dirs::home_dir().unwrap_or_default()))
}

/// Looks for the editors again. The check reads a few app folders only.
#[tauri::command]
pub fn detect_editors(preferences: State<'_, Preferences>) -> Editors {
    editor::view(&preferences.get().editors, detected())
}

/// What Open in editor did, so the UI can say where it fell back.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Opened {
    pub editor: String,
    pub folder: PathBuf,
    /// The asked editor is off or gone, so the default opened.
    pub editor_fell_back: bool,
    /// The terminal's folder is unknown, so the shield folder opened.
    pub folder_fell_back: bool,
}

/// `/usr/bin/open` with the arguments as a list, so no shell reads them.
async fn open(args: &[&OsStr], name: &str) -> Result<()> {
    let out = tokio::process::Command::new("/usr/bin/open").args(args).output().await?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::EditorFailed(name.to_owned(), String::from_utf8_lossy(&out.stderr).trim().to_owned()))
}

/// Opens the shield folder, or the terminal's current folder, in `editor_id`
/// or the default editor. The folder comes from the backend, never from the UI.
#[tauri::command]
pub async fn open_in_editor(
    shields: Shields<'_>,
    preferences: State<'_, Preferences>,
    shield_id: String,
    editor_id: Option<String>,
    terminal_id: Option<String>,
) -> Result<Opened> {
    let (folder, known) = shields.editor_folder(&shield_id, terminal_id.as_deref())?;
    let (editor, editor_fell_back) =
        editor::resolve(&preferences.get().editors, &detected(), editor_id.as_deref()).ok_or(Error::NoEditor)?;
    open(&["-a".as_ref(), editor.path.as_os_str(), folder.as_os_str()], &editor.name).await?;
    Ok(Opened { editor: editor.name, folder, editor_fell_back, folder_fell_back: !known })
}

/// Shows the shield folder in Finder.
#[tauri::command]
pub async fn reveal_shield(shields: Shields<'_>, shield_id: String) -> Result<()> {
    let (folder, _) = shields.editor_folder(&shield_id, None)?;
    open(&["-R".as_ref(), folder.as_os_str()], "Finder").await
}
