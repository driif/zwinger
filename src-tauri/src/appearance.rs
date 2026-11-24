//! User themes: one JSON file per theme in `<app data>/themes/<id>.json`.

use std::collections::{BTreeMap, BTreeSet};
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

/// The ids of the built-in themes in `src/features/appearance/builtIns.ts`.
/// A custom theme must not use them.
pub const RESERVED_IDS: [&str; 3] = ["zwinger-dark", "zwinger-light", "zwinger-contrast"];
/// The names of the built-in themes. An import does not reuse them.
const BUILT_IN_NAMES: [&str; 3] = ["zwinger dark", "zwinger light", "High contrast"];
/// Attempts to publish an import under a free id before the import fails.
const PUBLISH_TRIES: usize = 5;

/// A safe id that is not the id of a built-in theme.
fn check_custom_id(id: &str) -> Result<()> {
    check_id(id)?;
    if RESERVED_IDS.contains(&id) {
        return Err(invalid(format!("the id {id} belongs to a built-in theme")));
    }
    Ok(())
}

/// Reads a regular file of at most `FILE_LIMIT` bytes and follows no link.
/// A FIFO or a device fails at once and does not block.
fn load(path: &Path) -> Result<(Theme, Vec<u8>)> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(
        |e| match e.raw_os_error() {
            Some(libc::ELOOP) => invalid("the file is a link; choose the file that it points to".into()),
            _ => e.into(),
        },
    )?;
    if !file.metadata()?.is_file() {
        return Err(invalid("the path is not a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_LIMIT {
        return Err(invalid(format!("the file is larger than {} KiB", FILE_LIMIT >> 10)));
    }
    let theme: Theme =
        serde_json::from_slice(&bytes).map_err(|e| invalid(format!("the file is not a zwinger theme: {e}")))?;
    validate(&theme)?;
    Ok((theme, bytes))
}

/// Reads and validates a theme file. The error tells the user why it is not valid.
pub fn read_file(path: &Path) -> Result<Theme> {
    load(path).map(|(theme, _)| theme)
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
        match read_file(&path).ok().filter(|t| regular && t.id == stem) {
            Some(theme) => list.themes.push(theme),
            None => list.skipped.push(name),
        }
    }
    list.themes.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    list.skipped.sort();
    list
}

/// Pretty JSON if it fits in `FILE_LIMIT`, else compact JSON, so that `list`
/// reads each stored file again.
fn encode(theme: &Theme) -> Result<Vec<u8>> {
    let pretty = serde_json::to_vec_pretty(theme)?;
    if pretty.len() as u64 <= FILE_LIMIT {
        return Ok(pretty);
    }
    let compact = serde_json::to_vec(theme)?;
    if compact.len() as u64 <= FILE_LIMIT {
        return Ok(compact);
    }
    Err(invalid(format!("the theme is larger than {} KiB", FILE_LIMIT >> 10)))
}

/// Writes `bytes` to a new temp file in the dir of `path`, then calls `publish`
/// with the temp path. The temp file is removed if a step fails.
fn with_temp<T>(path: &Path, bytes: &[u8], publish: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let dir = path.parent().ok_or_else(|| invalid("the path has no folder".into()))?;
    let tmp = dir.join(format!(".zwinger-{}.tmp", uuid::Uuid::new_v4()));
    let mut file: File = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let write = || -> Result<T> {
        file.write_all(bytes)?;
        file.sync_all()?;
        publish(&tmp)
    };
    let written = write();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Writes a new temp file next to `path` and renames it onto `path`. The
/// rename replaces a link at that path and does not follow it.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    with_temp(path, bytes, |tmp| Ok(std::fs::rename(tmp, path)?))
}

/// Publishes the theme as `<id>.json` and never replaces a file there.
/// `false`: the path is taken.
fn publish_new(dir: &Path, theme: &Theme) -> Result<bool> {
    let bytes = encode(theme)?;
    std::fs::create_dir_all(dir)?;
    with_temp(&path_of(dir, &theme.id), &bytes, |tmp| {
        let linked = std::fs::hard_link(tmp, path_of(dir, &theme.id));
        // The destination is already published on success, even if the temporary name remains.
        let _ = std::fs::remove_file(tmp);
        match linked {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    })
}

pub fn save(dir: &Path, theme: &Theme) -> Result<()> {
    validate(theme)?;
    check_custom_id(&theme.id)?;
    let bytes = encode(theme)?;
    std::fs::create_dir_all(dir)?;
    write_atomic(&path_of(dir, &theme.id), &bytes)
}

/// The result of an import. `renamed`: the theme got a different name,
/// because a saved or built-in theme has `original_name`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Imported {
    pub theme: Theme,
    pub original_name: String,
    pub renamed: bool,
}

