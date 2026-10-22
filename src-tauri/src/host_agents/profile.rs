//! The agent profile: a host-owned snapshot of the allow-listed entries of an
//! agent's config dir, updated in place at each shield start in
//! `<app_data>/profiles/<shield>/<agent>` and mounted read-only at
//! `/opt/zwinger/profile/<agent>`. The agent's state home in the guest gets
//! links into that mount. Only listed entries are copied, and a denied
//! name is skipped at any depth, so logins, history, sessions and projects
//! never are.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent::AgentKind;

/// The guest folder of the snapshots.
pub const GUEST_DIR: &str = "/opt/zwinger/profile";
/// The bytes and the files that a snapshot can hold; the rest is skipped.
const SIZE_LIMIT: u64 = 256 << 20;
const FILE_LIMIT: usize = 20_000;
const DEPTH_LIMIT: usize = 16;

/// The config dir of the agent, below the host home.
pub fn config_dir(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => ".claude",
        AgentKind::Codex => ".codex",
        AgentKind::Pi => ".pi/agent",
        AgentKind::Gemini => ".gemini",
    }
}

/// The entries that the profile copies. Never a login, history or session.
pub fn entries(kind: AgentKind) -> &'static [&'static str] {
    match kind {
        AgentKind::Claude => &["settings.json", "CLAUDE.md", "skills", "commands", "agents", "plugins"],
        AgentKind::Codex => &["config.toml", "AGENTS.md", "prompts", "skills"],
        AgentKind::Pi => &["settings.json", "extensions", "themes", "models-store.json", "npm"],
        AgentKind::Gemini => &["settings.json", "GEMINI.md", "commands", "policies"],
    }
}

/// Names that are never copied, at any depth, also through a link: logins,
/// history, sessions and the data of other projects. Compared in lowercase,
/// as the Mac's filesystem ignores case.
fn is_denied(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "projects"
            | "projects.json"
            | "sessions"
            | "todos"
            | "shell-snapshots"
            | "statsig"
            | "auth.json"
            | "oauth_creds.json"
            | "google_accounts.json"
            | "trust.json"
    ) || ["history", ".credentials", "antigravity"].iter().any(|prefix| name.starts_with(prefix))
}

/// True if a path below an entry has a denied name. Below `node_modules`
/// (pi's `npm`), package folders keep their names, such as the `history`
/// package, but files with denied names are still refused.
fn refused(rel: &Path, last_is_dir: bool) -> bool {
    let parts: Vec<_> = rel.iter().map(|part| part.to_string_lossy()).collect();
    let mut package = false;
    for (i, part) in parts.iter().enumerate() {
        let dir = i + 1 < parts.len() || last_is_dir;
        if is_denied(part) && !(package && dir) {
            return true;
        }
        package |= part == "node_modules";
    }
    false
}

/// The folders of the agent's config dir in the guest, below the state dir
/// (`/opt/zwinger/state`).
pub fn state_home(kind: AgentKind) -> &'static [&'static str] {
    match kind {
        AgentKind::Claude => &["claude", ".claude"],
        AgentKind::Codex => &["codex"],
        AgentKind::Pi => &["agent"],
        AgentKind::Gemini => &["gemini", ".gemini"],
    }
}

pub fn guest_dir(kind: AgentKind) -> String {
    format!("{GUEST_DIR}/{}", kind.name())
}

/// What a snapshot copied and skipped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The top-level entries in the snapshot.
    pub entries: Vec<String>,
    /// The host paths that were skipped: links out of their top-level entry,
    /// denied names, and files over the limits. A folder after the file
    /// limit counts once.
    pub skipped: Vec<String>,
    /// The files that this snapshot wrote: new or changed ones.
    pub written: usize,
}

struct Copy<'a> {
    /// The canonical config dir.
    root: PathBuf,
    kind: AgentKind,
    home: &'a Path,
    size: u64,
    files: usize,
    /// A file was refused for the size limit.
    full: bool,
    skipped: Vec<String>,
    written: usize,
}

