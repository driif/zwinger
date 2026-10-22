use tauri::State;

use crate::appearance::{self, Theme, ThemeList, Themes};
use crate::error::Result;

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
