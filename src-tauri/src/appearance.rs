//! User themes: one JSON file per theme in `<app data>/themes/<id>.json`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const ID_LIMIT: usize = 64;
const NAME_LIMIT: usize = 64;
const TOKEN_COUNT: usize = 128;
const KEY_LIMIT: usize = 64;
const VALUE_LIMIT: usize = 128;
const ANSI_COUNT: usize = 16;
/// A theme file larger than this is skipped, not read.
const FILE_LIMIT: u64 = 64 << 10;
const KEY_PREFIXES: [&str; 2] = ["--motiv-", "--zw-"];

/// The folder of the user themes.
pub struct Themes {
    pub dir: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Base {
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalColors {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub selection: String,
    /// The 16 ANSI colors, black to bright white.
    pub ansi: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    pub id: String,
    pub name: String,
    /// The built-in theme that gives the tokens that this theme does not set.
    pub base: Base,
    /// CSS custom properties, such as `--motiv-bg`, and their values.
    pub tokens: BTreeMap<String, String>,
    pub terminal: TerminalColors,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeList {
    /// Sorted by name.
    pub themes: Vec<Theme>,
    /// The file names of the theme files that are not valid.
    pub skipped: Vec<String>,
}

fn invalid(why: String) -> Error {
    Error::InvalidTheme(why)
}

/// A lowercase slug: `a-z`, `0-9` and `-`, not first, at most 64 characters.
pub fn check_id(id: &str) -> Result<()> {
    let mut chars = id.chars();
    let ok = id.len() <= ID_LIMIT
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok { Ok(()) } else { Err(invalid(format!("the id must be a lowercase slug of 1 to 64 characters: {id}"))) }
}

fn check_key(key: &str) -> Result<()> {
    let rest = KEY_PREFIXES.iter().find_map(|p| key.strip_prefix(p));
    let ok = key.len() <= KEY_LIMIT
        && rest.is_some_and(|r| {
            !r.is_empty() && r.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "a token must start with --motiv- or --zw-, use only a-z, 0-9 and -, and have at most 64 characters: {key}"
        )))
    }
}

/// A CSS value that cannot end its declaration or load a resource.
fn check_value(field: &str, value: &str) -> Result<()> {
    let len = value.chars().count();
    if len == 0 || len > VALUE_LIMIT {
        return Err(invalid(format!("the value of {field} must have 1 to 128 characters")));
    }
    if value.chars().any(|c| c.is_control() || matches!(c, ';' | '{' | '}' | '\\')) {
        return Err(invalid(format!("the value of {field} must not contain control characters, ;, {{, }} or \\")));
    }
    if value.to_ascii_lowercase().contains("url(") {
        return Err(invalid(format!("the value of {field} must not contain url(")));
    }
    Ok(())
}

/// Checks all fields. The webview and the theme files are untrusted.
pub fn validate(theme: &Theme) -> Result<()> {
    check_id(&theme.id)?;
    let name = theme.name.trim();
    let len = name.chars().count();
    if len == 0 || len > NAME_LIMIT || name.chars().any(char::is_control) {
        return Err(invalid("the name must have 1 to 64 characters and no control characters".into()));
    }
    if theme.tokens.len() > TOKEN_COUNT {
        return Err(invalid(format!("a theme can have at most {TOKEN_COUNT} tokens")));
    }
    for (key, value) in &theme.tokens {
        check_key(key)?;
        check_value(key, value)?;
    }
    let terminal = &theme.terminal;
    for (field, value) in [
        ("background", &terminal.background),
        ("foreground", &terminal.foreground),
        ("cursor", &terminal.cursor),
        ("selection", &terminal.selection),
    ] {
        check_value(&format!("terminal {field}"), value)?;
    }
    if terminal.ansi.len() != ANSI_COUNT {
        return Err(invalid(format!("the terminal needs exactly {ANSI_COUNT} ANSI colors")));
    }
    for (i, value) in terminal.ansi.iter().enumerate() {
        check_value(&format!("ANSI color {i}"), value)?;
    }
    Ok(())
}

fn path_of(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Reads a regular file and follows no link.
fn read(path: &Path) -> Option<Theme> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > FILE_LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT).read_to_end(&mut bytes).ok()?;
    let theme: Theme = serde_json::from_slice(&bytes).ok()?;
    validate(&theme).ok().map(|()| theme)
}

/// The valid themes of `dir`. A missing dir has none. A `.json` entry that
/// is not a regular file, does not parse, is not valid, or has an id that
/// is not its file name goes to `skipped`.
pub fn list(dir: &Path) -> ThemeList {
    let mut list = ThemeList::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return list;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let path = entry.path();
        let regular = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file());
        match read(&path).filter(|t| regular && t.id == stem) {
            Some(theme) => list.themes.push(theme),
            None => list.skipped.push(name),
        }
    }
    list.themes.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    list.skipped.sort();
    list
}

