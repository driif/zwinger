//! The zoom of the whole app, like the zoom of a browser.

use serde::Deserialize;
use tauri::{AppHandle, Emitter as _, Manager as _};

use crate::error::Result;
use crate::preferences::Preferences;

/// The zoom levels, from smallest to largest. 1.0 is the actual size.
const LEVELS: [f64; 11] = [0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

pub const MENU_IN: &str = "zoom-in";
pub const MENU_OUT: &str = "zoom-out";
pub const MENU_RESET: &str = "zoom-reset";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    In,
    Out,
    Reset,
}

/// A zoom level in range. A bad value gives the actual size.
pub fn clamped(level: f64) -> f64 {
    if level.is_finite() { level.clamp(LEVELS[0], LEVELS[LEVELS.len() - 1]) } else { 1.0 }
}

/// The level after one step. A level between two levels goes to the next one.
pub fn next(level: f64, step: Step) -> f64 {
    let level = clamped(level);
    match step {
        Step::In => LEVELS.into_iter().find(|&l| l > level + 0.001).unwrap_or(LEVELS[LEVELS.len() - 1]),
        Step::Out => LEVELS.into_iter().rev().find(|&l| l < level - 0.001).unwrap_or(LEVELS[0]),
        Step::Reset => 1.0,
    }
}

/// Sets the zoom of the main window.
pub fn apply(app: &AppHandle, level: f64) -> Result<()> {
    if let Some(window) = app.get_webview_window("main") {
        window.set_zoom(level).map_err(std::io::Error::other)?;
    }
    Ok(())
}

/// Zooms the app one step, saves the level and tells the UI, so the
/// terminals fit again.
pub fn zoom(app: &AppHandle, step: Step) -> Result<()> {
    let preferences = app.state::<Preferences>();
    let level = next(preferences.get().zoom, step);
    preferences.set_zoom(level)?;
    apply(app, level)?;
    let _ = app.emit("zoom-changed", level);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_go_through_the_levels_and_stop_at_the_ends() {
        assert_eq!(next(1.0, Step::In), 1.1);
        assert_eq!(next(1.0, Step::Out), 0.9);
        assert_eq!(next(2.0, Step::In), 2.0);
        assert_eq!(next(0.5, Step::Out), 0.5);
        assert_eq!(next(1.75, Step::Reset), 1.0);
    }

    #[test]
    fn a_level_between_two_levels_goes_to_the_next_one() {
        assert_eq!(next(1.2, Step::In), 1.25);
        assert_eq!(next(1.2, Step::Out), 1.1);
    }

    #[test]
    fn bad_levels_are_clamped() {
        assert_eq!(clamped(f64::NAN), 1.0);
        assert_eq!(clamped(9.0), 2.0);
        assert_eq!(clamped(0.1), 0.5);
        assert_eq!(next(f64::INFINITY, Step::In), 1.1);
    }
}