/// Updates `dest` to hold the allow-listed entries of the agent's config dir
/// below `home`. A file is written only if it is new or its size or mtime
/// changed; an entry that the source has no more, or that is now skipped,
/// is removed. A link in `dest` is removed, not followed. After an error
/// `dest` is removed and copied again in full once; if that fails too, `dest`
/// stays removed and the error is returned. A link in the
/// source is copied as its target if the target stays below the canonical
/// top-level entry that the link is in; a top-level link must point below
/// the config dir.
pub fn snapshot(kind: AgentKind, home: &Path, dest: &Path) -> io::Result<Snapshot> {
    let parent = dest.parent().expect("snapshot dir has a parent");
    fs::create_dir_all(parent)?;
    let result = copy_entries(kind, home, dest).or_else(|_| {
        remove(dest)?;
        copy_entries(kind, home, dest)
    });
    if result.is_err() {
        let _ = remove(dest);
    }
    result
}

fn copy_entries(kind: AgentKind, home: &Path, dest: &Path) -> io::Result<Snapshot> {
    make_dir(dest)?;
    let config = home.join(config_dir(kind));
    let Ok(root) = config.canonicalize() else {
        prune(dest, &HashSet::new())?;
        return Ok(Snapshot::default());
    };
    let mut copy = Copy { root, kind, home, size: 0, files: 0, full: false, skipped: Vec::new(), written: 0 };
    let mut copied = Vec::new();
    for name in entries(kind) {
        let src = config.join(name);
        // The bound of each link below this entry, and the entry's own check.
        let top = match src.canonicalize() {
            Ok(top) => top,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let inside = top.strip_prefix(&copy.root).ok().filter(|rel| !rel.as_os_str().is_empty());
        if inside.is_none_or(|rel| rel.iter().any(|part| is_denied(&part.to_string_lossy()))) {
            copy.skipped.push(src.display().to_string());
            continue;
        }
        if copy.entry(&src, &dest.join(name), name, &top, 0)? {
            copied.push((*name).to_owned());
        }
    }
    prune(dest, &copied.iter().map(OsString::from).collect())?;
    Ok(Snapshot { entries: copied, skipped: copy.skipped, written: copy.written })
}

/// Makes `path` a real folder. A link or a file there is removed first.
fn make_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::create_dir(path)
}

/// Removes the entry at `path` and follows no link.
fn remove(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes the entries of `dir` that are not in `kept`.
fn prune(dir: &Path, kept: &HashSet<OsString>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !kept.contains(&entry.file_name()) {
            remove(&entry.path())?;
        }
    }
    Ok(())
}

