//! Editors: the IDEs on this Mac that open a host folder.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::preferences::EditorPrefs;
use crate::shield::{Shield, WORKSPACE};

/// A known editor and the names of its app bundles, the stable one first.
struct Known {
    id: &'static str,
    name: &'static str,
    bundles: &'static [&'static str],
}

/// The detection and the launchers keep this order.
const KNOWN: &[Known] = &[
    Known { id: "zed", name: "Zed", bundles: &["Zed.app", "Zed Preview.app"] },
    Known { id: "vscode", name: "VS Code", bundles: &["Visual Studio Code.app", "Visual Studio Code - Insiders.app"] },
    Known { id: "cursor", name: "Cursor", bundles: &["Cursor.app"] },
    Known { id: "windsurf", name: "Windsurf", bundles: &["Windsurf.app"] },
    Known { id: "vscodium", name: "VSCodium", bundles: &["VSCodium.app"] },
    Known { id: "sublime", name: "Sublime Text", bundles: &["Sublime Text.app"] },
    Known { id: "fleet", name: "Fleet", bundles: &["Fleet.app"] },
    Known {
        id: "intellij",
        name: "IntelliJ IDEA",
        bundles: &["IntelliJ IDEA.app", "IntelliJ IDEA Ultimate.app", "IntelliJ IDEA CE.app"],
    },
    Known { id: "webstorm", name: "WebStorm", bundles: &["WebStorm.app"] },
    Known { id: "rustrover", name: "RustRover", bundles: &["RustRover.app"] },
    Known {
        id: "pycharm",
        name: "PyCharm",
        bundles: &["PyCharm.app", "PyCharm Professional Edition.app", "PyCharm CE.app"],
    },
    Known { id: "goland", name: "GoLand", bundles: &["GoLand.app"] },
    Known { id: "xcode", name: "Xcode", bundles: &["Xcode.app"] },
    Known { id: "nova", name: "Nova", bundles: &["Nova.app"] },
];

/// The default when the user chose none, in this order.
const PREFERRED: [&str; 2] = ["zed", "vscode"];

/// An installed editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Editor {
    pub id: String,
    pub name: String,
    /// The app bundle.
    pub path: PathBuf,
}

/// The folders that hold the apps. No login shell and no `PATH`.
pub fn app_dirs(home: &Path) -> [PathBuf; 3] {
    [PathBuf::from("/Applications"), home.join("Applications"), PathBuf::from("/Applications/Setapp")]
}

/// The known editors that have a bundle in `dirs`, in [`KNOWN`] order.
pub fn detect(dirs: &[PathBuf]) -> Vec<Editor> {
    KNOWN
        .iter()
        .filter_map(|known| {
            let path = known.bundles.iter().flat_map(|b| dirs.iter().map(move |d| d.join(b))).find(|p| p.is_dir())?;
            Some(Editor { id: known.id.to_owned(), name: known.name.to_owned(), path })
        })
        .collect()
}

/// The detected editors that the user has on. Unset prefs turn each one on.
fn enabled<'a>(prefs: &EditorPrefs, detected: &'a [Editor]) -> Vec<&'a Editor> {
    detected.iter().filter(|e| prefs.enabled.as_ref().is_none_or(|ids| ids.contains(&e.id))).collect()
}

/// The chosen default if it is on, else Zed, else VS Code, else the first one that is on.
fn default_of<'a>(prefs: &EditorPrefs, detected: &'a [Editor]) -> Option<&'a Editor> {
    let on = enabled(prefs, detected);
    let find = |id: &str| on.iter().copied().find(|e| e.id == id);
    prefs
        .default
        .as_deref()
        .and_then(find)
        .or_else(|| PREFERRED.iter().find_map(|id| find(id)))
        .or_else(|| on.first().copied())
}

/// The editor to open: `requested` if it is on, else the default. The flag
/// is true when the user asked for an editor and a different one opens. A
/// saved default that is off or gone sets no flag: [`view`] already shows
/// its replacement as the default.
pub fn resolve(prefs: &EditorPrefs, detected: &[Editor], requested: Option<&str>) -> Option<(Editor, bool)> {
    if let Some(editor) = requested.and_then(|id| enabled(prefs, detected).into_iter().find(|e| e.id == id)) {
        return Some((editor.clone(), false));
    }
    let editor = default_of(prefs, detected)?;
    Some((editor.clone(), requested.is_some_and(|id| id != editor.id)))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorView {
    #[serde(flatten)]
    pub editor: Editor,
    pub enabled: bool,
}

/// The detected editors and the default, as the launchers show them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Editors {
    pub editors: Vec<EditorView>,
    pub default: Option<String>,
}

