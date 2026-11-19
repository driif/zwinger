use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt as _;

use crate::appearance::{self, Imported, Theme, ThemeList, Themes};
use crate::error::Result;
use crate::preferences::Preferences;

#[tauri::command]
pub fn list_themes(themes: State<'_, Themes>) -> Result<ThemeList> {
    Ok(appearance::list(&themes.dir))
}

/// Adds the theme, or replaces the theme with the same id.
#[tauri::command]
pub fn save_theme(themes: State<'_, Themes>, theme: Theme) -> Result<()> {
    appearance::save(&themes.dir, &theme)
}

/// A missing theme is not an error.
#[tauri::command]
pub fn delete_theme(themes: State<'_, Themes>, id: String) -> Result<()> {
    appearance::delete(&themes.dir, &id)
}

/// Asks for a theme file and adds it as a new theme. `None`: the user cancelled.
#[tauri::command]
pub async fn import_theme(app: AppHandle) -> Result<Option<Imported>> {
    let dir = app.state::<Themes>().dir.clone();
    tokio::task::spawn_blocking(move || {
        let Some(file) = app.dialog().file().add_filter("Theme", &["json"]).blocking_pick_file() else {
            return Ok(None);
        };
        let src = file.into_path().map_err(std::io::Error::other)?;
        let prefs = app.state::<Preferences>().get();
        let mut selected = vec![prefs.theme.as_str(), prefs.light_theme.as_str(), prefs.dark_theme.as_str()];
        selected.extend(prefs.terminal.theme_id.as_deref());
        appearance::import(&dir, &src, &selected).map(Some)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Asks for a place and writes the theme file there. `false`: the user cancelled.
#[tauri::command]
pub async fn export_theme(app: AppHandle, id: String) -> Result<bool> {
    let dir = app.state::<Themes>().dir.clone();
    tokio::task::spawn_blocking(move || {
        let Some(file) =
            app.dialog().file().add_filter("Theme", &["json"]).set_file_name(format!("{id}.json")).blocking_save_file()
        else {
            return Ok(false);
        };
        let dest = file.into_path().map_err(std::io::Error::other)?;
        appearance::export(&dir, &id, &dest).map(|()| true)
    })
    .await
    .map_err(std::io::Error::other)?
}