impl Copy<'_> {
    /// Copies one entry into `dst`, if it changed. `rel` is its path below
    /// the config dir, and `top` is the canonical top-level entry that it is
    /// in. Returns false if it was skipped or does not exist; the caller then
    /// removes `dst`.
    fn entry(&mut self, src: &Path, dst: &Path, rel: &str, top: &Path, depth: usize) -> io::Result<bool> {
        let meta = match fs::symlink_metadata(src) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let (src, meta) = if meta.is_symlink() {
            match src.canonicalize() {
                Ok(target) if target.starts_with(top) => {
                    let meta = fs::metadata(&target)?;
                    let rest = target.strip_prefix(top).unwrap_or(&target);
                    if refused(rest, meta.is_dir()) {
                        self.skipped.push(src.display().to_string());
                        return Ok(false);
                    }
                    (target, meta)
                }
                _ => {
                    self.skipped.push(src.display().to_string());
                    return Ok(false);
                }
            }
        } else {
            (src.to_owned(), meta)
        };
        if depth > 0 && refused(Path::new(rel), meta.is_dir()) {
            self.skipped.push(src.display().to_string());
            return Ok(false);
        }
        if meta.is_dir() {
            // After a limit, a whole folder is skipped and not walked.
            if depth >= DEPTH_LIMIT || self.files >= FILE_LIMIT || self.full {
                self.skipped.push(src.display().to_string());
                return Ok(false);
            }
            make_dir(dst)?;
            let old: Vec<OsString> = fs::read_dir(dst)?.map(|e| e.map(|e| e.file_name())).collect::<io::Result<_>>()?;
            let mut names: Vec<_> = fs::read_dir(&src)?.flatten().map(|e| e.file_name()).collect();
            names.sort();
            let mut kept = HashSet::new();
            for name in names {
                // The Mac ignores case: a renamed `Foo` to `foo` would find the old file.
                if !old.contains(&name)
                    && let Some(other) = old.iter().find(|o| o.eq_ignore_ascii_case(&name))
                {
                    remove(&dst.join(other))?;
                }
                let rel = format!("{rel}/{}", name.to_string_lossy());
                if self.entry(&src.join(&name), &dst.join(&name), &rel, top, depth + 1)? {
                    kept.insert(name);
                }
            }
            prune(dst, &kept)?;
            return Ok(true);
        }
        if !meta.is_file() {
            return Ok(false);
        }
        if self.files >= FILE_LIMIT || self.size + meta.len() > SIZE_LIMIT {
            self.full |= self.size + meta.len() > SIZE_LIMIT;
            self.skipped.push(src.display().to_string());
            return Ok(false);
        }
        self.files += 1;
        self.size += meta.len();
        let old = match fs::symlink_metadata(dst) {
            Ok(old) if old.is_file() => Some(old),
            Ok(_) => {
                remove(dst)?;
                None
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        match self.filter(rel) {
            // Small files, made again each time: their size differs from the source.
            Some(filter) => {
                let text = filter(self, &fs::read_to_string(&src)?);
                if old.is_none() || fs::read(dst)? != text.as_bytes() {
                    fs::write(dst, text)?;
                    self.written += 1;
                }
            }
            None => {
                let modified = meta.modified()?;
                let same =
                    old.as_ref().is_some_and(|old| old.len() == meta.len() && old.modified().ok() == Some(modified));
                if !same {
                    // The copy takes the source's mode, which can deny a write over it.
                    if old.is_some() {
                        fs::remove_file(dst)?;
                    }
                    fs::copy(&src, dst)?;
                    fs::File::open(dst)?.set_times(fs::FileTimes::new().set_modified(modified))?;
                    self.written += 1;
                }
            }
        }
        Ok(true)
    }

    /// The change that a file gets on its way into the snapshot.
    fn filter(&self, rel: &str) -> Option<fn(&Self, &str) -> String> {
        match (self.kind, rel) {
            (AgentKind::Codex, "config.toml") => Some(|_, text| filter_toml(text)),
            (_, "settings.json" | "models-store.json") => Some(|_, text| filter_json(text)),
            (AgentKind::Claude, "plugins/installed_plugins.json" | "plugins/known_marketplaces.json") => {
                Some(|copy, text| copy.rewrite_paths(text))
            }
            _ => None,
        }
    }

    /// Claude Code keeps the absolute host paths of its plugins. In the guest
    /// they are below the profile mount.
    fn rewrite_paths(&self, text: &str) -> String {
        let Ok(mut value) = serde_json::from_str::<Value>(text) else {
            return text.to_owned();
        };
        let guest = guest_dir(AgentKind::Claude);
        let hosts = [self.home.join(".claude"), self.root.clone()];
        let in_claude = |s: &str| hosts.iter().any(|host| Path::new(s).starts_with(host));
        drop_project_paths(&mut value, &in_claude);
        rewrite(&mut value, &|s: &str| {
            hosts.iter().find_map(|host| {
                let rest = Path::new(s).strip_prefix(host).ok()?;
                Some(Path::new(&guest).join(rest).display().to_string())
            })
        });
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_owned())
    }
}

/// Drops each `projectPath` that is not below Claude Code's folder: the
/// path of a project on the Mac (a plugin installed for one project).
fn drop_project_paths(value: &mut Value, in_claude: &dyn Fn(&str) -> bool) {
    match value {
        Value::Object(map) => {
            map.retain(|key, v| key != "projectPath" || v.as_str().is_some_and(in_claude));
            map.values_mut().for_each(|v| drop_project_paths(v, in_claude));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| drop_project_paths(v, in_claude)),
        _ => {}
    }
}

