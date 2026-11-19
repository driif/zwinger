//! Editors: the IDEs on this Mac that open a host folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cage::Members;
use crate::cage::manager::task_guest_folder;
use crate::preferences::EditorPrefs;

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

/// The `Contents/Resources` file that the bundle's `Info.plist` names in
/// `CFBundleIconFile`, also when that file is not there. `None` if the plist
/// is unreadable, is no regular file in the bundle, or the name is not a plain
/// file name. A bundle with only an asset catalog (`CFBundleIconName`) has none.
pub fn declared_icon(bundle: &Path) -> Option<PathBuf> {
    let root = bundle.canonicalize().ok()?;
    let plist = root.join("Contents/Info.plist").canonicalize().ok()?;
    if !plist.starts_with(&root) || !plist.is_file() {
        return None;
    }
    let info = plist::Value::from_file(plist).ok()?;
    let name = info.as_dictionary()?.get("CFBundleIconFile")?.as_string()?;
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return None;
    }
    let file = if Path::new(name).extension().is_some() { name.to_owned() } else { format!("{name}.icns") };
    Some(bundle.join("Contents/Resources").join(file))
}

/// The [`declared_icon`] file, resolved. `None` if it is not a regular file in
/// the bundle's `Contents/Resources`. A link that stays in that folder is valid.
pub fn icon_source(bundle: &Path) -> Option<PathBuf> {
    let declared = declared_icon(bundle)?;
    let root = bundle.canonicalize().ok()?;
    let resources = bundle.join("Contents/Resources").canonicalize().ok()?;
    let path = declared.canonicalize().ok()?;
    (resources.starts_with(&root) && path.starts_with(&resources) && path.is_file()).then_some(path)
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

/// True if `id` is in the [`KNOWN`] table.
pub fn is_known(id: &str) -> bool {
    KNOWN.iter().any(|k| k.id == id)
}

/// The editor to open: `requested` if it is on, else the `project` override
/// if it is on, else the default. The flag is true when the user asked for
/// an editor and a different one opens. A saved default or override that is
/// off or gone sets no flag: the launchers already show its replacement as
/// the default.
pub fn resolve(
    prefs: &EditorPrefs,
    detected: &[Editor],
    requested: Option<&str>,
    project: Option<&str>,
) -> Option<(Editor, bool)> {
    let on = enabled(prefs, detected);
    let find = |id: Option<&str>| on.iter().copied().find(|e| Some(e.id.as_str()) == id);
    if let Some(editor) = find(requested) {
        return Some((editor.clone(), false));
    }
    let editor = find(project).or_else(|| default_of(prefs, detected))?;
    Some((editor.clone(), requested.is_some_and(|id| id != editor.id)))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorView {
    #[serde(flatten)]
    pub editor: Editor,
    pub enabled: bool,
    /// The app icon as a `data:image/png` URL. `None`: the UI shows a generic icon.
    pub icon: Option<String>,
}

/// The detected editors and the default, as the launchers show them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Editors {
    pub editors: Vec<EditorView>,
    pub default: Option<String>,
    /// The name of each known editor, also one that is not installed, so a saved choice shows its name.
    pub names: BTreeMap<&'static str, &'static str>,
}

pub fn view(prefs: &EditorPrefs, detected: Vec<Editor>) -> Editors {
    let on: Vec<String> = enabled(prefs, &detected).into_iter().map(|e| e.id.clone()).collect();
    let default = default_of(prefs, &detected).map(|e| e.id.clone());
    let editors = detected
        .into_iter()
        .map(|editor| EditorView { enabled: on.contains(&editor.id), editor, icon: None })
        .collect();
    Editors { editors, default, names: KNOWN.iter().map(|k| (k.id, k.name)).collect() }
}

/// The host folder of a guest folder below the guest folder of a member
/// ([`task_guest_folder`]) or a mount of the VM. `None` for any other path,
/// a path that is gone on the host, or a path whose `..` or link leads out
/// of its host folder.
pub fn map_guest(guest: &str, members: Members<'_>) -> Option<PathBuf> {
    let below = |root: &str| guest.strip_prefix(root).filter(|rest| rest.is_empty() || rest.starts_with('/'));
    let folders = members.sessions.iter().filter_map(|s| {
        let (_, root, _) = task_guest_folder(members.sessions, &s.id)?;
        Some((s.folder.as_path(), root))
    });
    let (host, rest) = folders
        .chain(members.cage.mounts.iter().map(|m| (m.host.as_path(), m.guest.clone())))
        .filter_map(|(host, root)| Some((host, root.len(), below(&root)?)))
        .max_by_key(|(_, len, _)| *len)
        .map(|(host, _, rest)| (host, rest))?;
    let base = host.canonicalize().ok()?;
    let path = base.join(rest.trim_start_matches('/')).canonicalize().ok()?;
    (path.starts_with(&base) && path.is_dir()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cage::{Cage, Mount};
    use crate::session::Session;

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

    /// A bundle in a new folder below `root` with `info` as its `Info.plist`, and its `Resources` folder.
    fn make_bundle(root: &Path, info: &plist::Dictionary, binary: bool) -> (PathBuf, PathBuf) {
        let bundle = root.join(uuid::Uuid::new_v4().to_string()).join("Editor.app");
        let resources = bundle.join("Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        let file = bundle.join("Contents/Info.plist");
        let value = plist::Value::Dictionary(info.clone());
        if binary { value.to_file_binary(file) } else { value.to_file_xml(file) }.unwrap();
        (bundle, resources)
    }

    fn info(icon: &str) -> plist::Dictionary {
        let mut info = plist::Dictionary::new();
        info.insert("CFBundleIconFile".into(), icon.into());
        info
    }

    #[test]
    fn the_icon_source_is_the_named_file_in_resources() {
        let root = temp();
        for binary in [false, true] {
            let (bundle, resources) = make_bundle(&root, &info("Code.icns"), binary);
            std::fs::write(resources.join("Code.icns"), "icns").unwrap();
            assert_eq!(icon_source(&bundle), Some(resources.join("Code.icns")), "binary {binary}");
        }
        // Xcode names its icon with no extension.
        let (bundle, resources) = make_bundle(&root, &info("Xcode"), false);
        std::fs::write(resources.join("Xcode.icns"), "icns").unwrap();
        assert_eq!(icon_source(&bundle), Some(resources.join("Xcode.icns")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_bad_plist_name_or_file_gives_no_icon_source() {
        let root = temp();
        // A missing icon file, or a folder with the icon's name.
        let (bundle, resources) = make_bundle(&root, &info("Gone.icns"), false);
        assert_eq!(icon_source(&bundle), None);
        std::fs::create_dir(resources.join("Gone.icns")).unwrap();
        assert_eq!(icon_source(&bundle), None);
        // Only an asset catalog.
        let mut catalog = plist::Dictionary::new();
        catalog.insert("CFBundleIconName".into(), "AppIcon".into());
        assert_eq!(icon_source(&make_bundle(&root, &catalog, false).0), None);
        // A malformed plist.
        let (bundle, resources) = make_bundle(&root, &info("Code.icns"), false);
        std::fs::write(resources.join("Code.icns"), "icns").unwrap();
        std::fs::write(bundle.join("Contents/Info.plist"), "<plist><dict><key>").unwrap();
        assert_eq!(icon_source(&bundle), None);
        // A name with a separator or `..`, also when the file is there, or a link out of Resources.
        for name in ["../Info.plist", "..", "/etc/hosts", "sub/Code.icns", "Code..icns", ""] {
            let (bundle, resources) = make_bundle(&root, &info(name), false);
            std::fs::create_dir(resources.join("sub")).unwrap();
            std::fs::write(resources.join("sub/Code.icns"), "icns").unwrap();
            std::fs::write(resources.join("Code..icns"), "icns").unwrap();
            assert_eq!(icon_source(&bundle), None, "{name}");
        }
        let (bundle, resources) = make_bundle(&root, &info("Link.icns"), false);
        std::os::unix::fs::symlink(bundle.join("Contents/Info.plist"), resources.join("Link.icns")).unwrap();
        assert_eq!(icon_source(&bundle), None);
        // A link in Resources that stays there is valid.
        std::fs::write(resources.join("Real.icns"), "icns").unwrap();
        std::fs::remove_file(resources.join("Link.icns")).unwrap();
        std::os::unix::fs::symlink("Real.icns", resources.join("Link.icns")).unwrap();
        assert_eq!(icon_source(&bundle), Some(resources.join("Real.icns")));
        // An Info.plist or Resources that links out of the bundle.
        let (outside, _) = make_bundle(&root, &info("Code.icns"), false);
        std::fs::write(outside.join("Contents/Resources/Code.icns"), "icns").unwrap();
        let (bundle, resources) = make_bundle(&root, &info("Code.icns"), false);
        std::fs::write(resources.join("Code.icns"), "icns").unwrap();
        std::fs::remove_file(bundle.join("Contents/Info.plist")).unwrap();
        std::os::unix::fs::symlink(outside.join("Contents/Info.plist"), bundle.join("Contents/Info.plist")).unwrap();
        assert_eq!(declared_icon(&bundle), None);
        let (bundle, resources) = make_bundle(&root, &info("Code.icns"), false);
        std::fs::remove_dir(&resources).unwrap();
        std::os::unix::fs::symlink(outside.join("Contents/Resources"), &resources).unwrap();
        assert_eq!(icon_source(&bundle), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unset_prefs_turn_all_on_and_an_empty_list_turns_all_off() {
        let all = [editor("vscode"), editor("zed")];
        assert_eq!(enabled(&prefs(None, None), &all).len(), 2);
        assert!(enabled(&prefs(Some(&[]), None), &all).is_empty());
        assert_eq!(resolve(&prefs(Some(&[]), Some("zed")), &all, Some("zed"), Some("zed")), None);
        let view = view(&prefs(Some(&["vscode"]), None), all.to_vec());
        assert_eq!(view.default.as_deref(), Some("vscode"));
        assert_eq!(view.editors.iter().map(|e| e.enabled).collect::<Vec<_>>(), [true, false]);
        let json = serde_json::to_value(&view.editors[0]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "id": "vscode", "name": "vscode", "path": "/Applications/vscode.app", "enabled": true, "icon": null })
        );
        // A known editor that is not installed still has its name.
        assert_eq!(view.names.get("nova"), Some(&"Nova"));
        assert_eq!(view.names.len(), KNOWN.len());
    }

    #[test]
    fn resolve_falls_back_and_says_so() {
        let all = [editor("vscode"), editor("zed")];
        let resolved = |p: &EditorPrefs, requested| resolve(p, &all, requested, None).map(|(e, fell)| (e.id, fell));
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
    fn a_project_override_opens_before_the_app_default() {
        let all = [editor("cursor"), editor("vscode"), editor("zed")];
        let open =
            |p: &EditorPrefs, requested, project| resolve(p, &all, requested, project).map(|(e, fell)| (e.id, fell));
        let app = prefs(None, Some("zed"));
        // Two projects of one cage with different overrides.
        assert_eq!(open(&app, None, Some("cursor")), Some(("cursor".into(), false)));
        assert_eq!(open(&app, None, Some("vscode")), Some(("vscode".into(), false)));
        assert_eq!(open(&app, None, None), Some(("zed".into(), false)));
        // An override that is off or not installed gives way to the app default, with no flag.
        assert_eq!(
            open(&prefs(Some(&["zed", "vscode"]), Some("zed")), None, Some("cursor")),
            Some(("zed".into(), false))
        );
        assert_eq!(open(&app, None, Some("nova")), Some(("zed".into(), false)));
        // A changed app default applies to a project with an unavailable override.
        assert_eq!(open(&prefs(None, Some("vscode")), None, Some("nova")), Some(("vscode".into(), false)));
        assert_eq!(open(&prefs(None, Some("vscode")), None, Some("cursor")), Some(("cursor".into(), false)));
        // No editor is on.
        assert_eq!(open(&prefs(Some(&[]), Some("zed")), None, Some("cursor")), None);
        // An explicit request wins over the override.
        assert_eq!(open(&app, Some("vscode"), Some("cursor")), Some(("vscode".into(), false)));
        // A request that is not on falls back to the override and says so.
        assert_eq!(open(&app, Some("nova"), Some("cursor")), Some(("cursor".into(), true)));
        assert_eq!(open(&app, Some("cursor"), Some("cursor")), Some(("cursor".into(), false)));
    }

    #[test]
    fn only_known_editors_can_be_a_project_choice() {
        let dir = temp();
        let mut session = Session::new(dir.to_str().unwrap(), None, []).unwrap();
        assert_eq!(session.editor, None);
        // A known editor is kept also when it is not installed.
        session.set_editor(Some("nova")).unwrap();
        assert_eq!(session.editor.as_deref(), Some("nova"));
        assert!(matches!(session.set_editor(Some("notepad")), Err(crate::error::Error::UnknownEditor(_))));
        assert_eq!(session.editor.as_deref(), Some("nova"));
        session.set_editor(None).unwrap();
        assert_eq!(session.editor, None);
    }

    #[test]
    fn guest_paths_map_to_the_session_folder_or_a_mount_and_never_out() {
        let root = temp();
        let folder = root.join("project");
        let data = root.join("data");
        std::fs::create_dir_all(folder.join("src/deep")).unwrap();
        std::fs::create_dir_all(data.join("sets")).unwrap();
        std::fs::create_dir_all(root.join("secret")).unwrap();
        std::os::unix::fs::symlink(root.join("secret"), folder.join("link")).unwrap();
        std::os::unix::fs::symlink(folder.join("src"), folder.join("inside")).unwrap();
        std::fs::write(folder.join("file"), "").unwrap();
        let session = Session::new(folder.to_str().unwrap(), Some("c".into()), []).unwrap();
        let mut cage = Cage::new("c".into(), session.name.clone());
        cage.mounts.push(Mount { host: data.clone(), id: None, guest: "/data".into(), readonly: true });

        let sessions = [session];
        let map = |guest: &str| map_guest(guest, Members { sessions: &sessions, cage: &cage });
        assert_eq!(map("/workspace"), Some(folder.clone()));
        assert_eq!(map("/workspace/"), Some(folder.clone()));
        assert_eq!(map("/workspace/src/deep"), Some(folder.join("src/deep")));
        assert_eq!(map("/workspace/inside"), Some(folder.join("src")));
        assert_eq!(map("/data/sets"), Some(data.join("sets")));
        assert_eq!(map("/workspace/src/../src/deep"), Some(folder.join("src/deep")));
        // Out of the project, gone, a file or a different root.
        for guest in ["/workspace/..", "/workspace/../secret", "/workspace/link", "/data/../project", "/workspace/gone"]
        {
            assert_eq!(map(guest), None, "{guest}");
        }
        for guest in ["/workspace/file", "/workspaces", "/tmp", "/", "", "workspace/src", "/datax"] {
            assert_eq!(map(guest), None, "{guest}");
        }
    }

    #[test]
    fn with_two_members_each_maps_below_its_slug() {
        let root = temp();
        let (a, b) = (root.join("alpha"), root.join("beta"));
        std::fs::create_dir_all(a.join("src")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let first = Session::new(a.to_str().unwrap(), Some("c".into()), []).unwrap();
        let second = Session::new(b.to_str().unwrap(), Some("c".into()), [first.slug.as_str()]).unwrap();
        let cage = Cage::new("c".into(), "c".into());
        let sessions = [first, second];
        let map = |guest: &str| map_guest(guest, Members { sessions: &sessions, cage: &cage });
        assert_eq!(map("/workspace/alpha/src"), Some(a.join("src")));
        assert_eq!(map("/workspace/beta"), Some(b.clone()));
        // `/workspace` itself is no session folder in a shared VM.
        for guest in ["/workspace", "/workspace/src", "/workspace/alphax"] {
            assert_eq!(map(guest), None, "{guest}");
        }
    }
}
