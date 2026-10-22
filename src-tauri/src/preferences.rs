use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// The app-wide choices of the user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Prefs {
    /// The user finished or skipped the first-run steps.
    pub onboarded: bool,
    /// Look for an update at each start.
    pub auto_update: bool,
    /// The theme id when the app does not follow the system.
    pub theme: String,
    /// Use `light_theme` or `dark_theme` as the system appearance changes.
    pub follow_system: bool,
    pub light_theme: String,
    pub dark_theme: String,
    /// The width of the shield rail, in CSS pixels.
    pub rail_width: u32,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            onboarded: false,
            auto_update: true,
            theme: "zwinger-dark".into(),
            follow_system: false,
            light_theme: "zwinger-light".into(),
            dark_theme: "zwinger-dark".into(),
            rail_width: 256,
        }
    }
}

pub struct Preferences {
    path: PathBuf,
    current: Mutex<Prefs>,
}

impl Preferences {
    /// A missing or unreadable file gives the defaults, so a bad file cannot
    /// block the start.
    pub fn load(path: PathBuf) -> Self {
        let current =
            std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        Self { path, current: Mutex::new(current) }
    }

    pub fn get(&self) -> Prefs {
        self.current.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Writes a temp file and renames it, so a crash keeps the old file.
    pub fn set(&self, prefs: Prefs) -> Result<()> {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&prefs)?)?;
        std::fs::rename(&tmp, &self.path)?;
        *current = prefs;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        std::env::temp_dir().join(format!("zwinger-prefs-{}", uuid::Uuid::new_v4())).join("preferences.json")
    }

    #[test]
    fn a_new_install_is_not_onboarded_and_checks_for_updates() {
        assert_eq!(Preferences::load(path()).get(), Prefs { onboarded: false, auto_update: true, ..Prefs::default() });
    }

    #[test]
    fn the_flag_survives_a_restart() {
        let path = path();
        let prefs = Preferences::load(path.clone());
        let changed = Prefs {
            onboarded: true,
            auto_update: false,
            theme: "my-theme".into(),
            follow_system: true,
            rail_width: 300,
            ..Prefs::default()
        };
        prefs.set(changed.clone()).unwrap();
        assert_eq!(Preferences::load(path.clone()).get(), changed);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_bad_file_gives_the_defaults() {
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(Preferences::load(path.clone()).get(), Prefs::default());
        std::fs::write(&path, br#"{"onboarded":true}"#).unwrap();
        assert_eq!(Preferences::load(path).get(), Prefs { onboarded: true, auto_update: true, ..Prefs::default() });
    }

    #[test]
    fn an_old_file_gets_the_new_defaults() {
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"onboarded":true,"autoUpdate":false}"#).unwrap();
        let prefs = Preferences::load(path).get();
        assert_eq!(prefs, Prefs { onboarded: true, auto_update: false, ..Prefs::default() });
        assert_eq!(
            (prefs.theme.as_str(), prefs.follow_system, prefs.light_theme.as_str(), prefs.dark_theme.as_str()),
            ("zwinger-dark", false, "zwinger-light", "zwinger-dark")
        );
        assert_eq!(prefs.rail_width, 256);
    }

    #[test]
    fn the_new_fields_have_camel_case_names() {
        let json = serde_json::to_value(Prefs::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "onboarded": false,
                "autoUpdate": true,
                "theme": "zwinger-dark",
                "followSystem": false,
                "lightTheme": "zwinger-light",
                "darkTheme": "zwinger-dark",
                "railWidth": 256,
            })
        );
    }
}
