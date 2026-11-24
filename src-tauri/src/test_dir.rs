//! The owned temporary root of the test run.
//!
//! Each test process puts its synthetic directories under one root,
//! `$TMPDIR/zwinger-test-<pid>`. The process deletes the root when it exits.
//! The first call also deletes the roots of test processes that are gone,
//! so a crashed run does not leave data behind.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const PREFIX: &str = "zwinger-test-";

/// The temporary root of this test process. Use it in place of `std::env::temp_dir()`.
pub(crate) fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let tmp = std::env::temp_dir();
        remove_stale(&tmp);
        let root = tmp.join(format!("{PREFIX}{}", std::process::id()));
        if !is_real_dir(&root) {
            // A link or a file at our name is not ours: fail, do not follow it.
            std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        }
        // SAFETY: `remove_root` is a plain `extern "C"` function with no arguments.
        unsafe { libc::atexit(remove_root) };
        root
    })
}

extern "C" fn remove_root() {
    let root = std::env::temp_dir().join(format!("{PREFIX}{}", std::process::id()));
    remove(&root);
}

/// Deletes `dir`. Some tests make folders read-only, so on a failure this
/// gives each folder back its write access and tries again.
fn remove(dir: &Path) {
    if !is_real_dir(dir) {
        return;
    }
    if std::fs::remove_dir_all(dir).is_ok() {
        return;
    }
    unlock(dir);
    let _ = std::fs::remove_dir_all(dir);
}

/// True for a directory that is not a symlink.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

fn unlock(dir: &Path) {
    if !is_real_dir(dir) {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            unlock(&entry.path());
        }
    }
}

/// Deletes the roots of test processes that no longer run.
fn remove_stale(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) =
            name.to_str().and_then(|name| name.strip_prefix(PREFIX)).and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        // SAFETY: signal 0 only checks that the process exists.
        let gone =
            unsafe { libc::kill(pid, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            remove(&entry.path());
        }
    }
}