fn rewrite(value: &mut Value, change: &dyn Fn(&str) -> Option<String>) {
    match value {
        Value::String(s) => {
            if let Some(new) = change(s) {
                *s = new;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rewrite(v, change)),
        Value::Object(map) => map.values_mut().for_each(|v| rewrite(v, change)),
        _ => {}
    }
}

/// A settings key that can hold a credential: environment variables and
/// headers (which often carry keys), credential helpers, and any key that
/// names a key, token, secret, password or credential.
fn is_credential(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['_', '-'], "");
    matches!(
        key.as_str(),
        "env"
            | "headers"
            | "httpheaders"
            | "envhttpheaders"
            | "apikeyhelper"
            | "awsauthrefresh"
            | "awscredentialexport"
            | "otelheadershelper"
    ) || ["apikey", "token", "secret", "password", "credential"].iter().any(|word| key.contains(word))
}

fn strip(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !is_credential(key));
            map.values_mut().for_each(strip);
        }
        Value::Array(items) => items.iter_mut().for_each(strip),
        _ => {}
    }
}

/// Drops the credential keys of a JSON settings file. A file that is not
/// JSON is dropped whole: its content is unknown.
fn filter_json(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(mut value) => {
            strip(&mut value);
            serde_json::to_string_pretty(&value).unwrap_or_default()
        }
        Err(_) => "{}".to_owned(),
    }
}

/// Drops from Codex's `config.toml` the tables and keys that name host
/// paths, host commands or credentials, at any depth: `projects` (the
/// folders of the Mac), `mcp_servers` (commands of the Mac) and each
/// credential key. A file that is not TOML is dropped whole.
fn filter_toml(text: &str) -> String {
    fn strip(table: &mut toml::Table) {
        table.retain(|key, _| key != "projects" && key != "mcp_servers" && !is_credential(key));
        table.iter_mut().for_each(|(_, value)| strip_value(value));
    }
    fn strip_value(value: &mut toml::Value) {
        match value {
            toml::Value::Table(table) => strip(table),
            toml::Value::Array(items) => items.iter_mut().for_each(strip_value),
            _ => {}
        }
    }
    match text.parse::<toml::Table>() {
        Ok(mut table) => {
            strip(&mut table);
            toml::to_string(&table).unwrap_or_default()
        }
        Err(_) => String::new(),
    }
}

