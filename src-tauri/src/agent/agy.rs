//! Antigravity CLI (`agy`): its paths in the VM and its first settings. It
//! runs from its downloaded Linux build. It keeps a Google sign-in as a token
//! that it can read in the guest, so zwinger does not reuse the sign-in of
//! the Mac (ADR 0002). The settings select the Gemini API key mode: a
//! `GEMINI_API_KEY` credential gives the guest a placeholder, and the proxy
//! sets the real key.

use std::path::Path;

use crate::host_agents;

/// The installed runtime, mounted read-only.
pub const DIR: &str = "/opt/zwinger/agy";
/// Its `HOME`, so `~/.gemini/antigravity-cli` is in the project's state and never the host's.
pub const HOME: &str = "/opt/zwinger/state/agy";
/// The settings dir below the state dir.
const SETTINGS_DIR: [&str; 3] = ["agy", ".gemini", "antigravity-cli"];
const SETTINGS: &str = "settings.json";
/// The Gemini API key mode. `GEMINI_API_KEY` alone does not select it.
const API_KEY_MODE: &str = "{\n  \"modelProvider\": \"gemini\"\n}\n";

/// Writes the settings of the API key mode in the state dir `state` if it
/// has no settings file. The user's own settings stay as they are.
pub fn seed(state: &Path) -> std::io::Result<()> {
    let dir = host_agents::open_parts(state, &SETTINGS_DIR)?;
    if host_agents::type_at(&dir, SETTINGS)?.is_some() {
        return Ok(());
    }
    host_agents::write_at(&dir, SETTINGS, API_KEY_MODE.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_selects_the_api_key_mode_once() {
        let state = std::env::temp_dir().join(format!("zwinger-agy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state).unwrap();
        seed(&state).unwrap();
        let file = state.join("agy/.gemini/antigravity-cli/settings.json");
        let settings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(settings["modelProvider"], "gemini");
        // The user's own settings stay.
        std::fs::write(&file, "{\"model\":\"x\"}").unwrap();
        seed(&state).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"model\":\"x\"}");
        let _ = std::fs::remove_dir_all(state);
    }
}