/// Writes a new temp file and renames it onto `<id>.json`. The rename
/// replaces a link at that path and does not follow it.
pub fn save(dir: &Path, theme: &Theme) -> Result<()> {
    validate(theme)?;
    std::fs::create_dir_all(dir)?;
    let body = serde_json::to_vec_pretty(theme)?;
    let tmp = dir.join(format!(".{}.{}.tmp", theme.id, uuid::Uuid::new_v4()));
    let write = |tmp: &Path| -> Result<()> {
        let mut file: File = OpenOptions::new().write(true).create_new(true).open(tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
        std::fs::rename(tmp, path_of(dir, &theme.id))?;
        Ok(())
    };
    let written = write(&tmp);
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A missing theme is not an error.
pub fn delete(dir: &Path, id: &str) -> Result<()> {
    check_id(id)?;
    match std::fs::remove_file(path_of(dir, id)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-themes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn theme(id: &str, name: &str) -> Theme {
        Theme {
            id: id.into(),
            name: name.into(),
            base: Base::Dark,
            tokens: BTreeMap::from([
                ("--motiv-bg".to_owned(), "#101010".to_owned()),
                ("--zw-rail-bg".to_owned(), "rgb(16 16 16 / 0.9)".to_owned()),
            ]),
            terminal: TerminalColors {
                background: "#000000".into(),
                foreground: "#ffffff".into(),
                cursor: "#ff8800".into(),
                selection: "rgba(255, 255, 255, 0.3)".into(),
                ansi: (0..16).map(|i| format!("#0000{i:02x}")).collect(),
            },
        }
    }

    fn rejects(change: impl FnOnce(&mut Theme)) -> bool {
        let mut t = theme("good", "Good");
        change(&mut t);
        matches!(validate(&t), Err(Error::InvalidTheme(_)))
    }

    #[test]
    fn a_good_theme_is_valid() {
        validate(&theme("good", "Good")).unwrap();
        validate(&theme("0-a-b9", &"n".repeat(64))).unwrap();
        validate(&theme(&"a".repeat(64), "  Padded  ")).unwrap();
        let mut many = theme("many", "Many");
        many.tokens = (0..128).map(|i| (format!("--zw-t{i}"), "1px".to_owned())).collect();
        validate(&many).unwrap();
        let mut long = theme("long", "Long");
        long.tokens.insert(format!("--zw-{}", "a".repeat(59)), "x".repeat(128));
        validate(&long).unwrap();
    }

    #[test]
    fn each_bad_field_is_rejected() {
        for id in ["", "Upper", "-lead", "a_b", "a.b", "../x", "a b", "é"] {
            assert!(rejects(|t| t.id = id.into()), "{id}");
        }
        assert!(rejects(|t| t.id = "a".repeat(65)));
        for name in ["", "   ", "a\nb", "a\u{7}b"] {
            assert!(rejects(|t| t.name = name.into()), "{name:?}");
        }
        assert!(rejects(|t| t.name = "n".repeat(65)));
        assert!(rejects(|t| t.tokens = (0..129).map(|i| (format!("--zw-t{i}"), "1px".to_owned())).collect()));
        let long_key = format!("--zw-{}", "a".repeat(60));
        for key in ["--other-bg", "motiv-bg", "--zw-", "--motiv-", "--zw-Bg", "--zw-b_g", "--zw-b g", long_key.as_str()]
        {
            assert!(rejects(|t| _ = t.tokens.insert(key.into(), "red".into())), "{key}");
        }
        let long_value = "x".repeat(129);
        for value in [
            "",
            long_value.as_str(),
            "red; color: blue",
            "red{",
            "}",
            "url(x)",
            "URL(x)",
            "image-set(Url(x))",
            "a\nb",
            "a\u{0}b",
            "\\72 ed",
        ] {
            assert!(rejects(|t| _ = t.tokens.insert("--zw-bg".into(), value.into())), "{value:?}");
            assert!(rejects(|t| t.terminal.background = value.into()), "{value:?}");
            assert!(rejects(|t| t.terminal.foreground = value.into()), "{value:?}");
            assert!(rejects(|t| t.terminal.cursor = value.into()), "{value:?}");
            assert!(rejects(|t| t.terminal.selection = value.into()), "{value:?}");
            assert!(rejects(|t| t.terminal.ansi[15] = value.into()), "{value:?}");
        }
        assert!(rejects(|t| _ = t.terminal.ansi.pop()));
        assert!(rejects(|t| t.terminal.ansi.push("#ffffff".into())));
        assert!(rejects(|t| t.terminal.ansi.clear()));
    }

    #[test]
    fn the_error_has_its_own_code() {
        let error = validate(&theme("Bad", "Bad")).unwrap_err();
        assert_eq!(error.code(), "InvalidTheme");
        assert!(error.message().starts_with("Invalid theme: "), "{}", error.message());
    }

    #[test]
    fn the_json_shape_is_camel_case_with_a_lowercase_base() {
        let json = serde_json::to_value(theme("good", "Good")).unwrap();
        assert_eq!(json["base"], "dark");
        assert_eq!(json["tokens"]["--motiv-bg"], "#101010");
        assert_eq!(json["terminal"]["ansi"].as_array().unwrap().len(), 16);
        let light: Base = serde_json::from_str(r#""light""#).unwrap();
        assert_eq!(light, Base::Light);
        assert!(serde_json::from_str::<Base>(r#""Light""#).is_err());
    }

    #[test]
    fn a_missing_dir_has_no_themes() {
        assert_eq!(list(&temp_dir().join("themes")), ThemeList::default());
    }

    #[test]
    fn save_and_list_round_trip_sorted_by_name() {
        let dir = temp_dir().join("themes");
        save(&dir, &theme("zeta", "Alpha")).unwrap();
        save(&dir, &theme("alpha", "Zeta")).unwrap();
        let mut changed = theme("zeta", "Beta");
        changed.base = Base::Light;
        save(&dir, &changed).unwrap();

        let listed = list(&dir);
        assert_eq!(listed.themes, [changed, theme("alpha", "Zeta")]);
        assert!(listed.skipped.is_empty());
        let mut names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["alpha.json", "zeta.json"], "no temp file is left");
    }

    #[test]
    fn save_refuses_a_bad_theme_and_writes_nothing() {
        let dir = temp_dir().join("themes");
        assert!(matches!(save(&dir, &theme("Bad", "Bad")), Err(Error::InvalidTheme(_))));
        assert!(!dir.exists());
    }

    #[test]
    fn bad_files_are_skipped() {
        let dir = temp_dir();
        save(&dir, &theme("good", "Good")).unwrap();
        std::fs::write(dir.join("broken.json"), b"{not json").unwrap();
        let mut bad = serde_json::to_value(theme("invalid", "Invalid")).unwrap();
        bad["tokens"]["--zw-bg"] = "url(https://x)".into();
        std::fs::write(dir.join("invalid.json"), bad.to_string()).unwrap();
        std::fs::write(dir.join("other.json"), serde_json::to_vec(&theme("renamed", "Renamed")).unwrap()).unwrap();
        std::fs::create_dir(dir.join("folder.json")).unwrap();
        std::fs::write(dir.join("big.json"), vec![b' '; FILE_LIMIT as usize + 1]).unwrap();
        std::fs::write(dir.join("notes.txt"), b"not a theme").unwrap();
        std::fs::write(dir.join(".good.x.tmp"), b"half").unwrap();

        let listed = list(&dir);
        assert_eq!(listed.themes, [theme("good", "Good")]);
        assert_eq!(listed.skipped, ["big.json", "broken.json", "folder.json", "invalid.json", "other.json"]);
    }

    #[test]
    fn a_link_is_skipped() {
        let dir = temp_dir();
        let outside = temp_dir().join("linked.json");
        std::fs::write(&outside, serde_json::to_vec(&theme("linked", "Linked")).unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("linked.json")).unwrap();
        let listed = list(&dir);
        assert!(listed.themes.is_empty());
        assert_eq!(listed.skipped, ["linked.json"]);
    }

    #[test]
    fn save_does_not_follow_a_link_at_the_target() {
        let dir = temp_dir();
        let outside = temp_dir().join("outside.json");
        std::fs::write(&outside, b"keep me").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("good.json")).unwrap();

        save(&dir, &theme("good", "Good")).unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");
        assert!(std::fs::symlink_metadata(dir.join("good.json")).unwrap().file_type().is_file());
        assert_eq!(list(&dir).themes, [theme("good", "Good")]);
    }

    #[test]
    fn delete_removes_the_file_and_a_missing_theme_is_ok() {
        let dir = temp_dir();
        save(&dir, &theme("good", "Good")).unwrap();
        delete(&dir, "good").unwrap();
        assert!(!dir.join("good.json").exists());
        delete(&dir, "good").unwrap();
        delete(&temp_dir().join("missing"), "good").unwrap();
        assert!(matches!(delete(&dir, "../x"), Err(Error::InvalidTheme(_))));
    }

    #[test]
    fn delete_removes_a_link_and_not_its_target() {
        let dir = temp_dir();
        let outside = temp_dir().join("outside.json");
        std::fs::write(&outside, b"keep me").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("good.json")).unwrap();
        delete(&dir, "good").unwrap();
        assert!(std::fs::symlink_metadata(dir.join("good.json")).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");
    }
}
