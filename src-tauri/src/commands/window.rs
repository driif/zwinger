use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager as _};
use tauri::window::Color;

use crate::instance::InstanceLock;

#[derive(Default)]
pub(crate) struct WindowReveal {
    revealed: AtomicBool,
    gate: Mutex<()>,
}

impl WindowReveal {
    pub(crate) fn is_revealed(&self) -> bool {
        self.revealed.load(Ordering::Acquire)
    }
}

#[derive(Debug, serde::Serialize, thiserror::Error)]
#[error("{message}")]
pub(crate) struct RevealError {
    message: String,
}

fn parse_color(value: &str) -> Result<Color, RevealError> {
    let bytes = value.strip_prefix('#').filter(|hex| hex.len() == 6)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .ok_or_else(|| RevealError { message: "invalid window background color".into() })?;
    Ok(Color((bytes >> 16) as u8, (bytes >> 8) as u8, bytes as u8, 255))
}

#[tauri::command]
pub(crate) fn reveal_main_window(app: AppHandle, background_color: String) -> Result<(), RevealError> {
    if app.try_state::<InstanceLock>().is_none() {
        return Ok(());
    }
    let state = app.state::<WindowReveal>();
    let _gate = state.gate.lock().map_err(|e| RevealError { message: e.to_string() })?;
    if state.is_revealed() {
        return Ok(());
    }
    let color = parse_color(&background_color)?;
    let window = app.get_webview_window("main")
        .ok_or_else(|| RevealError { message: "main window is unavailable".into() })?;
    window.set_background_color(Some(color)).map_err(|e| RevealError { message: e.to_string() })?;
    window.show().map_err(|e| RevealError { message: e.to_string() })?;
    window.set_focus().map_err(|e| RevealError { message: e.to_string() })?;
    state.revealed.store(true, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_color_accepts_only_opaque_six_digit_hex() {
        assert!(parse_color("#21c881").is_ok());
        assert!(parse_color("#fff").is_err());
        assert!(parse_color("rgba(0,0,0,0)").is_err());
    }
}