fn fresh_id() -> String {
    format!("custom-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

/// The words of `name`, joined by one space.
fn collapse(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `name`, or `name 2`, `name 3`, ... that is not in `used`, after `collapse`. The base
/// is cut so that the result has at most `NAME_LIMIT` characters.
fn free_name(name: &str, used: &BTreeSet<String>) -> String {
    let base = collapse(name);
    if !used.contains(&base) {
        return name.to_owned();
    }
    (2..)
        .map(|n| {
            let suffix = format!(" {n}");
            let keep = NAME_LIMIT - suffix.chars().count();
            let cut: String = base.chars().take(keep).collect();
            format!("{}{suffix}", cut.trim_end())
        })
        .find(|candidate| !used.contains(candidate))
        .expect("an unused suffix exists")
}

/// Copies a theme file into `dir` as a new theme. It never replaces a file:
/// a taken id gets a fresh id, and a taken name gets a number.
pub fn import(dir: &Path, src: &Path, selected: &[&str]) -> Result<Imported> {
    let mut theme = read_file(src)?;
    check_custom_id(&theme.id)?;
    // Two open pickers may finish together. Allocate names and publish under the same lock.
    static IMPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _import = IMPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Each `*.json` name counts, also a file that `list` skips.
    let mut taken: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => {
            entries.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_owned)).collect()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let names: BTreeSet<String> = list(dir)
        .themes
        .into_iter()
        .map(|theme| collapse(&theme.name))
        .chain(BUILT_IN_NAMES.map(str::to_owned))
        .collect();
    let original_name = theme.name.clone();
    theme.name = free_name(&original_name, &names);
    for _ in 0..PUBLISH_TRIES {
        while taken.contains(&theme.id) || selected.contains(&theme.id.as_str()) {
            theme.id = fresh_id();
        }
        if publish_new(dir, &theme)? {
            let renamed = theme.name != original_name;
            return Ok(Imported { theme, original_name, renamed });
        }
        taken.push(theme.id.clone());
    }
    Err(invalid("no free theme id was found; try again".into()))
}

/// Writes the stored file of the custom theme `id` to `dest`.
pub fn export(dir: &Path, id: &str, dest: &Path) -> Result<()> {
    check_custom_id(id)?;
    let (theme, bytes) = load(&path_of(dir, id))?;
    if theme.id != id {
        return Err(invalid(format!("the file {id}.json has the id {}", theme.id)));
    }
    write_atomic(dest, &bytes)
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

    fn import(dir: &Path, src: &Path) -> Result<Imported> {
        super::import(dir, src, &[])
    }

    fn temp_dir() -> PathBuf {
        let dir = crate::test_dir::root().join(format!("zwinger-themes-{}", uuid::Uuid::new_v4()));
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

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    }

    fn write_json(path: &Path, theme: &Theme) {
        std::fs::write(path, serde_json::to_vec(theme).unwrap()).unwrap();
    }

    fn reason(result: Result<impl std::fmt::Debug>) -> String {
        match result {
            Err(Error::InvalidTheme(why)) => why,
            other => panic!("not an invalid theme: {other:?}"),
        }
    }

    /// 128 tokens with 64-character keys and multibyte values.
    fn big(value: &str) -> Theme {
        let mut t = theme("big", "Big pretty theme");
        t.tokens = (0..128).map(|i| (format!("{:a<64}", format!("--zw-t{i:03}")), value.to_owned())).collect();
        t
    }

    #[test]
    fn export_then_import_round_trips_each_property() {
        let dir = temp_dir();
        let mut full = theme("full", "Full");
        full.base = Base::Light;
        full.tokens.insert("--motiv-radius".into(), "6px".into());
        full.tokens.insert("--motiv-font-size".into(), "0.875rem".into());
        save(&dir, &full).unwrap();
        let out = temp_dir().join("full.json");
        export(&dir, "full", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(dir.join("full.json")).unwrap());

        let other = temp_dir();
        let imported = import(&other, &out).unwrap();
        assert_eq!(imported, Imported { theme: full.clone(), original_name: "Full".into(), renamed: false });
        assert_eq!(list(&other).themes, [full]);
    }

    #[test]
    fn import_rejects_a_bad_source_and_writes_nothing() {
        let src = temp_dir();
        let dir = temp_dir();
        std::fs::write(src.join("big.json"), vec![b' '; FILE_LIMIT as usize + 1]).unwrap();
        assert!(reason(import(&dir, &src.join("big.json"))).contains("larger than 64 KiB"));
        std::fs::write(src.join("bad.json"), b"{not json").unwrap();
        assert!(reason(import(&dir, &src.join("bad.json"))).contains("not a zwinger theme"));
        let mut bad = serde_json::to_value(theme("invalid", "Invalid")).unwrap();
        bad["tokens"]["--zw-bg"] = "url(https://x)".into();
        std::fs::write(src.join("invalid.json"), bad.to_string()).unwrap();
        assert!(reason(import(&dir, &src.join("invalid.json"))).contains("url("));
        write_json(&src.join("unsafe.json"), &Theme { id: "../x".into(), ..theme("x", "X") });
        assert!(reason(import(&dir, &src.join("unsafe.json"))).contains("lowercase slug"));
        for id in RESERVED_IDS {
            write_json(&src.join("reserved.json"), &theme(id, "Mine"));
            assert!(reason(import(&dir, &src.join("reserved.json"))).contains("built-in"), "{id}");
        }
        assert!(files(&dir).is_empty());
    }

    #[test]
    fn import_rejects_a_link_and_a_fifo() {
        let src = temp_dir();
        let dir = temp_dir();
        write_json(&src.join("real.json"), &theme("real", "Real"));
        std::os::unix::fs::symlink(src.join("real.json"), src.join("link.json")).unwrap();
        assert!(reason(import(&dir, &src.join("link.json"))).contains("link"));

        let fifo = src.join("fifo.json");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(reason(import(&dir, &fifo)).contains("not a regular file"));
        assert!(files(&dir).is_empty());
    }

    #[test]
    fn a_taken_id_gets_a_fresh_id_and_keeps_the_old_file() {
        let src = temp_dir();
        let dir = temp_dir();
        save(&dir, &theme("mine", "Old")).unwrap();
        let before = std::fs::read(dir.join("mine.json")).unwrap();
        std::fs::write(dir.join("broken.json"), b"{not json").unwrap();
        write_json(&src.join("a.json"), &theme("mine", "New"));
        write_json(&src.join("b.json"), &theme("broken", "Other"));

        let a = import(&dir, &src.join("a.json")).unwrap();
        let b = import(&dir, &src.join("b.json")).unwrap();
        for id in [&a.theme.id, &b.theme.id] {
            assert!(id.starts_with("custom-") && id.len() == 15, "{id}");
            check_custom_id(id).unwrap();
        }
        assert_eq!(std::fs::read(dir.join("mine.json")).unwrap(), before);
        assert_eq!(std::fs::read(dir.join("broken.json")).unwrap(), b"{not json");
        let listed = list(&dir);
        assert_eq!(listed.themes.len(), 3);
        assert_eq!(listed.skipped, ["broken.json"]);
        assert_eq!(files(&dir).len(), 4, "no temp file is left");
    }

    #[test]
    fn a_taken_name_gets_a_number() {
        let src = temp_dir();
        let dir = temp_dir();
        save(&dir, &theme("one", "Sea")).unwrap();
        save(&dir, &theme("two", "Sea 2")).unwrap();
        write_json(&src.join("sea.json"), &theme("three", "Sea"));
        let imported = import(&dir, &src.join("sea.json")).unwrap();
        assert_eq!(
            (imported.theme.name.as_str(), imported.original_name.as_str(), imported.renamed),
            ("Sea 3", "Sea", true)
        );
        assert_eq!(imported.theme.id, "three");

        let long = "n".repeat(NAME_LIMIT);
        save(&dir, &theme("long", &long)).unwrap();
        write_json(&src.join("long.json"), &theme("long2", &long));
        let imported = import(&dir, &src.join("long.json")).unwrap();
        assert_eq!(imported.theme.name, format!("{} 2", "n".repeat(NAME_LIMIT - 2)));
        validate(&imported.theme).unwrap();
    }

    #[test]
    fn publish_new_does_not_replace_a_file_that_appears() {
        let dir = temp_dir();
        std::fs::write(dir.join("late.json"), b"keep me").unwrap();
        assert!(!publish_new(&dir, &theme("late", "Late")).unwrap());
        assert_eq!(std::fs::read(dir.join("late.json")).unwrap(), b"keep me");
        assert_eq!(files(&dir), ["late.json"], "no temp file is left");
        assert!(publish_new(&dir, &theme("free", "Free")).unwrap());
    }

    #[test]
    fn a_large_theme_is_stored_compact_and_read_again() {
        let large = big(&format!("{}€€{}", "😀".repeat(100), "x".repeat(26)));
        validate(&large).unwrap();
        assert_eq!(serde_json::to_vec(&large).unwrap().len(), 64_605);
        assert_eq!(serde_json::to_vec_pretty(&large).unwrap().len(), 65_547);
        let dir = temp_dir();
        save(&dir, &large).unwrap();
        assert_eq!(std::fs::read(dir.join("big.json")).unwrap().len(), 64_605);
        assert_eq!(list(&dir).themes, std::slice::from_ref(&large));
        let out = temp_dir().join("big.json");
        export(&dir, "big", &out).unwrap();
        assert_eq!(import(&temp_dir(), &out).unwrap().theme, large);
    }

    #[test]
    fn a_theme_too_large_for_compact_json_is_rejected() {
        let huge = big(&"😀".repeat(128));
        validate(&huge).unwrap();
        let dir = temp_dir().join("themes");
        assert!(reason(save(&dir, &huge)).contains("larger than 64 KiB"));
        assert!(!dir.exists());
    }

    #[test]
    fn export_checks_the_id_and_a_failed_write_changes_no_theme() {
        let dir = temp_dir();
        save(&dir, &theme("good", "Good")).unwrap();
        let before = files(&dir);
        assert!(reason(export(&dir, "../good", &temp_dir().join("x.json"))).contains("lowercase slug"));
        assert!(reason(export(&dir, "zwinger-dark", &temp_dir().join("x.json"))).contains("built-in"));
        assert!(matches!(export(&dir, "good", &temp_dir().join("missing/x.json")), Err(Error::Io(_))));
        assert_eq!(files(&dir), before);
        assert_eq!(list(&dir).themes, [theme("good", "Good")]);
    }

    #[test]
    fn save_refuses_a_reserved_id() {
        let dir = temp_dir().join("themes");
        assert!(reason(save(&dir, &theme("zwinger-light", "Mine"))).contains("built-in"));
        assert!(!dir.exists());
    }

    #[test]
    fn the_reserved_ids_and_names_are_the_built_in_ones() {
        let source = include_str!("../../src/features/appearance/builtIns.ts");
        let values = |key: &str| -> Vec<String> {
            let prefix = format!("{key}: \"");
            source
                .lines()
                .filter_map(|line| Some(line.trim().strip_prefix(prefix.as_str())?.strip_suffix("\",")?.to_owned()))
                .collect()
        };
        assert_eq!(values("id"), RESERVED_IDS);
        assert_eq!(values("name"), BUILT_IN_NAMES);
    }

    /// Imports each source file into `dir` at the same time.
    fn import_together(dir: &Path, sources: Vec<PathBuf>) -> Vec<Imported> {
        let start = std::sync::Barrier::new(sources.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = sources
                .iter()
                .map(|src| {
                    let start = &start;
                    scope.spawn(move || {
                        start.wait();
                        import(dir, src).unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    }

    fn distinct(values: impl IntoIterator<Item = String>) -> usize {
        values.into_iter().collect::<BTreeSet<_>>().len()
    }

    #[test]
    fn concurrent_imports_with_the_same_id_and_name_get_distinct_ids_and_names() {
        let src = temp_dir();
        let dir = temp_dir();
        let sources: Vec<PathBuf> = (0..8)
            .map(|i| {
                let path = src.join(format!("{i}.json"));
                write_json(&path, &theme("same", "Same"));
                path
            })
            .collect();
        let imported = import_together(&dir, sources);
        assert_eq!(distinct(imported.iter().map(|i| i.theme.id.clone())), 8);
        assert_eq!(distinct(imported.iter().map(|i| i.theme.name.clone())), 8);
        assert_eq!(list(&dir).themes.len(), 8);
        assert_eq!(files(&dir).len(), 8, "no temp file is left");
    }

    #[test]
    fn concurrent_imports_with_different_ids_and_the_same_name_get_distinct_names() {
        let src = temp_dir();
        let dir = temp_dir();
        let sources: Vec<PathBuf> = (0..8)
            .map(|i| {
                let path = src.join(format!("{i}.json"));
                write_json(&path, &theme(&format!("id{i}"), "Same"));
                path
            })
            .collect();
        let imported = import_together(&dir, sources);
        assert!(imported.iter().all(|i| i.theme.id.starts_with("id")));
        assert_eq!(distinct(imported.iter().map(|i| i.theme.name.clone())), 8);
    }

    #[test]
    fn a_built_in_name_gets_a_number() {
        let src = temp_dir();
        let dir = temp_dir();
        for (i, name) in BUILT_IN_NAMES.iter().enumerate() {
            write_json(&src.join("t.json"), &theme(&format!("t{i}"), name));
            let imported = import(&dir, &src.join("t.json")).unwrap();
            assert_eq!(imported.theme.name, format!("{name} 2"));
            assert!(imported.renamed);
        }
    }

    #[test]
    fn names_that_differ_only_in_whitespace_collide() {
        let src = temp_dir();
        let dir = temp_dir();
        save(&dir, &theme("one", "Deep  Sea")).unwrap();
        write_json(&src.join("a.json"), &theme("two", " Deep Sea "));
        assert_eq!(import(&dir, &src.join("a.json")).unwrap().theme.name, "Deep Sea 2");
        write_json(&src.join("b.json"), &theme("three", "zwinger  dark"));
        assert_eq!(import(&dir, &src.join("b.json")).unwrap().theme.name, "zwinger dark 2");
    }

    #[test]
    fn a_selected_id_without_a_file_gets_a_fresh_id() {
        let src = temp_dir();
        let dir = temp_dir();
        save(&dir, &theme("kept", "Kept")).unwrap();
        let before = std::fs::read(dir.join("kept.json")).unwrap();
        write_json(&src.join("gone.json"), &theme("gone", "Gone"));
        let selected = ["zwinger-dark", "gone", "zwinger-light"];

        let imported = super::import(&dir, &src.join("gone.json"), &selected).unwrap();
        assert!(imported.theme.id.starts_with("custom-"), "{}", imported.theme.id);
        assert_eq!(selected, ["zwinger-dark", "gone", "zwinger-light"]);
        assert_eq!(std::fs::read(dir.join("kept.json")).unwrap(), before);
        let mut expected = ["kept.json".to_owned(), format!("{}.json", imported.theme.id)];
        expected.sort();
        assert_eq!(files(&dir), expected);
    }

    #[test]
    fn export_to_a_long_file_name_leaves_no_temp_file() {
        let dir = temp_dir();
        save(&dir, &theme("good", "Good")).unwrap();
        let out = temp_dir();
        let name = format!("{}.json", "a".repeat(217));
        assert_eq!(name.len(), 222);
        export(&dir, "good", &out.join(&name)).unwrap();
        assert_eq!(files(&out), [name]);
    }

    #[test]
    fn a_failed_write_keeps_a_temp_file_that_it_did_not_create() {
        let dir = temp_dir();
        save(&dir, &theme("good", "Good")).unwrap();
        let out = temp_dir();
        std::fs::write(out.join(".zwinger-other.tmp"), b"keep me").unwrap();
        std::fs::create_dir(out.join("busy.json")).unwrap();
        std::fs::write(out.join("busy.json/inside"), b"x").unwrap();

        assert!(matches!(export(&dir, "good", &out.join("busy.json")), Err(Error::Io(_))));
        assert_eq!(files(&out), [".zwinger-other.tmp", "busy.json"]);
        assert_eq!(std::fs::read(out.join(".zwinger-other.tmp")).unwrap(), b"keep me");
    }
}
