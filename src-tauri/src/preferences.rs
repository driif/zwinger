use std::ops::RangeInclusive;
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
    /// The width of the project rail, in CSS pixels.
    pub rail_width: u32,
    pub terminal: TerminalPrefs,
    pub editors: EditorPrefs,
}

/// The editors that the launchers offer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EditorPrefs {
    /// The ids of the editors that are on. `None`: not set, so each detected
    /// editor is on. `Some([])`: the user turned all of them off.
    pub enabled: Option<Vec<String>>,
    /// `None`, or an editor that is off or gone: Zed, else VS Code, else the
    /// first editor that is on.
    pub default: Option<String>,
}

/// The look and keys of all terminals.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalPrefs {
    /// A CSS font-family list.
    pub font_family: String,
    /// In CSS pixels.
    pub font_size: f64,
    pub line_height: f64,
    pub cursor_style: CursorStyle,
    pub cursor_blink: bool,
    /// The space around the text, in CSS pixels.
    pub padding: u32,
    /// The lines that a terminal keeps above the screen.
    pub scrollback: u32,
    /// Option sends Meta, so Option+B and Option+F move by word.
    pub option_as_meta: bool,
    /// The theme of the terminal palette. `None`: the app theme.
    pub theme_id: Option<String>,
}

/// The valid ranges of the numeric terminal prefs. The settings page uses
/// the same limits.
pub const FONT_SIZE: RangeInclusive<f64> = 8.0..=32.0;
pub const LINE_HEIGHT: RangeInclusive<f64> = 1.0..=2.0;
pub const PADDING: RangeInclusive<u32> = 0..=32;
pub const SCROLLBACK: RangeInclusive<u32> = 1000..=100_000;

impl TerminalPrefs {
    /// The prefs with each number in its range. A non-finite number gives the
    /// default: xterm throws for a bad value.
    pub fn clamped(self) -> Self {
        let default = Self::default();
        let clamp = |value: f64, range: RangeInclusive<f64>, default: f64| {
            if value.is_finite() { value.clamp(*range.start(), *range.end()) } else { default }
        };
        Self {
            font_size: clamp(self.font_size, FONT_SIZE, default.font_size),
            line_height: clamp(self.line_height, LINE_HEIGHT, default.line_height),
            padding: self.padding.clamp(*PADDING.start(), *PADDING.end()),
            scrollback: self.scrollback.clamp(*SCROLLBACK.start(), *SCROLLBACK.end()),
            ..self
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

impl Default for TerminalPrefs {
    fn default() -> Self {
        Self {
            font_family: r#""JetBrainsMono Nerd Font", "MesloLGS NF", ui-monospace, SFMono-Regular, Menlo, monospace"#
                .into(),
            font_size: 13.0,
            line_height: 1.2,
            cursor_style: CursorStyle::Bar,
            cursor_blink: true,
            padding: 8,
            scrollback: 10_000,
            option_as_meta: true,
            theme_id: None,
        }
    }
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
            terminal: TerminalPrefs::default(),
            editors: EditorPrefs::default(),
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
        let mut current: Prefs =
            std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        current.terminal = current.terminal.clamped();
        Self { path, current: Mutex::new(current) }
    }

    pub fn get(&self) -> Prefs {
        self.current.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Writes a temp file and renames it, so a crash keeps the old file.
    pub fn set(&self, mut prefs: Prefs) -> Result<()> {
        prefs.terminal = prefs.terminal.clamped();
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
        assert_eq!(prefs.terminal, TerminalPrefs::default());
        assert_eq!(prefs.editors, EditorPrefs { enabled: None, default: None });
    }

    #[test]
    fn unset_editors_differ_from_all_editors_off() {
        let unset: Prefs = serde_json::from_str(r#"{"editors":{}}"#).unwrap();
        assert_eq!(unset.editors.enabled, None);
        let off: Prefs = serde_json::from_str(r#"{"editors":{"enabled":[],"default":"zed"}}"#).unwrap();
        assert_eq!(off.editors, EditorPrefs { enabled: Some(Vec::new()), default: Some("zed".into()) });
        let path = path();
        let prefs = Preferences::load(path.clone());
        prefs.set(off.clone()).unwrap();
        assert_eq!(Preferences::load(path).get().editors, off.editors);
    }

    #[test]
    fn terminal_prefs_load_in_part() {
        let prefs: Prefs = serde_json::from_str(r#"{"terminal":{"fontSize":15,"cursorStyle":"block"}}"#).unwrap();
        assert_eq!(
            prefs.terminal,
            TerminalPrefs { font_size: 15.0, cursor_style: CursorStyle::Block, ..TerminalPrefs::default() }
        );
    }

    #[test]
    fn terminal_numbers_out_of_range_are_clamped_on_load() {
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"terminal":{"fontSize":2,"lineHeight":0.8,"padding":500,"scrollback":0}}"#).unwrap();
        assert_eq!(
            Preferences::load(path).get().terminal,
            TerminalPrefs {
                font_size: 8.0,
                line_height: 1.0,
                padding: 32,
                scrollback: 1000,
                ..TerminalPrefs::default()
            }
        );
    }

    #[test]
    fn terminal_numbers_out_of_range_are_clamped_on_set() {
        let path = path();
        let prefs = Preferences::load(path.clone());
        let terminal =
            TerminalPrefs { font_size: f64::NAN, line_height: 3.0, scrollback: 1_000_000, ..TerminalPrefs::default() };
        prefs.set(Prefs { terminal, ..Prefs::default() }).unwrap();
        let clamped = TerminalPrefs { line_height: 2.0, scrollback: 100_000, ..TerminalPrefs::default() };
        assert_eq!(prefs.get().terminal, clamped);
        assert_eq!(Preferences::load(path).get().terminal, clamped);
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
                "terminal": {
                    "fontFamily": TerminalPrefs::default().font_family,
                    "fontSize": 13.0,
                    "lineHeight": 1.2,
                    "cursorStyle": "bar",
                    "cursorBlink": true,
                    "padding": 8,
                    "scrollback": 10000,
                    "optionAsMeta": true,
                    "themeId": null,
                },
                "editors": { "enabled": null, "default": null },
            })
        );
    }
}
