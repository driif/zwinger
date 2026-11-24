//! Reads the terminal settings and colors of a Ghostty config.

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::preferences::{CursorStyle, FONT_SIZE, LINE_HEIGHT, PADDING, SCROLLBACK};

/// A larger config file is not read.
const FILE_LIMIT: u64 = 1 << 20;
const ANSI_COUNT: usize = 16;

/// The settings that the config sets; the other fields stay as they are.
#[derive(Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_style: Option<CursorStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_blink: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option_as_meta: Option<bool>,
}

/// The colors that the config sets, as `#rrggbb`. `ansi` has the 16 ANSI
/// colors in order, `None` where the config sets none.
#[derive(Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Palette {
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub cursor: Option<String>,
    pub selection: Option<String>,
    pub ansi: [Option<String>; ANSI_COUNT],
}

#[derive(Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Import {
    pub settings: Settings,
    /// `None` if the config sets no color.
    pub palette: Option<Palette>,
    /// The keys that zwinger does not use, each once, in file order.
    pub ignored: Vec<String>,
    /// The numeric settings whose applied config value was out of range, in file order.
    pub adjusted: Vec<Adjustment>,
}

/// A config value that was out of range. `setting` is the camelCase name of the setting.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Adjustment {
    pub setting: &'static str,
    /// The config line as `key = value`.
    pub input: String,
    /// The limit that the setting got.
    pub value: f64,
}

/// The config files, in the order that Ghostty reads them: a later file wins.
pub fn config_paths(home: &Path) -> [PathBuf; 3] {
    [
        home.join("Library/Application Support/com.mitchellh.ghostty/config"),
        home.join(".config/ghostty/config"),
        home.join(".config/agterm/ghostty.conf"),
    ]
}

/// Reads the config files that exist. Only regular files (also through a
/// link) of at most 1 MiB count.
pub fn import(home: &Path) -> Result<Import> {
    let paths = config_paths(home);
    let texts: Vec<String> = paths
        .iter()
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= FILE_LIMIT))
        .filter_map(|p| std::fs::read(p).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect();
    if texts.is_empty() {
        return Err(Error::MissingPath(paths[1].display().to_string()));
    }
    Ok(parse_files(texts.iter().map(String::as_str)))
}

#[cfg(test)]
pub fn parse(text: &str) -> Import {
    parse_files([text])
}

/// Parses the files in order. A later value wins, except for `font-family`:
/// the first one in a file starts a new list, and a repeat in the same file
/// adds a fallback.
fn parse_files<'a>(texts: impl IntoIterator<Item = &'a str>) -> Import {
    let mut import = Import::default();
    let mut palette = Palette::default();
    let mut fonts: Vec<String> = Vec::new();
    for text in texts {
        let mut new_file = true;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), unquote(value.trim()));
            let settings = &mut import.settings;
            let adjusted = &mut import.adjusted;
            let used = match key {
                "config-file" => true,
                "font-family" => {
                    // An empty value clears the list.
                    if std::mem::take(&mut new_file) || value.is_empty() {
                        fonts.clear();
                    }
                    if !value.is_empty() {
                        fonts.push(value.to_owned());
                    }
                    true
                }
                "font-size" => set_number(
                    &mut settings.font_size,
                    value.parse::<f64>().ok().filter(|s| s.is_finite() && *s > 0.0),
                    &FONT_SIZE,
                    Note { setting: "fontSize", key, value, adjusted },
                ),
                "adjust-cell-height" => set_number(
                    &mut settings.line_height,
                    value
                        .strip_suffix('%')
                        .and_then(|p| p.trim().parse::<f64>().ok())
                        .filter(|p| p.is_finite())
                        .map(|p| 1.0 + p / 100.0),
                    &LINE_HEIGHT,
                    Note { setting: "lineHeight", key, value, adjusted },
                ),
                "cursor-style" => set(
                    &mut settings.cursor_style,
                    match value {
                        "block" | "block_hollow" => Some(CursorStyle::Block),
                        "bar" => Some(CursorStyle::Bar),
                        "underline" => Some(CursorStyle::Underline),
                        _ => None,
                    },
                ),
                "cursor-style-blink" => set(&mut settings.cursor_blink, value.parse().ok()),
                "scrollback-limit" => set_number(
                    &mut settings.scrollback,
                    value.parse::<u64>().ok().map(|bytes| u32::try_from(bytes / 100).unwrap_or(u32::MAX)),
                    &SCROLLBACK,
                    Note { setting: "scrollback", key, value, adjusted },
                ),
                "window-padding-x" => set_number(
                    &mut settings.padding,
                    value.split(',').next().and_then(|x| x.trim().parse::<u32>().ok()),
                    &PADDING,
                    Note { setting: "padding", key, value, adjusted },
                ),
                "macos-option-as-alt" => set(
                    &mut settings.option_as_meta,
                    match value {
                        "true" | "left" | "right" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    },
                ),
                "background" => set(&mut palette.background, color(value)),
                "foreground" => set(&mut palette.foreground, color(value)),
                "cursor-color" => set(&mut palette.cursor, color(value)),
                "selection-background" => set(&mut palette.selection, color(value)),
                "palette" => match value.split_once('=') {
                    Some((index, hex)) => match (index.trim().parse::<usize>(), color(hex.trim())) {
                        (Ok(index), Some(hex)) if index < ANSI_COUNT => {
                            palette.ansi[index] = Some(hex);
                            true
                        }
                        // The 256-color part has no place in a theme.
                        (Ok(_), Some(_)) => true,
                        _ => false,
                    },
                    None => false,
                },
                _ => false,
            };
            if !used && !import.ignored.iter().any(|k| k == key) {
                import.ignored.push(key.to_owned());
            }
        }
    }
    if !fonts.is_empty() {
        let mut list: Vec<String> = fonts.iter().map(|f| format!("\"{}\"", f.replace('"', ""))).collect();
        list.push("monospace".into());
        import.settings.font_family = Some(list.join(", "));
    }
    if palette != Palette::default() {
        import.palette = Some(palette);
    }
    import
}

