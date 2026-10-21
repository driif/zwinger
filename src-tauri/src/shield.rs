use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent;
use crate::error::{Error, Result};
use crate::secrets::{self, Secret};

pub const WORKSPACE: &str = "/workspace";
const FOLDER_LIMIT: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shield {
    pub id: String,
    pub name: String,
    pub folder: PathBuf,
    pub mounts: Vec<Mount>,
    pub network: NetworkPolicy,
    /// The guest folder where agent sessions start: below `/workspace` or a mount.
    #[serde(default)]
    pub harness: Option<String>,
    /// The values are in the vault.
    #[serde(default)]
    pub secrets: Vec<Secret>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    pub host: PathBuf,
    pub guest: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NetworkMode {
    #[default]
    Deny,
    List,
    Public,
}

/// `domains` is kept in every mode, so the allow list survives a mode change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredNetworkPolicy")]
pub struct NetworkPolicy {
    pub mode: NetworkMode,
    pub domains: Vec<String>,
}

/// Configs saved before `mode` existed have `allowAll` instead.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredNetworkPolicy {
    Current {
        mode: NetworkMode,
        domains: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    Legacy {
        allow_all: bool,
        domains: Vec<String>,
    },
}

impl From<StoredNetworkPolicy> for NetworkPolicy {
    fn from(stored: StoredNetworkPolicy) -> Self {
        match stored {
            StoredNetworkPolicy::Current { mode, domains } => Self { mode, domains },
            StoredNetworkPolicy::Legacy { allow_all, domains } => Self {
                mode: if allow_all { NetworkMode::Public } else { NetworkMode::List },
                domains,
            },
        }
    }
}

impl NetworkPolicy {
    /// Adds `input` as a normalized domain, once.
    pub fn add_domain(&mut self, input: &str) -> Result<()> {
        let domain = domain(input)?;
        if !self.domains.contains(&domain) {
            self.domains.push(domain);
        }
        Ok(())
    }

    pub fn remove_domain(&mut self, domain: &str) {
        self.domains.retain(|d| d != domain);
    }
}

impl Shield {
    pub fn new(folder: &str) -> Result<Self> {
        let folder = host_dir(folder)?;
        let name = folder
            .file_name()
            .map_or_else(|| folder.display().to_string(), |n| n.to_string_lossy().into_owned());
        Ok(Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            folder,
            mounts: Vec::new(),
            network: NetworkPolicy::default(),
            harness: None,
            secrets: Vec::new(),
        })
    }

    pub fn rename(&mut self, name: &str) -> Result<()> {
        let name = name.trim();
        if !(1..=64).contains(&name.chars().count()) || name.chars().any(char::is_control) {
            return Err(Error::InvalidName(name.to_owned()));
        }
        name.clone_into(&mut self.name);
        Ok(())
    }

    /// Accepts a sub-path of `/workspace` whose host folder exists, or a mount's
    /// guest path or a sub-path of it. `None` clears the harness.
    pub fn set_harness(&mut self, harness: Option<&str>) -> Result<()> {
        let Some(input) = harness else {
            self.harness = None;
            return Ok(());
        };
        let path = normalize(input).ok_or_else(|| Error::InvalidGuestPath(input.to_owned()))?;
        if let Ok(rel) = path.strip_prefix(WORKSPACE)
            && !rel.as_os_str().is_empty()
            && !self.folder.join(rel).is_dir()
        {
            return Err(Error::MissingPath(path.display().to_string()));
        }
        self.harness = Some(path.to_string_lossy().into_owned());
        self.check()
    }

    /// Adds the secret, or replaces the secret with the same name. Returns
    /// true if the secret is new.
    pub fn put_secret(&mut self, secret: Secret) -> bool {
        match self.secrets.iter_mut().find(|s| s.name == secret.name) {
            Some(current) => {
                *current = secret;
                false
            }
            None => {
                self.secrets.push(secret);
                true
            }
        }
    }

    pub fn remove_secret(&mut self, name: &str) -> Result<()> {
        let before = self.secrets.len();
        self.secrets.retain(|s| s.name != name);
        if self.secrets.len() == before {
            return Err(Error::SecretNotFound(name.to_owned()));
        }
        Ok(())
    }

    /// The rules that every stored config must meet.
    pub fn check(&self) -> Result<()> {
        self.check_overlap()?;
        secrets::check_unique(&self.secrets)?;
        match &self.harness {
            Some(harness) if !self.harness_target_exists(Path::new(harness)) => {
                Err(Error::HarnessTarget(harness.clone()))
            }
            _ => Ok(()),
        }
    }

    fn harness_target_exists(&self, harness: &Path) -> bool {
        harness.starts_with(WORKSPACE) && harness != Path::new(WORKSPACE)
            || self.mounts.iter().any(|m| harness.starts_with(&m.guest))
    }

    /// The guest path of the mount that holds the harness: the deepest mount
    /// whose guest path contains it. `None` for `/workspace` or no harness.
    fn harness_mount(&self) -> Option<&str> {
        let harness = Path::new(self.harness.as_deref()?);
        self.mounts
            .iter()
            .filter(|m| harness.starts_with(&m.guest))
            .map(|m| m.guest.as_str())
            .max_by_key(|g| g.len())
    }

    /// Refuses a mount change that moves the harness to a different mount, so
    /// the harness cannot silently point to a different host folder.
    fn keep_harness_mount(&mut self, change: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        let before = self.harness_mount().map(str::to_owned);
        let saved = self.mounts.clone();
        change(self)?;
        if self.harness_mount() != before.as_deref() {
            self.mounts = saved;
            let harness = self.harness.clone().unwrap_or_default();
            return Err(Error::HarnessMount(harness, before.unwrap_or_else(|| WORKSPACE.to_owned())));
        }
        Ok(())
    }

    pub fn add_mount(&mut self, host: &str, guest: &str, readonly: bool) -> Result<()> {
        let host = host_dir(host)?;
        let guest = guest_path(guest)?;
        if self.mounts.iter().any(|m| m.guest == guest) {
            return Err(Error::DuplicateMount(guest));
        }
        self.keep_harness_mount(|s| {
            s.mounts.push(Mount {
                host,
                guest,
                readonly,
            });
            Ok(())
        })?;
        self.check_overlap().inspect_err(|_| {
            self.mounts.pop();
        })
    }

    pub fn remove_mount(&mut self, guest: &str) -> Result<()> {
        self.mount_mut(guest)?;
        self.keep_harness_mount(|s| {
            s.mounts.retain(|m| m.guest != guest);
            Ok(())
        })
    }

    /// Two host paths may not be nested, in either direction, when either one is
    /// writable: the guest could then write through one mount into the other.
    /// The shield folder is writable.
    pub fn check_overlap(&self) -> Result<()> {
        let paths: Vec<(&Path, bool)> = std::iter::once((self.folder.as_path(), true))
            .chain(self.mounts.iter().map(|m| (m.host.as_path(), !m.readonly)))
            .collect();
        for (i, (a, a_rw)) in paths.iter().enumerate() {
            for (b, b_rw) in &paths[i + 1..] {
                if (*a_rw || *b_rw) && (a.starts_with(b) || b.starts_with(a)) {
                    return Err(Error::MountOverlap(
                        a.display().to_string(),
                        b.display().to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// A writable path of another shield may not nest with any path of this
    /// shield, in either direction: a guest of one shield could then swap a
    /// folder that the other shield binds at start.
    pub fn check_overlap_with<'a>(&self, others: impl IntoIterator<Item = &'a Shield>) -> Result<()> {
        let ours: Vec<&Path> = std::iter::once(self.folder.as_path())
            .chain(self.mounts.iter().map(|m| m.host.as_path()))
            .collect();
        for other in others {
            let theirs = std::iter::once(other.folder.as_path())
                .chain(other.mounts.iter().filter(|m| !m.readonly).map(|m| m.host.as_path()));
            for b in theirs {
                if let Some(a) = ours.iter().find(|a| a.starts_with(b) || b.starts_with(a)) {
                    return Err(Error::MountOverlap(
                        a.display().to_string(),
                        b.display().to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// No host path may nest with the app data dir, in either direction, even
    /// read-only: it holds the pi runtime and the chats of every shield.
    pub fn check_app_data(&self, data: &Path) -> Result<()> {
        let paths = std::iter::once(&self.folder).chain(self.mounts.iter().map(|m| &m.host));
        for path in paths {
            if path.starts_with(data) || data.starts_with(path) {
                return Err(Error::AppDataOverlap(path.display().to_string(), data.display().to_string()));
            }
        }
        Ok(())
    }

    /// Resolves the stored host paths again before a start. A path that now
    /// resolves elsewhere (for example, a swapped symlink) is refused.
    pub fn verify_paths(&self) -> Result<()> {
        let stored = std::iter::once(&self.folder).chain(self.mounts.iter().map(|m| &m.host));
        for path in stored {
            let display = path.display().to_string();
            if host_dir(&display)? != *path {
                return Err(Error::PathChanged(display));
            }
        }
        self.check_overlap()
    }

    pub fn mount_mut(&mut self, guest: &str) -> Result<&mut Mount> {
        self.mounts
            .iter_mut()
            .find(|m| m.guest == guest)
            .ok_or(Error::MountNotFound)
    }
}

/// Resolves a host folder to its canonical path, so symlinks are followed once here
/// and the stored path is the real directory.
pub fn host_dir(path: &str) -> Result<PathBuf> {
    let raw = Path::new(path);
    if !raw.is_absolute() {
        return Err(Error::RelativePath(path.to_owned()));
    }
    let canonical = raw
        .canonicalize()
        .map_err(|_| Error::MissingPath(path.to_owned()))?;
    if !canonical.is_dir() {
        return Err(Error::NotDirectory(path.to_owned()));
    }
    Ok(canonical)
}

/// Accepts a hostname with at least two labels, or `*.` and such a hostname.
/// Returns it in lowercase.
pub fn domain(input: &str) -> Result<String> {
    let invalid = |reason: &str| Error::InvalidDomain(input.to_owned(), reason.to_owned());
    let domain = input.trim().to_ascii_lowercase();
    let host = domain.strip_prefix("*.").unwrap_or(&domain);
    if host.len() > 253 {
        return Err(invalid("longer than 253 characters"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(invalid("needs at least two labels, like example.com"));
    }
    let valid_label = |l: &&str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    if !labels.iter().all(valid_label) {
        return Err(invalid("use only letters, digits, hyphens and dots, with no scheme, path or port"));
    }
    Ok(domain)
}

/// An absolute guest path below `/`, with no `.` or `..` and no repeated or
/// trailing slashes.
fn normalize(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    let normalized = p.is_absolute()
        && p.components()
            .skip(1)
            .all(|c| matches!(c, Component::Normal(_)))
        && p.components().count() > 1;
    normalized.then(|| p.components().collect())
}

pub fn guest_path(path: &str) -> Result<String> {
    let p = normalize(path).ok_or_else(|| Error::InvalidGuestPath(path.to_owned()))?;
    if p.starts_with(WORKSPACE) || Path::new(WORKSPACE).starts_with(&p) {
        return Err(Error::WorkspaceOverlap(path.to_owned()));
    }
    if p.starts_with(agent::RESERVED) || Path::new(agent::RESERVED).starts_with(&p) {
        return Err(Error::ReservedGuestPath(path.to_owned()));
    }
    Ok(p.to_string_lossy().into_owned())
}

/// The top-level folders of a shield folder, for display: real directories
/// only (no symlinks, no hidden names), sorted, at most `FOLDER_LIMIT`.
pub fn list_folders(folder: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string()
            && !name.starts_with('.')
        {
            names.push(name);
        }
    }
    names.sort();
    names.truncate(FOLDER_LIMIT);
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn host_dir_rejects_relative_path() {
        assert!(matches!(host_dir("some/dir"), Err(Error::RelativePath(_))));
    }

    #[test]
    fn host_dir_rejects_missing_path() {
        let missing = temp_dir("missing").join("nope");
        assert!(matches!(
            host_dir(missing.to_str().unwrap()),
            Err(Error::MissingPath(_))
        ));
    }

    #[test]
    fn host_dir_rejects_file() {
        let file = temp_dir("file").join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(
            host_dir(file.to_str().unwrap()),
            Err(Error::NotDirectory(_))
        ));
    }

    #[test]
    fn host_dir_canonicalizes_symlink() {
        let dir = temp_dir("link");
        let target = dir.join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.join("alias");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            host_dir(link.to_str().unwrap()).unwrap(),
            target.canonicalize().unwrap()
        );
    }

    #[test]
    fn guest_path_rejects_workspace_overlap() {
        for path in ["/workspace", "/workspace/sub", "/"] {
            assert!(guest_path(path).is_err(), "{path}");
        }
        assert!(matches!(
            guest_path("/workspace/x"),
            Err(Error::WorkspaceOverlap(_))
        ));
    }

    #[test]
    fn guest_path_rejects_relative_and_dot_segments() {
        for path in ["data", "/data/../workspace", "/mnt/.."] {
            assert!(
                matches!(guest_path(path), Err(Error::InvalidGuestPath(_))),
                "{path}"
            );
        }
    }

    #[test]
    fn guest_path_rejects_the_zwinger_prefix() {
        for path in ["/opt", "/opt/zwinger", "/opt/zwinger/pi", "/opt/zwinger/state/x"] {
            assert!(matches!(guest_path(path), Err(Error::ReservedGuestPath(_))), "{path}");
        }
        assert_eq!(guest_path("/opt/tools").unwrap(), "/opt/tools");
    }

    #[test]
    fn app_data_may_not_nest_with_any_path() {
        let (mut shield, root) = shield_with_dirs(&["project", "data", "data/app", "other"]);
        shield.add_mount(&path(&root, "other"), "/mnt/other", true).unwrap();
        assert!(shield.check_app_data(&root.join("data/app")).is_ok());

        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        assert!(matches!(shield.check_app_data(&root.join("data/app")), Err(Error::AppDataOverlap(..))));
        assert!(matches!(shield.check_app_data(&shield.folder.join("x")), Err(Error::AppDataOverlap(..))));
    }

    #[test]
    fn guest_path_accepts_and_trims() {
        assert_eq!(guest_path("//mnt/data/").unwrap(), "/mnt/data");
        assert_eq!(guest_path("/workspaces").unwrap(), "/workspaces");
    }

    #[test]
    fn add_mount_rejects_duplicate_guest_path() {
        let mut shield = Shield::new(temp_dir("dup").to_str().unwrap()).unwrap();
        let data = temp_dir("dup-data");
        shield.add_mount(data.to_str().unwrap(), "/mnt/a", true).unwrap();
        assert!(matches!(
            shield.add_mount(data.to_str().unwrap(), "/mnt/a/", false),
            Err(Error::DuplicateMount(_))
        ));
    }

    fn shield_with_dirs(names: &[&str]) -> (Shield, PathBuf) {
        let root = temp_dir("overlap").canonicalize().unwrap();
        for name in names {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let shield = Shield::new(root.join("project").to_str().unwrap()).unwrap();
        (shield, root)
    }

    fn path(root: &Path, name: &str) -> String {
        root.join(name).to_str().unwrap().to_owned()
    }

    #[test]
    fn mount_inside_folder_is_rejected() {
        let (mut shield, root) = shield_with_dirs(&["project/sub"]);
        assert!(matches!(
            shield.add_mount(&path(&root, "project/sub"), "/mnt/sub", true),
            Err(Error::MountOverlap(..))
        ));
        assert!(shield.mounts.is_empty());
    }

    #[test]
    fn mount_containing_folder_is_rejected() {
        let (mut shield, root) = shield_with_dirs(&["project"]);
        assert!(matches!(
            shield.add_mount(root.to_str().unwrap(), "/mnt/root", true),
            Err(Error::MountOverlap(..))
        ));
    }

    #[test]
    fn mount_inside_rw_mount_is_rejected() {
        let (mut shield, root) = shield_with_dirs(&["project", "data/inner"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", false).unwrap();
        assert!(matches!(
            shield.add_mount(&path(&root, "data/inner"), "/mnt/inner", true),
            Err(Error::MountOverlap(..))
        ));
    }

    #[test]
    fn shield_inside_other_shields_folder_is_rejected() {
        let (mut other, root) = shield_with_dirs(&["project/inner", "data"]);
        other.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        let inner = Shield::new(&path(&root, "project/inner")).unwrap();
        assert!(matches!(inner.check_overlap_with([&other]), Err(Error::MountOverlap(..))));
        let data = Shield::new(&path(&root, "data")).unwrap();
        data.check_overlap_with([&other]).unwrap();
    }

    #[test]
    fn nested_readonly_mounts_are_accepted() {
        let (mut shield, root) = shield_with_dirs(&["project", "data/inner"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.add_mount(&path(&root, "data/inner"), "/mnt/inner", true).unwrap();
        assert_eq!(shield.mounts.len(), 2);
    }

    #[test]
    fn readwrite_toggle_that_creates_overlap_is_rejected() {
        let (mut shield, root) = shield_with_dirs(&["project", "data/inner"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.add_mount(&path(&root, "data/inner"), "/mnt/inner", true).unwrap();
        shield.mount_mut("/mnt/data").unwrap().readonly = false;
        assert!(matches!(shield.check_overlap(), Err(Error::MountOverlap(..))));
    }

    #[test]
    fn verify_paths_detects_symlink_swap() {
        let (mut shield, root) = shield_with_dirs(&["project", "data", "elsewhere"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.verify_paths().unwrap();

        std::fs::remove_dir(root.join("data")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("data")).unwrap();
        assert!(matches!(shield.verify_paths(), Err(Error::PathChanged(_))));
    }

    #[test]
    fn verify_paths_detects_missing_folder() {
        let (shield, root) = shield_with_dirs(&["project"]);
        std::fs::remove_dir(root.join("project")).unwrap();
        assert!(matches!(shield.verify_paths(), Err(Error::MissingPath(_))));
    }

    #[test]
    fn set_harness_accepts_workspace_folders_and_mounts() {
        let (mut shield, root) = shield_with_dirs(&["project/agent", "data"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        for (input, stored) in [
            ("//workspace/agent/", "/workspace/agent"),
            ("/mnt/data", "/mnt/data"),
            ("/mnt/data/sub", "/mnt/data/sub"),
        ] {
            shield.set_harness(Some(input)).unwrap();
            assert_eq!(shield.harness.as_deref(), Some(stored));
        }
        shield.set_harness(None).unwrap();
        assert_eq!(shield.harness, None);
    }

    #[test]
    fn set_harness_rejects_other_paths() {
        let (mut shield, _) = shield_with_dirs(&["project"]);
        for input in ["agent", "/workspace/../etc", "/"] {
            assert!(
                matches!(shield.set_harness(Some(input)), Err(Error::InvalidGuestPath(_))),
                "{input}"
            );
        }
        assert!(matches!(
            shield.set_harness(Some("/workspace/missing")),
            Err(Error::MissingPath(_))
        ));
        for input in ["/workspace", "/mnt/data", "/etc"] {
            assert!(
                matches!(shield.set_harness(Some(input)), Err(Error::HarnessTarget(_))),
                "{input}"
            );
        }
    }

    #[test]
    fn check_refuses_harness_without_its_mount() {
        let (mut shield, root) = shield_with_dirs(&["project", "data"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.set_harness(Some("/mnt/data")).unwrap();
        shield.mounts.clear();
        assert!(matches!(shield.check(), Err(Error::HarnessTarget(h)) if h == "/mnt/data"));
    }

    #[test]
    fn mount_change_that_moves_the_harness_is_refused() {
        let (mut shield, root) = shield_with_dirs(&["project", "outer", "inner"]);
        shield.add_mount(&path(&root, "outer"), "/mnt", true).unwrap();
        shield.add_mount(&path(&root, "inner"), "/mnt/sub", true).unwrap();
        shield.set_harness(Some("/mnt/sub/agent")).unwrap();
        assert!(matches!(
            shield.remove_mount("/mnt/sub"),
            Err(Error::HarnessMount(h, m)) if h == "/mnt/sub/agent" && m == "/mnt/sub"
        ));
        assert_eq!(shield.mounts.len(), 2);
        shield.remove_mount("/mnt").unwrap();

        shield.set_harness(Some("/mnt/sub/deeper")).unwrap();
        assert!(matches!(
            shield.add_mount(&path(&root, "outer"), "/mnt/sub/deeper", true),
            Err(Error::HarnessMount(..))
        ));
        assert_eq!(shield.mounts.len(), 1);
    }

    #[test]
    fn rename_trims_and_limits() {
        let (mut shield, _) = shield_with_dirs(&["project"]);
        shield.rename("  api  ").unwrap();
        assert_eq!(shield.name, "api");
        shield.rename(&"ü".repeat(64)).unwrap();
        for name in ["", "   ", "a\nb", "tab\there", &"x".repeat(65)] {
            assert!(matches!(shield.rename(name), Err(Error::InvalidName(_))), "{name:?}");
        }
        assert_eq!(shield.name, "ü".repeat(64));
    }

    #[test]
    fn list_folders_returns_visible_real_directories() {
        let (_, root) = shield_with_dirs(&["project/web", "project/api", "project/.git", "outside"]);
        let folder = root.join("project");
        std::fs::write(folder.join("README.md"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), folder.join("link")).unwrap();
        assert_eq!(list_folders(&folder).unwrap(), ["api", "web"]);
    }

    #[test]
    fn list_folders_caps_the_count() {
        let (_, root) = shield_with_dirs(&["project"]);
        let folder = root.join("project");
        for i in 0..60 {
            std::fs::create_dir(folder.join(format!("d{i:02}"))).unwrap();
        }
        let names = list_folders(&folder).unwrap();
        assert_eq!(names.len(), FOLDER_LIMIT);
        assert_eq!(names[0], "d00");
        assert_eq!(names[49], "d49");
    }

    #[test]
    fn legacy_network_policy_migrates() {
        for (allow_all, mode) in [(false, NetworkMode::List), (true, NetworkMode::Public)] {
            let json = format!(r#"{{"allowAll":{allow_all},"domains":["registry.npmjs.org"]}}"#);
            let policy: NetworkPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(
                policy,
                NetworkPolicy {
                    mode,
                    domains: vec!["registry.npmjs.org".into()],
                }
            );
        }
        let current: NetworkPolicy = serde_json::from_str(r#"{"mode":"deny","domains":["a.io"]}"#).unwrap();
        assert_eq!(current.mode, NetworkMode::Deny);
        assert_eq!(serde_json::to_string(&current).unwrap(), r#"{"mode":"deny","domains":["a.io"]}"#);
    }

    #[test]
    fn add_domain_normalizes_and_dedupes() {
        let mut policy = NetworkPolicy::default();
        policy.add_domain(" Registry.npmjs.org ").unwrap();
        policy.add_domain("registry.npmjs.org").unwrap();
        policy.add_domain("*.anthropic.com").unwrap();
        assert!(matches!(policy.add_domain("not a domain"), Err(Error::InvalidDomain(..))));
        assert_eq!(policy.domains, ["registry.npmjs.org", "*.anthropic.com"]);

        policy.remove_domain("registry.npmjs.org");
        policy.remove_domain("registry.npmjs.org");
        assert_eq!(policy.domains, ["*.anthropic.com"]);
    }

    #[test]
    fn domain_accepts_hostnames_and_wildcards() {
        assert_eq!(domain(" Registry.NPMjs.org ").unwrap(), "registry.npmjs.org");
        assert_eq!(domain("*.anthropic.com").unwrap(), "*.anthropic.com");
        assert_eq!(domain("a-b.c1.io").unwrap(), "a-b.c1.io");
    }

    #[test]
    fn domain_rejects_other_input() {
        for input in [
            "",
            "localhost",
            "*.com",
            "https://example.com",
            "example.com/path",
            "example.com:443",
            "exa mple.com",
            "*example.com",
            "a.*.example.com",
            "-bad.example.com",
            "bad-.example.com",
            "example..com",
            "example.com.",
            "ex_ample.com",
        ] {
            assert!(matches!(domain(input), Err(Error::InvalidDomain(..))), "{input:?}");
        }
    }
}
