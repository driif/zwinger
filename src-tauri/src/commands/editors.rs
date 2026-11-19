use std::ffi::OsStr;
use std::path::PathBuf;

use serde::Serialize;
use tauri::State;

use crate::editor::{self, Editor, Editors};
use crate::editor_icons::EditorIcons;
use crate::error::{Error, Result};
use crate::preferences::Preferences;

use super::Managed;

fn detected() -> Vec<Editor> {
    editor::detect(&editor::app_dirs(&dirs::home_dir().unwrap_or_default()))
}

/// Looks for the editors again. The check reads a few app folders, and converts an
/// app icon only when it is new or changed.
#[tauri::command]
pub async fn detect_editors(preferences: State<'_, Preferences>, icons: State<'_, EditorIcons>) -> Result<Editors> {
    let found = tokio::task::spawn_blocking(detected).await.map_err(|e| Error::Io(std::io::Error::other(e)))?;
    let mut view = editor::view(&preferences.get().editors, found);
    for editor in &mut view.editors {
        editor.icon = icons.icon(&editor.editor.path).await;
    }
    Ok(view)
}

/// What Open in editor did, so the UI can say where it fell back.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Opened {
    pub editor: String,
    pub folder: PathBuf,
    /// The asked editor is off or gone, so the project or app default opened.
    pub editor_fell_back: bool,
    /// The terminal's folder is unknown, so the project folder opened.
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

/// Opens the session folder, or the terminal's current folder, in `editor_id`,
/// else the session's editor, else the default editor. The folder comes from the backend, never from the UI.
#[tauri::command]
pub async fn open_in_editor(
    manager: Managed<'_>,
    preferences: State<'_, Preferences>,
    session_id: String,
    editor_id: Option<String>,
    terminal_id: Option<String>,
) -> Result<Opened> {
    let (folder, known) = manager.editor_folder(&session_id, terminal_id.as_deref())?;
    let project = manager.session_editor(&session_id)?;
    let (editor, editor_fell_back) =
        editor::resolve(&preferences.get().editors, &detected(), editor_id.as_deref(), project.as_deref())
            .ok_or(Error::NoEditor)?;
    open(&["-a".as_ref(), editor.path.as_os_str(), folder.as_os_str()], &editor.name).await?;
    Ok(Opened { editor: editor.name, folder, editor_fell_back, folder_fell_back: !known })
}

/// Shows the session folder in Finder.
#[tauri::command]
pub async fn reveal_session(manager: Managed<'_>, session_id: String) -> Result<()> {
    let (folder, _) = manager.editor_folder(&session_id, None)?;
    open(&["-R".as_ref(), folder.as_os_str()], "Finder").await
}