pub fn view(prefs: &EditorPrefs, detected: Vec<Editor>) -> Editors {
    let on: Vec<String> = enabled(prefs, &detected).into_iter().map(|e| e.id.clone()).collect();
    let default = default_of(prefs, &detected).map(|e| e.id.clone());
    let editors = detected.into_iter().map(|editor| EditorView { enabled: on.contains(&editor.id), editor }).collect();
    Editors { editors, default }
}

/// The host folder of a guest folder below `/workspace` or a mount of
/// `shield`. `None` for any other path, a path that is gone on the host, or
/// a path whose `..` or link leads out of its host folder.
pub fn map_guest(guest: &str, shield: &Shield) -> Option<PathBuf> {
    let below = |root: &str| guest.strip_prefix(root).filter(|rest| rest.is_empty() || rest.starts_with('/'));
    let (host, rest) = std::iter::once((shield.folder.as_path(), WORKSPACE))
        .chain(shield.mounts.iter().map(|m| (m.host.as_path(), m.guest.as_str())))
        .filter_map(|(host, root)| Some((host, root.len(), below(root)?)))
        .max_by_key(|(_, len, _)| *len)
        .map(|(host, _, rest)| (host, rest))?;
    let base = host.canonicalize().ok()?;
    let path = base.join(rest.trim_start_matches('/')).canonicalize().ok()?;
    (path.starts_with(&base) && path.is_dir()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shield::Mount;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-editor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn editor(id: &str) -> Editor {
        Editor { id: id.into(), name: id.into(), path: PathBuf::from(format!("/Applications/{id}.app")) }
    }

    fn prefs(enabled: Option<&[&str]>, default: Option<&str>) -> EditorPrefs {
        EditorPrefs {
            enabled: enabled.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect()),
            default: default.map(str::to_owned),
        }
    }

    #[test]
    fn detection_finds_bundles_in_each_folder_in_table_order() {
        let (system, user, setapp) = (temp(), temp(), temp());
        std::fs::create_dir(system.join("Cursor.app")).unwrap();
        std::fs::create_dir(user.join("Zed Preview.app")).unwrap();
        std::fs::create_dir(setapp.join("Nova.app")).unwrap();
        std::fs::create_dir(system.join("Unknown.app")).unwrap();
        // A file is no bundle.
        std::fs::write(system.join("Xcode.app"), "").unwrap();

        let found = detect(&[system.clone(), user.clone(), setapp.clone()]);
        let ids: Vec<&str> = found.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["zed", "cursor", "nova"]);
        assert_eq!(found[0], Editor { id: "zed".into(), name: "Zed".into(), path: user.join("Zed Preview.app") });

        // The stable bundle comes before the preview.
        std::fs::create_dir(setapp.join("Zed.app")).unwrap();
        assert_eq!(detect(&[system, user, setapp.clone()])[0].path, setapp.join("Zed.app"));
        assert!(detect(&[temp()]).is_empty());
        assert_eq!(app_dirs(Path::new("/Users/me"))[1], Path::new("/Users/me/Applications"));
    }

    #[test]
    fn the_default_is_the_choice_else_zed_else_vs_code_else_the_first() {
        let all = [editor("cursor"), editor("vscode"), editor("zed")];
        let id = |prefs: &EditorPrefs, detected: &[Editor]| default_of(prefs, detected).map(|e| e.id.clone());
        assert_eq!(id(&prefs(None, None), &all).as_deref(), Some("zed"));
        assert_eq!(id(&prefs(None, None), &all[..2]).as_deref(), Some("vscode"));
        assert_eq!(id(&prefs(None, None), &all[..1]).as_deref(), Some("cursor"));
        assert_eq!(id(&prefs(None, Some("cursor")), &all).as_deref(), Some("cursor"));
        // A default that is off or gone gives way.
        assert_eq!(id(&prefs(Some(&["vscode"]), Some("cursor")), &all).as_deref(), Some("vscode"));
        assert_eq!(id(&prefs(None, Some("nova")), &all).as_deref(), Some("zed"));
    }

    #[test]
    fn unset_prefs_turn_all_on_and_an_empty_list_turns_all_off() {
        let all = [editor("vscode"), editor("zed")];
        assert_eq!(enabled(&prefs(None, None), &all).len(), 2);
        assert!(enabled(&prefs(Some(&[]), None), &all).is_empty());
        assert_eq!(resolve(&prefs(Some(&[]), Some("zed")), &all, Some("zed")), None);
        let view = view(&prefs(Some(&["vscode"]), None), all.to_vec());
        assert_eq!(view.default.as_deref(), Some("vscode"));
        assert_eq!(view.editors.iter().map(|e| e.enabled).collect::<Vec<_>>(), [true, false]);
        let json = serde_json::to_value(&view.editors[0]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "id": "vscode", "name": "vscode", "path": "/Applications/vscode.app", "enabled": true })
        );
    }

    #[test]
    fn resolve_falls_back_and_says_so() {
        let all = [editor("vscode"), editor("zed")];
        let resolved = |p: &EditorPrefs, requested| resolve(p, &all, requested).map(|(e, fell)| (e.id, fell));
        assert_eq!(resolved(&prefs(None, None), Some("vscode")), Some(("vscode".into(), false)));
        assert_eq!(resolved(&prefs(None, None), None), Some(("zed".into(), false)));
        assert_eq!(resolved(&prefs(None, Some("zed")), None), Some(("zed".into(), false)));
        // A saved default that is off or gone, with no request, is no fallback.
        assert_eq!(resolved(&prefs(None, Some("cursor")), None), Some(("zed".into(), false)));
        assert_eq!(resolved(&prefs(Some(&["zed"]), Some("vscode")), None), Some(("zed".into(), false)));
        // A request for an editor that is off or gone falls back.
        assert_eq!(resolved(&prefs(Some(&["zed"]), None), Some("vscode")), Some(("zed".into(), true)));
        assert_eq!(resolved(&prefs(None, None), Some("gone")), Some(("zed".into(), true)));
        // A request for the effective default is no fallback.
        assert_eq!(resolved(&prefs(Some(&["zed"]), Some("vscode")), Some("zed")), Some(("zed".into(), false)));
    }

    #[test]
    fn guest_paths_map_to_the_shield_folder_or_a_mount_and_never_out() {
        let root = temp();
        let folder = root.join("project");
        let data = root.join("data");
        std::fs::create_dir_all(folder.join("src/deep")).unwrap();
        std::fs::create_dir_all(data.join("sets")).unwrap();
        std::fs::create_dir_all(root.join("secret")).unwrap();
        std::os::unix::fs::symlink(root.join("secret"), folder.join("link")).unwrap();
        std::os::unix::fs::symlink(folder.join("src"), folder.join("inside")).unwrap();
        std::fs::write(folder.join("file"), "").unwrap();
        let mut shield = Shield::new(folder.to_str().unwrap()).unwrap();
        shield.mounts.push(Mount { host: data.clone(), id: None, guest: "/data".into(), readonly: true });

        let map = |guest: &str| map_guest(guest, &shield);
        assert_eq!(map("/workspace"), Some(folder.clone()));
        assert_eq!(map("/workspace/"), Some(folder.clone()));
        assert_eq!(map("/workspace/src/deep"), Some(folder.join("src/deep")));
        assert_eq!(map("/workspace/inside"), Some(folder.join("src")));
        assert_eq!(map("/data/sets"), Some(data.join("sets")));
        assert_eq!(map("/workspace/src/../src/deep"), Some(folder.join("src/deep")));
        // Out of the shield, gone, a file or a different root.
        for guest in ["/workspace/..", "/workspace/../secret", "/workspace/link", "/data/../project", "/workspace/gone"]
        {
            assert_eq!(map(guest), None, "{guest}");
        }
        for guest in ["/workspace/file", "/workspaces", "/tmp", "/", "", "workspace/src", "/datax"] {
            assert_eq!(map(guest), None, "{guest}");
        }
    }
}
