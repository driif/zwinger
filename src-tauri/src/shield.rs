use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const WORKSPACE: &str = "/workspace";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shield {
    pub id: String,
    pub name: String,
    pub folder: PathBuf,
    pub mounts: Vec<Mount>,
    pub network: NetworkPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    pub host: PathBuf,
    pub guest: String,
    pub readonly: bool,
}

/// `domains` is kept when `allow_all` is on, so the allow list survives a toggle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicy {
    pub allow_all: bool,
    pub domains: Vec<String>,
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
        })
    }

    pub fn add_mount(&mut self, host: &str, guest: &str, readonly: bool) -> Result<()> {
        let host = host_dir(host)?;
        let guest = guest_path(guest)?;
        if self.mounts.iter().any(|m| m.guest == guest) {
            return Err(Error::DuplicateMount(guest));
        }
        self.mounts.push(Mount {
            host,
            guest,
            readonly,
        });
        self.check_overlap().inspect_err(|_| {
            self.mounts.pop();
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

pub fn guest_path(path: &str) -> Result<String> {
    let p = Path::new(path);
    let normalized = p.is_absolute()
        && p.components()
            .skip(1)
            .all(|c| matches!(c, Component::Normal(_)))
        && p.components().count() > 1;
    if !normalized {
        return Err(Error::InvalidGuestPath(path.to_owned()));
    }
    if p.starts_with(WORKSPACE) || Path::new(WORKSPACE).starts_with(p) {
        return Err(Error::WorkspaceOverlap(path.to_owned()));
    }
    Ok(p.components().collect::<PathBuf>().to_string_lossy().into_owned())
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