/// Points the entries of the agent's state home at the snapshot: each
/// listed entry that the snapshot has becomes a link into the mount. A link
/// into the mount whose entry the snapshot has no more is removed. `state`
/// is the host state dir of the shield, which the guest can write, so no
/// link is followed (see [`super::open_parts`]).
pub fn link(kind: AgentKind, state: &Path, snapshot: &[String]) -> io::Result<()> {
    let dir = super::open_parts(state, state_home(kind))?;
    let guest = guest_dir(kind);
    for name in entries(kind) {
        let target = format!("{guest}/{name}");
        if snapshot.iter().any(|e| e == name) {
            super::replace_with_link(&dir, name, &target)?;
        } else if super::link_at(&dir, name)?.is_some_and(|t| t.starts_with(GUEST_DIR)) {
            super::remove_at(&dir, name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use serde_json::json;

    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-profile-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn copies_only_the_allow_list_and_no_link_out() {
        let home = temp_dir();
        let claude = home.join(".claude");
        fs::create_dir_all(claude.join("skills/review")).unwrap();
        fs::write(claude.join("skills/review/SKILL.md"), "review").unwrap();
        fs::write(claude.join("CLAUDE.md"), "be short").unwrap();
        // Denied names: a login, the history, sessions and projects.
        fs::write(claude.join(".credentials.json"), "secret").unwrap();
        fs::write(claude.join("history.jsonl"), "history").unwrap();
        fs::create_dir_all(claude.join("projects/x")).unwrap();
        fs::create_dir_all(claude.join("sessions")).unwrap();
        // A link out of the config dir, and one that stays in it.
        let outside = temp_dir();
        fs::write(outside.join("id_rsa"), "key").unwrap();
        symlink(&outside, claude.join("skills/out")).unwrap();
        symlink(outside.join("id_rsa"), claude.join("commands")).unwrap();
        symlink(claude.join("skills/review"), claude.join("agents")).unwrap();

        let dest = temp_dir().join("profiles/s1/claude");
        let copied = snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(copied.entries, ["CLAUDE.md", "skills", "agents"]);
        assert_eq!(copied.skipped.len(), 2);
        assert_eq!(names(&dest), ["CLAUDE.md", "agents", "skills"]);
        assert_eq!(names(&dest.join("skills")), ["review"]);
        assert_eq!(fs::read_to_string(dest.join("agents/SKILL.md")).unwrap(), "review");
        assert!(!fs::symlink_metadata(dest.join("agents")).unwrap().is_symlink());

        // Links that stay in the config dir but leave their top-level entry,
        // and denied names at any depth.
        fs::create_dir_all(claude.join("projects/x")).unwrap();
        fs::write(claude.join("projects/x/session.jsonl"), "other project").unwrap();
        symlink(claude.join("projects"), claude.join("skills/review/p")).unwrap();
        symlink(claude.join(".credentials.json"), claude.join("skills/review/c")).unwrap();
        symlink(&claude, claude.join("plugins")).unwrap();
        fs::write(claude.join("skills/review/history.jsonl"), "history").unwrap();
        fs::create_dir_all(claude.join("skills/review/Sessions")).unwrap();
        fs::write(claude.join("skills/review/auth.json"), "{}").unwrap();
        let copied = snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(copied.entries, ["CLAUDE.md", "skills", "agents"]);
        // The two links of before, the five entries in `skills` and again in
        // `agents` (a link to the same folder), and `plugins`.
        assert_eq!(copied.skipped.len(), 2 + 2 * 5 + 1, "{:?}", copied.skipped);
        assert_eq!(names(&dest.join("skills/review")), ["SKILL.md"]);
        assert_eq!(names(&dest.join("agents")), ["SKILL.md"]);
        for name in ["p", "c", "history.jsonl", "Sessions", "auth.json"] {
            fs::remove_file(claude.join("skills/review").join(name))
                .or_else(|_| fs::remove_dir(claude.join("skills/review").join(name)))
                .unwrap();
        }
        fs::remove_file(claude.join("plugins")).unwrap();

        // A second start replaces the snapshot.
        fs::remove_file(claude.join("CLAUDE.md")).unwrap();
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(names(&dest), ["agents", "skills"]);
        assert_eq!(names(dest.parent().unwrap()), ["claude"]);
    }

    #[test]
    fn a_folder_after_the_size_limit_is_skipped_once() {
        let home = temp_dir();
        let claude = home.join(".claude");
        fs::create_dir_all(claude.join("skills/review")).unwrap();
        fs::write(claude.join("skills/review/SKILL.md"), "review").unwrap();
        fs::write(claude.join("skills/review/notes.md"), "notes").unwrap();
        // Sparse, so the test writes no 256 MiB. `CLAUDE.md` comes before `skills`.
        fs::File::create(claude.join("CLAUDE.md")).unwrap().set_len(SIZE_LIMIT + 1).unwrap();

        let dest = temp_dir().join("claude");
        let copied = snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert!(copied.entries.is_empty(), "{:?}", copied.entries);
        let skipped: Vec<String> = [claude.join("CLAUDE.md"), claude.join("skills")]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(copied.skipped, skipped);
    }

    /// A Claude Code config dir with a skill, a read-only file and settings.
    fn claude_home() -> PathBuf {
        let home = temp_dir();
        let claude = home.join(".claude");
        fs::create_dir_all(claude.join("skills/review")).unwrap();
        fs::write(claude.join("skills/review/SKILL.md"), "review").unwrap();
        fs::write(claude.join("CLAUDE.md"), "be short").unwrap();
        fs::write(claude.join("settings.json"), r#"{"model":"opus","env":{"KEY":"sk"}}"#).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(claude.join("CLAUDE.md"), fs::Permissions::from_mode(0o444)).unwrap();
        home
    }

    #[test]
    fn an_unchanged_profile_writes_no_files() {
        let home = claude_home();
        let dest = temp_dir().join("claude");
        assert_eq!(snapshot(AgentKind::Claude, &home, &dest).unwrap().written, 3);
        let again = snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(again.entries, ["settings.json", "CLAUDE.md", "skills"]);
        assert_eq!(again.written, 0);
    }

    #[test]
    fn a_changed_file_is_copied_and_a_removed_one_goes() {
        let home = claude_home();
        let claude = home.join(".claude");
        let dest = temp_dir().join("claude");
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        // Also a read-only file, whose copy is read-only too.
        fs::remove_file(claude.join("CLAUDE.md")).unwrap();
        fs::write(claude.join("CLAUDE.md"), "be shorter").unwrap();
        fs::remove_dir_all(claude.join("skills/review")).unwrap();
        fs::create_dir_all(claude.join("skills/new")).unwrap();
        fs::write(claude.join("skills/new/SKILL.md"), "new").unwrap();

        let copied = snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(copied.written, 2);
        assert_eq!(fs::read_to_string(dest.join("CLAUDE.md")).unwrap(), "be shorter");
        assert_eq!(names(&dest.join("skills")), ["new"]);

        fs::remove_file(claude.join("CLAUDE.md")).unwrap();
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(names(&dest), ["settings.json", "skills"]);
        // No config dir: the snapshot is empty.
        fs::remove_dir_all(&claude).unwrap();
        assert_eq!(snapshot(AgentKind::Claude, &home, &dest).unwrap(), Snapshot::default());
        assert!(names(&dest).is_empty());
    }

    #[test]
    fn a_filtered_file_whose_source_changed_is_made_again() {
        let home = claude_home();
        let dest = temp_dir().join("claude");
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        let settings =
            || serde_json::from_str::<Value>(&fs::read_to_string(dest.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings(), json!({ "model": "opus" }));
        fs::write(home.join(".claude/settings.json"), r#"{"model":"sonnet","env":{"KEY":"sk"}}"#).unwrap();
        assert_eq!(snapshot(AgentKind::Claude, &home, &dest).unwrap().written, 1);
        assert_eq!(settings(), json!({ "model": "sonnet" }));
    }

    #[test]
    fn a_name_that_is_now_denied_is_removed() {
        let home = claude_home();
        let dest = temp_dir().join("claude");
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        // Left by an older snapshot, or a file that is now over a rule.
        fs::write(dest.join("skills/review/auth.json"), "{}").unwrap();
        fs::create_dir_all(dest.join("projects")).unwrap();
        fs::write(home.join(".claude/skills/review/history.jsonl"), "history").unwrap();
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(names(&dest.join("skills/review")), ["SKILL.md"]);
        assert_eq!(names(&dest), ["CLAUDE.md", "settings.json", "skills"]);
    }

    #[test]
    fn a_link_in_the_snapshot_is_replaced_and_not_followed() {
        let home = claude_home();
        let dest = temp_dir().join("claude");
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        let outside = temp_dir();
        fs::write(outside.join("precious"), "host").unwrap();
        fs::remove_file(dest.join("skills/review/SKILL.md")).unwrap();
        symlink(outside.join("precious"), dest.join("skills/review/SKILL.md")).unwrap();
        fs::remove_dir_all(dest.join("skills")).unwrap();
        symlink(&outside, dest.join("skills")).unwrap();
        fs::remove_file(dest.join("settings.json")).unwrap();
        symlink(outside.join("precious"), dest.join("settings.json")).unwrap();

        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(names(&outside), ["precious"]);
        assert_eq!(fs::read_to_string(outside.join("precious")).unwrap(), "host");
        assert!(!fs::symlink_metadata(dest.join("skills")).unwrap().is_symlink());
        assert!(!fs::symlink_metadata(dest.join("settings.json")).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(dest.join("skills/review/SKILL.md")).unwrap(), "review");

        // The snapshot dir itself as a link.
        fs::remove_dir_all(&dest).unwrap();
        symlink(&outside, &dest).unwrap();
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        assert_eq!(names(&outside), ["precious"]);
        assert!(dest.join("skills/review/SKILL.md").is_file());
    }

    #[test]
    fn settings_lose_their_credential_fields() {
        let settings = json!({
            "model": "opus",
            "env": { "ANTHROPIC_API_KEY": "sk" },
            "apiKeyHelper": "/bin/key",
            "permissions": { "allow": ["Bash(ls)"] },
            "mcpServers": { "x": { "command": "x", "headers": { "Authorization": "Bearer t" }, "githubToken": "g" } },
            "security": { "auth": { "selectedType": "oauth-personal" } },
        });
        let filtered: Value = serde_json::from_str(&filter_json(&settings.to_string())).unwrap();
        assert_eq!(
            filtered,
            json!({
                "model": "opus",
                "permissions": { "allow": ["Bash(ls)"] },
                "mcpServers": { "x": { "command": "x" } },
                "security": { "auth": { "selectedType": "oauth-personal" } },
            })
        );
        assert_eq!(filter_json("not json"), "{}");

        let toml = r#"
model = "gpt-5"
experimental_bearer_token = "t"
[projects."/Users/me/work"]
trust_level = "trusted"
[mcp_servers.x]
command = "/Users/me/bin/x"
[mcp_servers.x.env]
KEY = "sk"
[profiles.p]
model = "o3"
api_key = """
line
"""
keys = ["a"]
inline = { env = { A = "B" }, keep = 1 }
dotted.env.KEY = "v"
dotted.ok = 2
[[profiles.p.list]]
http_headers = { A = "B" }
name = "n"
"#;
        let expected = r#"
model = "gpt-5"
[profiles.p]
model = "o3"
keys = ["a"]
inline = { keep = 1 }
dotted.ok = 2
[[profiles.p.list]]
name = "n"
"#;
        assert_eq!(filter_toml(toml).parse::<toml::Table>().unwrap(), expected.parse::<toml::Table>().unwrap());
        assert_eq!(filter_toml("model = [\"open"), "");
    }

    #[test]
    fn plugin_paths_point_into_the_guest_mount() {
        let home = temp_dir();
        let plugins = home.join(".claude/plugins");
        fs::create_dir_all(&plugins).unwrap();
        let installed = json!({ "plugins": { "a@m": [
            { "installPath": home.join(".claude/plugins/cache/m/a/1").display().to_string(), "version": "1", "projectPath": "/Users/me/work/secret-client" },
            { "installPath": home.join(".claude/plugins/cache/m/a/2").display().to_string(), "projectPath": home.join(".claude/plugins/local").display().to_string() },
        ] } });
        fs::write(plugins.join("installed_plugins.json"), installed.to_string()).unwrap();
        let known = json!({ "m": { "installLocation": home.join(".claude/plugins/marketplaces/m").display().to_string(), "source": { "repo": "o/m" } } });
        fs::write(plugins.join("known_marketplaces.json"), known.to_string()).unwrap();

        let dest = temp_dir().join("claude");
        snapshot(AgentKind::Claude, &home, &dest).unwrap();
        let read = |name: &str| {
            serde_json::from_str::<Value>(&fs::read_to_string(dest.join("plugins").join(name)).unwrap()).unwrap()
        };
        let plugins = read("installed_plugins.json")["plugins"]["a@m"].clone();
        assert_eq!(
            plugins[0],
            json!({ "installPath": "/opt/zwinger/profile/claude/plugins/cache/m/a/1", "version": "1" })
        );
        assert_eq!(plugins[1]["projectPath"], "/opt/zwinger/profile/claude/plugins/local");
        assert_eq!(
            read("known_marketplaces.json")["m"]["installLocation"],
            "/opt/zwinger/profile/claude/plugins/marketplaces/m"
        );
        assert_eq!(read("known_marketplaces.json")["m"]["source"]["repo"], "o/m");
    }

    #[test]
    fn package_code_keeps_its_names() {
        let home = temp_dir();
        let npm = home.join(".pi/agent/npm/node_modules/history");
        fs::create_dir_all(&npm).unwrap();
        fs::write(npm.join("index.js"), "x").unwrap();
        fs::write(home.join(".pi/agent/auth.json"), "secret").unwrap();
        let dest = temp_dir().join("pi");
        let copied = snapshot(AgentKind::Pi, &home, &dest).unwrap();
        assert_eq!(copied.entries, ["npm"]);
        assert!(dest.join("npm/node_modules/history/index.js").is_file());
    }

    #[test]
    fn package_code_keeps_no_logins() {
        let home = temp_dir();
        let npm = home.join(".pi/agent/npm/node_modules/dep");
        fs::create_dir_all(npm.join("sessions")).unwrap();
        fs::write(npm.join("index.js"), "x").unwrap();
        fs::write(npm.join("auth.json"), "secret").unwrap();
        fs::write(npm.join(".credentials.json"), "secret").unwrap();
        fs::write(npm.join("history.jsonl"), "history").unwrap();
        fs::write(npm.join("sessions/s.json"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(npm.join("auth.json"), npm.join("link.js")).unwrap();
        let dest = temp_dir().join("pi");
        snapshot(AgentKind::Pi, &home, &dest).unwrap();
        let out = dest.join("npm/node_modules/dep");
        assert!(out.join("index.js").is_file());
        assert!(out.join("sessions/s.json").is_file());
        for name in ["auth.json", ".credentials.json", "history.jsonl", "link.js"] {
            assert!(!out.join(name).exists(), "{name}");
        }
    }

    #[test]
    fn links_replace_planted_entries_without_following_them() {
        let state = temp_dir();
        let host = temp_dir();
        fs::write(host.join("precious"), "host").unwrap();
        // The guest made a real folder and planted links at the entries.
        let home = state.join("claude/.claude");
        fs::create_dir_all(home.join("skills/mine")).unwrap();
        symlink(host.join("precious"), home.join("skills/mine/link")).unwrap();
        symlink(&host, home.join("settings.json")).unwrap();
        symlink("/opt/zwinger/profile/claude/plugins", home.join("plugins")).unwrap();
        fs::write(home.join("history.jsonl"), "own").unwrap();

        link(AgentKind::Claude, &state, &["skills".to_owned(), "settings.json".to_owned()]).unwrap();
        assert_eq!(fs::read_link(home.join("skills")).unwrap(), Path::new("/opt/zwinger/profile/claude/skills"));
        assert_eq!(
            fs::read_link(home.join("settings.json")).unwrap(),
            Path::new("/opt/zwinger/profile/claude/settings.json")
        );
        // A stale link into the mount goes; the guest's own files stay.
        assert!(fs::symlink_metadata(home.join("plugins")).is_err());
        assert_eq!(fs::read_to_string(home.join("history.jsonl")).unwrap(), "own");
        assert_eq!(fs::read_to_string(host.join("precious")).unwrap(), "host");
        assert!(host.is_dir());

        // The profile off: all links into the mount go.
        link(AgentKind::Claude, &state, &[]).unwrap();
        assert_eq!(names(&home), ["history.jsonl"]);

        // A link as a folder on the way fails, so nothing is written through it.
        fs::remove_dir_all(state.join("claude")).unwrap();
        symlink(&host, state.join("claude")).unwrap();
        assert!(link(AgentKind::Claude, &state, &["skills".to_owned()]).is_err());
        assert_eq!(names(&host), ["precious"]);
    }
}
