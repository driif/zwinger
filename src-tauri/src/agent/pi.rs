//! The pi agent: the bundled pi, or the host install that a shield mounts.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The pi bundle, mounted read-only.
pub const PI_DIR: &str = "/opt/zwinger/pi";
/// The host install of pi, mounted read-only when the shield uses it.
pub const HOST_DIR: &str = "/opt/zwinger/host/pi";
/// The entry point in the host package. Detection accepts only this layout.
pub const HOST_ENTRY: &str = "dist/cli.js";
pub const AGENT_DIR: &str = "/opt/zwinger/state/agent";
/// Where the interactive pi keeps its sessions.
pub const SESSION_DIR: &str = "/opt/zwinger/state/sessions";
/// pi 1.3 needs at least this Node version.
const NODE_MIN: (u32, u32) = (22, 19);

/// The script of the host pi (`host` true) or of the bundled pi.
pub fn cli(host: bool) -> String {
    if host { format!("{HOST_DIR}/{HOST_ENTRY}") } else { format!("{PI_DIR}/cli.js") }
}

/// Accepts `v22.19.0` and newer.
pub fn check_node(version: &str) -> Result<()> {
    let version = version.trim();
    let mut parts = version.trim_start_matches('v').split('.').map(str::parse::<u32>);
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) if (major, minor) >= NODE_MIN => Ok(()),
        _ => Err(Error::NodeTooOld(version.to_owned())),
    }
}

/// Copies the bundled pi once per version to `<runtime>/pi-<version>` and
/// returns that dir. The copy is in the app data dir, so no user mount can
/// reach it, and a partial copy is never used: it is renamed into place.
pub fn install_pi(bundle: &Path, runtime: &Path) -> Result<PathBuf> {
    let version = std::fs::read_to_string(bundle.join("VERSION"))?;
    let version = version.trim();
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return Err(Error::Io(std::io::Error::other(format!("invalid pi VERSION: {version}"))));
    }
    let dest = runtime.join(format!("pi-{version}"));
    if dest.is_dir() {
        return Ok(dest);
    }
    std::fs::create_dir_all(runtime)?;
    let tmp = runtime.join(format!(".pi-{version}-{}", uuid::Uuid::new_v4()));
    let copied = copy_dir(bundle, &tmp).and_then(|()| std::fs::rename(&tmp, &dest));
    if copied.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        // Another app instance may have won the rename.
        if !dest.is_dir() {
            copied?;
        }
    }
    Ok(dest)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn node_version_check() {
        for ok in ["v22.19.0\n", "v22.20.1", "v24.0.0"] {
            assert!(check_node(ok).is_ok(), "{ok}");
        }
        for old in ["v22.18.9", "v20.11.0", "", "bash: node: not found"] {
            assert!(matches!(check_node(old), Err(Error::NodeTooOld(_))), "{old}");
        }
    }

    #[test]
    fn install_pi_copies_once_per_version() {
        let bundle = temp_dir();
        std::fs::write(bundle.join("VERSION"), "1.3.0\n").unwrap();
        std::fs::create_dir(bundle.join("chunks")).unwrap();
        std::fs::write(bundle.join("chunks").join("a.js"), "a").unwrap();
        let runtime = temp_dir().join("runtime");

        let dest = install_pi(&bundle, &runtime).unwrap();
        assert_eq!(dest, runtime.join("pi-1.3.0"));
        assert_eq!(std::fs::read_to_string(dest.join("chunks").join("a.js")).unwrap(), "a");

        std::fs::write(bundle.join("chunks").join("a.js"), "changed").unwrap();
        assert_eq!(install_pi(&bundle, &runtime).unwrap(), dest);
        assert_eq!(std::fs::read_to_string(dest.join("chunks").join("a.js")).unwrap(), "a");
        let names: Vec<_> = std::fs::read_dir(&runtime).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["pi-1.3.0"]);

        std::fs::write(bundle.join("VERSION"), "../x").unwrap();
        assert!(install_pi(&bundle, &runtime).is_err());
    }
}