/// Sets `field` to a valid value. Returns if the value was valid.
fn set<T>(field: &mut Option<T>, value: Option<T>) -> bool {
    let valid = value.is_some();
    if valid {
        *field = value;
    }
    valid
}

/// Where `set_number` notes a clamp.
struct Note<'a> {
    setting: &'static str,
    key: &'a str,
    value: &'a str,
    adjusted: &'a mut Vec<Adjustment>,
}

/// Sets `field` to a valid value clamped to `range`. The note of an earlier value of the same setting goes, because this
/// value replaces it. Returns if the value was valid.
fn set_number<T: Copy + PartialOrd + Into<f64>>(
    field: &mut Option<T>,
    value: Option<T>,
    range: &RangeInclusive<T>,
    note: Note,
) -> bool {
    let Some(value) = value else {
        return false;
    };
    let applied = if value < *range.start() {
        *range.start()
    } else if value > *range.end() {
        *range.end()
    } else {
        value
    };
    note.adjusted.retain(|a| a.setting != note.setting);
    if applied != value {
        note.adjusted.push(Adjustment {
            setting: note.setting,
            input: format!("{} = {}", note.key, note.value),
            value: applied.into(),
        });
    }
    *field = Some(applied);
    true
}

fn unquote(value: &str) -> &str {
    value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value)
}

