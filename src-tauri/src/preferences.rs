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
}

impl Default for Prefs {
    fn default() -> Self {
        Self { onboarded: false, auto_update: true }
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
        assert_eq!(Preferences::load(path()).get(), Prefs { onboarded: false, auto_update: true });
    }

    #[test]
    fn the_flag_survives_a_restart() {
        let path = path();
        let prefs = Preferences::load(path.clone());
        prefs.set(Prefs { onboarded: true, auto_update: false }).unwrap();
        assert_eq!(Preferences::load(path.clone()).get(), Prefs { onboarded: true, auto_update: false });
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_bad_file_gives_the_defaults() {
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(Preferences::load(path.clone()).get(), Prefs::default());
        std::fs::write(&path, br#"{"onboarded":true}"#).unwrap();
        assert_eq!(Preferences::load(path).get(), Prefs { onboarded: true, auto_update: true });
    }
}
