use std::ffi::OsStr;
use std::path::PathBuf;

use serde::Serialize;
use tauri::State;

use crate::editor::{self, Editor, Editors, Launch};
use crate::editor_icons::EditorIcons;
use crate::error::{Error, Result};
use crate::preferences::{EditorPrefs, Preferences};

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

/// Confirms that the path `path` in the output of the live process `process_id`
/// names a Mac file. Resolves to that file.
#[tauri::command]
pub async fn terminal_location(manager: Managed<'_>, process_id: String, path: String) -> Result<PathBuf> {
    Ok(manager.location(&process_id, &path)?.1)
}

/// What a file link opened, so the UI can say when the editor ignored the line.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedLocation {
    pub editor: String,
    pub file: PathBuf,
    pub line_ignored: bool,
}

/// Opens `file` at `line` and `col` in the project's editor, else the app
/// default. `launcher` runs the command.
async fn open_location(
    prefs: &EditorPrefs,
    detected: &[Editor],
    project: Option<&str>,
    file: PathBuf,
    (line, col): (u32, Option<u32>),
    launcher: impl AsyncFnOnce(&Launch, &str) -> Result<()>,
) -> Result<OpenedLocation> {
    if line == 0 || col == Some(0) {
        return Err(Error::LinkZeroPosition);
    }
    let (editor, _) = editor::resolve(prefs, detected, None, project).ok_or(Error::NoEditor)?;
    let launch = editor::location_launch(&editor.id, &editor.path, &file, line, col);
    launcher(&launch, &editor.name).await?;
    Ok(OpenedLocation { editor: editor.name, file, line_ignored: launch.line_ignored })
}

/// Runs `launch` with no shell. A launcher that is not in the bundle is an error, not a fallback.
async fn run(launch: &Launch, name: &str) -> Result<()> {
    if !launch.program.is_file() {
        return Err(Error::EditorLauncherMissing(name.to_owned(), launch.program.display().to_string()));
    }
    let out = tokio::process::Command::new(&launch.program).args(&launch.args).output().await?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::EditorFailed(name.to_owned(), String::from_utf8_lossy(&out.stderr).trim().to_owned()))
}

/// Opens the file that the path `path` in the output of the live process
/// `process_id` names, at `line` and `col`. The backend resolves the file again.
#[tauri::command]
pub async fn open_terminal_location(
    manager: Managed<'_>,
    preferences: State<'_, Preferences>,
    process_id: String,
    path: String,
    line: u32,
    col: Option<u32>,
) -> Result<OpenedLocation> {
    if line == 0 || col == Some(0) {
        return Err(Error::LinkZeroPosition);
    }
    let (session_id, file) = manager.location(&process_id, &path)?;
    let project = manager.session_editor(&session_id)?;
    let detected = tokio::task::spawn_blocking(detected).await.map_err(|e| Error::Io(std::io::Error::other(e)))?;
    open_location(&preferences.get().editors, &detected, project.as_deref(), file, (line, col), run).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::Mutex;

    fn editor(id: &str) -> Editor {
        Editor { id: id.into(), name: id.into(), path: PathBuf::from(format!("/Apps/{id}.app")) }
    }

    fn prefs(enabled: Option<&[&str]>, default: Option<&str>) -> EditorPrefs {
        EditorPrefs {
            enabled: enabled.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect()),
            default: default.map(str::to_owned),
        }
    }

    /// Opens with a launcher that only records the command.
    async fn opened(
        prefs: &EditorPrefs,
        detected: &[Editor],
        project: Option<&str>,
    ) -> (Result<OpenedLocation>, Vec<(PathBuf, Vec<OsString>, String)>) {
        let seen = Mutex::new(Vec::new());
        let launcher = async |launch: &Launch, name: &str| {
            seen.lock().unwrap().push((launch.program.clone(), launch.args.clone(), name.to_owned()));
            Ok(())
        };
        let result = open_location(prefs, detected, project, "/p/a.rs".into(), (3, Some(7)), launcher).await;
        (result, seen.into_inner().unwrap())
    }

    #[tokio::test]
    async fn a_location_opens_in_the_project_editor_else_the_app_default() {
        let all = [editor("zed"), editor("vscode"), editor("fleet")];
        let (result, seen) = opened(&prefs(None, Some("vscode")), &all, None).await;
        assert_eq!(
            result.unwrap(),
            OpenedLocation { editor: "vscode".into(), file: "/p/a.rs".into(), line_ignored: false }
        );
        assert_eq!(
            seen,
            [(
                PathBuf::from("/Apps/vscode.app/Contents/Resources/app/bin/code"),
                vec![OsString::from("--goto"), "/p/a.rs:3:7".into()],
                "vscode".into()
            )]
        );
        let (result, seen) = opened(&prefs(None, Some("vscode")), &all, Some("zed")).await;
        assert_eq!(result.unwrap().editor, "zed");
        assert_eq!(seen[0].0, Path::new("/Apps/zed.app/Contents/MacOS/cli"));
        // An editor with no line protocol opens the file and says so.
        let (result, _) = opened(&prefs(None, None), &all, Some("fleet")).await;
        assert!(result.unwrap().line_ignored);
        // No editor is on: nothing runs.
        let (result, seen) = opened(&prefs(Some(&[]), None), &all, Some("zed")).await;
        assert!(matches!(result, Err(Error::NoEditor)));
        assert!(seen.is_empty());
        let (result, _) = opened(&prefs(None, None), &[], None).await;
        assert!(matches!(result, Err(Error::NoEditor)));
    }

    #[tokio::test]
    async fn a_zero_line_or_column_is_an_error_and_nothing_runs() {
        for position in [(0, None), (0, Some(3)), (3, Some(0))] {
            let ran = Mutex::new(false);
            let launcher = async |_: &Launch, _: &str| {
                *ran.lock().unwrap() = true;
                Ok(())
            };
            let result =
                open_location(&prefs(None, None), &[editor("zed")], None, "/p/a.rs".into(), position, launcher).await;
            assert!(matches!(result, Err(Error::LinkZeroPosition)), "{position:?}");
            assert!(!*ran.lock().unwrap());
        }
    }

    #[tokio::test]
    async fn a_failed_launch_or_a_missing_launcher_is_an_error() {
        let failing = async |_: &Launch, name: &str| Err(Error::EditorFailed(name.to_owned(), "boom".into()));
        let result =
            open_location(&prefs(None, None), &[editor("zed")], None, "/p/a.rs".into(), (1, None), failing).await;
        assert!(matches!(result, Err(Error::EditorFailed(..))));

        let bundle = std::env::temp_dir().join(format!("zwinger-launch-{}", uuid::Uuid::new_v4())).join("Zed.app");
        let launch = editor::location_launch("zed", &bundle, Path::new("/p/a.rs"), 1, None);
        assert!(matches!(run(&launch, "Zed").await, Err(Error::EditorLauncherMissing(..))));
    }
}