/// `#rrggbb` or `rrggbb` as lowercase `#rrggbb`. A named color is not used.
fn color(value: &str) -> Option<String> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("#{}", hex.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_settings_and_the_palette() {
        let import = parse(
            r#"
# a comment
font-family = "JetBrains Mono"
font-family = Symbols Nerd Font
font-size = 14
adjust-cell-height = 20%
cursor-style = block_hollow
cursor-style-blink = false
scrollback-limit = 50000000
window-padding-x = 6,10
macos-option-as-alt = left
background = 1e1e2e
foreground = #CDD6F4
cursor-color = #f5e0dc
selection-background = #585b70
palette = 0=#45475a
palette = 15=#a6adc8
palette = 200=#ffffff
theme = catppuccin-mocha
keybind = cmd+t=new_tab
keybind = cmd+w=close_surface
config-file = other
"#,
        );
        assert_eq!(
            import.settings,
            Settings {
                font_family: Some(r#""JetBrains Mono", "Symbols Nerd Font", monospace"#.into()),
                font_size: Some(14.0),
                line_height: Some(1.2),
                cursor_style: Some(CursorStyle::Block),
                cursor_blink: Some(false),
                padding: Some(6),
                scrollback: Some(100_000),
                option_as_meta: Some(true),
            }
        );
        let palette = import.palette.unwrap();
        assert_eq!(palette.background.as_deref(), Some("#1e1e2e"));
        assert_eq!(palette.foreground.as_deref(), Some("#cdd6f4"));
        assert_eq!((palette.cursor.as_deref(), palette.selection.as_deref()), (Some("#f5e0dc"), Some("#585b70")));
        assert_eq!((palette.ansi[0].as_deref(), palette.ansi[15].as_deref()), (Some("#45475a"), Some("#a6adc8")));
        assert!(palette.ansi[1..15].iter().all(Option::is_none));
        assert_eq!(import.ignored, ["theme", "keybind"]);
    }

    #[test]
    fn a_later_value_wins_and_bad_values_are_ignored() {
        let import = parse(
            "font-size = 12\nfont-size = 16\nadjust-cell-height = 2\nscrollback-limit = 10\nbackground = red\nfont-family = A\nfont-family =\n",
        );
        assert_eq!(import.settings, Settings { font_size: Some(16.0), scrollback: Some(1000), ..Settings::default() });
        assert_eq!(import.palette, None);
        assert_eq!(import.ignored, ["adjust-cell-height", "background"]);
    }

    #[test]
    fn numbers_are_clamped_to_the_pref_ranges() {
        let import = parse("font-size = 64\nadjust-cell-height = -40%\nwindow-padding-x = 80\n");
        assert_eq!(
            import.settings,
            Settings { font_size: Some(32.0), line_height: Some(1.0), padding: Some(32), ..Settings::default() }
        );
        assert_eq!(parse("adjust-cell-height = 300%\n").settings.line_height, Some(2.0));
    }

    fn adjusted(text: &str) -> Vec<(&'static str, String, f64)> {
        parse(text).adjusted.into_iter().map(|a| (a.setting, a.input, a.value)).collect()
    }

    #[test]
    fn notes_each_clamped_value_with_its_config_line() {
        assert_eq!(
            adjusted(
                "font-size = 64\nadjust-cell-height = -40%\nwindow-padding-x = 80,2\nscrollback-limit = 50000000\n"
            ),
            [
                ("fontSize", "font-size = 64".into(), 32.0),
                ("lineHeight", "adjust-cell-height = -40%".into(), 1.0),
                ("padding", "window-padding-x = 80,2".into(), 32.0),
                ("scrollback", "scrollback-limit = 50000000".into(), 100_000.0),
            ]
        );
        assert_eq!(
            adjusted("font-size = 4\nadjust-cell-height = 300%\nscrollback-limit = 99999\n"),
            [
                ("fontSize", "font-size = 4".into(), 8.0),
                ("lineHeight", "adjust-cell-height = 300%".into(), 2.0),
                ("scrollback", "scrollback-limit = 99999".into(), 1000.0),
            ]
        );
        assert_eq!(
            adjusted("scrollback-limit = 18446744073709551615\n"),
            [("scrollback", "scrollback-limit = 18446744073709551615".into(), 100_000.0)]
        );
    }

    #[test]
    fn boundaries_in_range_and_bad_values_are_not_noted() {
        let text = "font-size = 8\nfont-size = 32\nadjust-cell-height = 0%\nadjust-cell-height = 100%\nwindow-padding-x = 0\n\
                    window-padding-x = 32\nscrollback-limit = 100000\nscrollback-limit = 10000000\nfont-size = 13\n\
                    font-size = 0\nfont-size = big\nadjust-cell-height = 2\nwindow-padding-x = -4\nscrollback-limit = -1\n";
        let import = parse(text);
        assert_eq!(import.adjusted, []);
        assert_eq!(
            import.settings,
            Settings {
                font_size: Some(13.0),
                line_height: Some(2.0),
                padding: Some(32),
                scrollback: Some(100_000),
                ..Settings::default()
            }
        );
        assert_eq!(import.ignored, ["font-size", "adjust-cell-height", "window-padding-x", "scrollback-limit"]);
    }

    #[test]
    fn only_the_applied_value_is_noted() {
        // A later value replaces the note of an earlier one; a later bad value keeps it.
        assert_eq!(adjusted("font-size = 64\nfont-size = 14\n"), []);
        assert_eq!(adjusted("font-size = 64\nfont-size = 4\n"), [("fontSize", "font-size = 4".into(), 8.0)]);
        assert_eq!(adjusted("font-size = 64\nfont-size = big\n"), [("fontSize", "font-size = 64".into(), 32.0)]);
        let files = parse_files(["window-padding-x = 80\nfont-size = 2\n", "window-padding-x = 4\n"]);
        assert_eq!(files.settings.padding, Some(4));
        assert_eq!(files.adjusted, [Adjustment { setting: "fontSize", input: "font-size = 2".into(), value: 8.0 }]);
    }

    #[test]
    fn a_later_file_replaces_the_font_list() {
        let font = |texts: &[&str]| parse_files(texts.iter().copied()).settings.font_family;
        assert_eq!(
            font(&["font-family = A\nfont-family = B\n", "font-family = C\n"]),
            Some(r#""C", monospace"#.into())
        );
        assert_eq!(font(&["font-family = A\nfont-family = B\n"]), Some(r#""A", "B", monospace"#.into()));
        assert_eq!(font(&["font-family = A\n", "font-size = 14\n"]), Some(r#""A", monospace"#.into()));
        assert_eq!(font(&["font-family = A\n", "font-family =\n"]), None);
    }

    #[test]
    fn reads_the_files_in_order_and_skips_folders() {
        let home = crate::test_dir::root().join(format!("zwinger-ghostty-{}", uuid::Uuid::new_v4()));
        assert!(matches!(import(&home), Err(Error::MissingPath(_))));
        let [app, xdg, agterm] = config_paths(&home);
        for path in [&app, &xdg] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&app, "font-size = 12\ncursor-style = bar\nfont-family = A\n").unwrap();
        std::fs::write(&xdg, "font-size = 15\nfont-family = B\n").unwrap();
        std::fs::create_dir_all(&agterm).unwrap();
        let import = import(&home).unwrap();
        assert_eq!(
            import.settings,
            Settings {
                font_family: Some(r#""B", monospace"#.into()),
                font_size: Some(15.0),
                cursor_style: Some(CursorStyle::Bar),
                ..Settings::default()
            }
        );
    }
}
