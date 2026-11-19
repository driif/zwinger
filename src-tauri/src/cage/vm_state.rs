//! The host dir of the agent state of each VM: `<app_data>/projects/<VM key>`.
//! Before the rename of shields to projects, it was
//! `<app_data>/shields/<VM key>`. [`migrate`] moves those dirs. While the old
//! dir of a VM key is there, [`dir`] refuses the key, so no start, read or
//! delete uses an empty new state in its place. [`dir`] also refuses a key
//! that is not one path component, and a parent or a dir that is a link,
//! so no state path leaves the app data dir.

use std::io::ErrorKind;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use crate::error::{Error, Result};
use crate::host_agents::{dir_flags, type_at};

/// The parent of the agent state dirs, below the app data dir.
pub(crate) const STATE_DIR: &str = "projects";
/// The parent of the agent state dirs before the rename to projects.
pub(crate) const LEGACY_DIR: &str = "shields";

/// The agent state dir of VM `key`. Fails with
/// [`Error::VmStateNotMoved`] while `shields/<key>` is there, and with
/// [`Error::VmStatePath`] when a parent or the dir is a link or no folder.
pub(crate) fn dir(data: &Path, key: &str) -> Result<PathBuf> {
    resolve(data, key)?
}

/// As [`dir`]. The outer error is true for every key: a bad key, a parent
/// that is a link or no folder, or a failed lookup. The inner error is
/// true for this key only.
pub(crate) fn resolve(data: &Path, key: &str) -> Result<Result<PathBuf>> {
    if !vm_key(key) {
        return Err(Error::BadVmKey(key.to_owned()));
    }
    let (legacy_root, root) = (data.join(LEGACY_DIR), data.join(STATE_DIR));
    let (legacy, target) = (legacy_root.join(key), root.join(key));
    if kind(&legacy_root)?.is_some_and(|dir| !dir) {
        return Err(Error::VmStatePath { path: legacy_root.display().to_string() });
    }
    if kind(&legacy)?.is_some() {
        return Ok(Err(Error::VmStateNotMoved {
            key: key.to_owned(),
            legacy: legacy.display().to_string(),
            target: target.display().to_string(),
        }));
    }
    match kind(&root)? {
        None => return Ok(Ok(target)),
        Some(false) => return Err(Error::VmStatePath { path: root.display().to_string() }),
        Some(true) => {}
    }
    Ok(match kind(&target)? {
        Some(false) => Err(Error::VmStatePath { path: target.display().to_string() }),
        _ => Ok(target),
    })
}

/// [`dir`], made if it is missing. The parents are made and opened with no
/// link followed, then the path is checked again.
pub(crate) fn create(data: &Path, key: &str) -> Result<PathBuf> {
    dir(data, key)?;
    let mut fd = rustix::fs::open(data, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
        .map_err(std::io::Error::from)?;
    for part in [STATE_DIR, key] {
        match rustix::fs::mkdirat(&fd, part, Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
        fd = rustix::fs::openat(&fd, part, dir_flags(), Mode::empty()).map_err(std::io::Error::from)?;
    }
    dir(data, key)
}

/// `Some(true)`: a folder. `Some(false)`: a link or another file. `None`: no entry.
fn kind(path: &Path) -> std::io::Result<Option<bool>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta.is_dir())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// An old dir that [`migrate`] did not move.
#[derive(Debug, PartialEq)]
pub(crate) struct Problem {
    pub legacy: PathBuf,
    pub target: PathBuf,
    pub reason: String,
}

impl Problem {
    pub(crate) fn notice(&self) -> String {
        format!(
            "zwinger could not move the agent state {} to {}: {}. Move or delete one of the two, then restart zwinger.",
            self.legacy.display(),
            self.target.display(),
            self.reason
        )
    }
}

#[derive(Debug, Default)]
pub(crate) struct Report {
    /// The VM keys whose state moved.
    pub moved: Vec<String>,
    pub problems: Vec<Problem>,
    /// The move was interrupted and moved nothing. A later attempt can
    /// succeed, so the move stays pending.
    pub retry: bool,
}

impl Report {
    fn problem(&mut self, legacy: PathBuf, target: PathBuf, reason: impl Into<String>) {
        self.problems.push(Problem { legacy, target, reason: reason.into() });
    }
}

/// Moves each `shields/<key>` to `projects/<key>` with one rename, so the
/// contents, the permissions and the inode stay. It copies nothing, follows
/// no link, reads only the top level of `shields/` and deletes no folder
/// that has contents. Each change is relative to a descriptor of a parent
/// that was opened with no link followed, and the rename does not replace
/// a target. An entry that it cannot move stays and is a problem. It runs
/// only when no VM of an earlier run can still mount `shields/`.
pub(crate) fn migrate(data: &Path) -> Report {
    let (legacy_root, root) = (data.join(LEGACY_DIR), data.join(STATE_DIR));
    let mut report = Report::default();
    let data_fd = match rustix::fs::open(data, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return report,
        Err(e) => {
            report.problem(legacy_root, root, std::io::Error::from(e).to_string());
            return report;
        }
    };
    let legacy_fd = match open_parent(&data_fd, LEGACY_DIR, false) {
        Ok(Some(fd)) => fd,
        Ok(None) => return report,
        Err(reason) => {
            report.problem(legacy_root, root, reason);
            return report;
        }
    };
    let root_fd = match open_parent(&data_fd, STATE_DIR, true) {
        Ok(Some(fd)) => fd,
        // Removed between the make and the open: an interrupted move.
        Ok(None) => {
            eprintln!("{} was removed during the move of the agent state; the next start tries again", root.display());
            report.retry = true;
            return report;
        }
        Err(reason) => {
            report.problem(legacy_root, root, format!("the new path: {reason}"));
            return report;
        }
    };
    let entries = match Dir::read_from(&legacy_fd) {
        Ok(entries) => entries,
        Err(e) => {
            report.problem(legacy_root, root, std::io::Error::from(e).to_string());
            return report;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                report.problem(legacy_root.clone(), root.clone(), std::io::Error::from(e).to_string());
                break;
            }
        };
        let raw = entry.file_name().to_string_lossy().into_owned();
        if raw == "." || raw == ".." {
            continue;
        }
        let (legacy, target) = (legacy_root.join(&raw), root.join(&raw));
        let Some(key) = entry.file_name().to_str().ok().filter(|name| vm_key(name)) else {
            report.problem(legacy, target, "the name is not a VM key");
            continue;
        };
        match type_at(&legacy_fd, key) {
            Ok(Some(FileType::Directory)) => {}
            Ok(Some(_)) => {
                report.problem(legacy, target, "it is a link or not a folder");
                continue;
            }
            Ok(None) => continue,
            Err(e) => {
                report.problem(legacy, target, e.to_string());
                continue;
            }
        }
        match move_one(&legacy_fd, &root_fd, key) {
            Ok(()) => report.moved.push(key.to_owned()),
            Err(reason) => report.problem(legacy, target, reason),
        }
    }
    match rustix::fs::unlinkat(&data_fd, LEGACY_DIR, AtFlags::REMOVEDIR) {
        Ok(()) | Err(Errno::NOTEMPTY | Errno::EXIST | Errno::NOENT) => {}
        Err(e) => eprintln!("the empty {} could not be removed: {e}", legacy_root.display()),
    }
    report
}

/// Opens the parent `name` of `data` with no link followed. `make` makes it
/// if it is missing. `None`: it is missing.
fn open_parent(data: &OwnedFd, name: &str, make: bool) -> std::result::Result<Option<OwnedFd>, String> {
    if make {
        match rustix::fs::mkdirat(data, name, Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(std::io::Error::from(e).to_string()),
        }
        #[cfg(test)]
        if let Some(hook) = tests::AFTER_MAKE.with(std::cell::Cell::get) {
            hook(data, name);
        }
    }
    match rustix::fs::openat(data, name, dir_flags(), Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => Err("it is a link or not a folder".into()),
        Err(e) => Err(std::io::Error::from(e).to_string()),
    }
}

/// The names that zwinger gives VM keys: cage ids, which are UUIDs or
/// slugs, `@library`, and `<cage id>-<n>`. Each is one path component.
pub(crate) fn vm_key(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with(['-', '.'])
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
}

/// An empty target is from an earlier attempt or a start that made it: it
/// is removed. A target with contents stays, and so does the old dir. The
/// rename fails if a new target was made meanwhile.
fn move_one(legacy: &OwnedFd, root: &OwnedFd, key: &str) -> std::result::Result<(), String> {
    match type_at(root, key) {
        Ok(Some(FileType::Directory)) => match rustix::fs::unlinkat(root, key, AtFlags::REMOVEDIR) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(Errno::NOTEMPTY | Errno::EXIST) => return Err("the new folder is there and is not empty".into()),
            Err(e) => return Err(std::io::Error::from(e).to_string()),
        },
        Ok(Some(_)) => return Err("the new path is there and is a link or not a folder".into()),
        Ok(None) => {}
        Err(e) => return Err(e.to_string()),
    }
    // No copy on a failure, such as across file systems: the state stays where it is.
    match rustix::fs::renameat_with(legacy, key, root, key, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err("a new folder was made there during the move".into()),
        Err(e) => Err(std::io::Error::from(e).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

    use super::*;

    type Hook = fn(&OwnedFd, &str);

    thread_local! {
        /// Runs in `open_parent` between the make and the open of a parent.
        pub(super) static AFTER_MAKE: std::cell::Cell<Option<Hook>> = const { std::cell::Cell::new(None) };
    }

    const SHARED: &str = "0b5c7c4e-2f61-4e43-9a0e-4b7a3c1d9e21";
    const OWN: &str = "0b5c7c4e-2f61-4e43-9a0e-4b7a3c1d9e21-2";

    fn data() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-vm-state-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn legacy(data: &Path, key: &str) -> PathBuf {
        data.join(LEGACY_DIR).join(key)
    }

    fn target(data: &Path, key: &str) -> PathBuf {
        data.join(STATE_DIR).join(key)
    }

    /// An old state dir with a private file and a nested folder.
    fn old_state(data: &Path, key: &str) -> PathBuf {
        let dir = legacy(data, key);
        fs::create_dir_all(dir.join("claude/.claude")).unwrap();
        fs::write(dir.join("claude/.claude/settings.json"), key).unwrap();
        fs::set_permissions(dir.join("claude/.claude/settings.json"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn a_fresh_store_uses_the_new_dir_with_no_step() {
        let data = data();
        assert_eq!(dir(&data, SHARED).unwrap(), target(&data, SHARED));
        let report = migrate(&data);
        assert!(report.moved.is_empty() && report.problems.is_empty());
        assert!(!data.join(STATE_DIR).exists() && !data.join(LEGACY_DIR).exists());
    }

    #[test]
    fn a_key_with_an_old_dir_is_refused_until_it_moves() {
        let data = data();
        old_state(&data, OWN);
        let e = dir(&data, OWN).unwrap_err();
        assert!(matches!(&e, Error::VmStateNotMoved { key, .. } if key == OWN), "{e}");
        let text = e.to_string();
        assert!(text.contains(&legacy(&data, OWN).display().to_string()), "{text}");
        assert!(text.contains(&target(&data, OWN).display().to_string()), "{text}");
        assert_eq!(dir(&data, SHARED).unwrap(), target(&data, SHARED));
        migrate(&data);
        assert_eq!(dir(&data, OWN).unwrap(), target(&data, OWN));
    }

    #[test]
    fn shared_and_own_keys_move_with_contents_permissions_and_inode() {
        let data = data();
        let inodes = [SHARED, OWN].map(|key| fs::metadata(old_state(&data, key)).unwrap().ino());
        let report = migrate(&data);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        let mut moved = report.moved.clone();
        moved.sort();
        assert_eq!(moved, [SHARED, OWN]);
        for (key, ino) in [SHARED, OWN].into_iter().zip(inodes) {
            let dir = target(&data, key);
            assert_eq!(fs::metadata(&dir).unwrap().ino(), ino);
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            let file = dir.join("claude/.claude/settings.json");
            assert_eq!(fs::read_to_string(&file).unwrap(), key);
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!data.join(LEGACY_DIR).exists(), "the empty old parent is removed");
    }

    #[test]
    fn an_empty_target_from_an_earlier_attempt_is_replaced() {
        let data = data();
        old_state(&data, SHARED);
        fs::create_dir_all(target(&data, SHARED)).unwrap();
        let report = migrate(&data);
        assert_eq!((report.moved, report.problems), (vec![SHARED.to_owned()], vec![]));
        assert!(target(&data, SHARED).join("claude/.claude/settings.json").is_file());
    }

    #[test]
    fn a_partial_earlier_attempt_moves_the_rest() {
        let data = data();
        old_state(&data, SHARED);
        old_state(&data, OWN);
        fs::create_dir_all(data.join(STATE_DIR)).unwrap();
        fs::rename(legacy(&data, SHARED), target(&data, SHARED)).unwrap();
        let report = migrate(&data);
        assert_eq!((report.moved, report.problems), (vec![OWN.to_owned()], vec![]));
        for key in [SHARED, OWN] {
            assert!(target(&data, key).join("claude/.claude/settings.json").is_file());
        }
        assert!(!data.join(LEGACY_DIR).exists());
    }

    #[test]
    fn a_target_with_contents_is_a_conflict_that_keeps_both_until_a_retry() {
        let data = data();
        old_state(&data, SHARED);
        fs::create_dir_all(target(&data, SHARED)).unwrap();
        fs::write(target(&data, SHARED).join("new"), "new").unwrap();
        let report = migrate(&data);
        assert!(report.moved.is_empty());
        assert_eq!(report.problems.len(), 1);
        let problem = &report.problems[0];
        assert_eq!((&problem.legacy, &problem.target), (&legacy(&data, SHARED), &target(&data, SHARED)));
        let notice = problem.notice();
        assert!(notice.contains(&legacy(&data, SHARED).display().to_string()), "{notice}");
        assert!(notice.contains(&target(&data, SHARED).display().to_string()), "{notice}");
        assert!(notice.contains("then restart zwinger"), "{notice}");
        assert!(legacy(&data, SHARED).join("claude/.claude/settings.json").is_file());
        assert_eq!(fs::read_to_string(target(&data, SHARED).join("new")).unwrap(), "new");
        assert!(dir(&data, SHARED).is_err(), "the key stays pending");

        // A restart after the user deleted the new folder moves the old state.
        fs::remove_dir_all(target(&data, SHARED)).unwrap();
        let report = migrate(&data);
        assert_eq!((report.moved, report.problems), (vec![SHARED.to_owned()], vec![]));
        assert!(dir(&data, SHARED).is_ok() && !data.join(LEGACY_DIR).exists());
    }

    #[test]
    fn a_new_parent_that_disappears_is_an_interrupted_move_to_retry() {
        let data = data();
        old_state(&data, SHARED);
        AFTER_MAKE.with(|hook| {
            hook.set(Some(|data, name| rustix::fs::unlinkat(data, name, AtFlags::REMOVEDIR).unwrap()));
        });
        let report = migrate(&data);
        AFTER_MAKE.with(|hook| hook.set(None));
        assert!(report.retry && report.moved.is_empty() && report.problems.is_empty(), "{report:?}");
        assert!(legacy(&data, SHARED).join("claude/.claude/settings.json").is_file());
        assert!(!data.join(STATE_DIR).exists());
        assert!(matches!(dir(&data, SHARED), Err(Error::VmStateNotMoved { .. })));

        let report = migrate(&data);
        assert!(!report.retry && report.problems.is_empty(), "{report:?}");
        assert_eq!(report.moved, [SHARED]);
        assert!(target(&data, SHARED).join("claude/.claude/settings.json").is_file());
    }

    #[test]
    fn a_second_run_does_nothing() {
        let data = data();
        old_state(&data, SHARED);
        migrate(&data);
        let report = migrate(&data);
        assert!(report.moved.is_empty() && report.problems.is_empty());
        assert!(target(&data, SHARED).join("claude/.claude/settings.json").is_file());
    }

    #[test]
    fn links_files_and_malformed_names_stay_and_are_problems() {
        let data = data();
        let outside = data.join("outside");
        fs::create_dir_all(outside.join("inner")).unwrap();
        fs::create_dir_all(data.join(LEGACY_DIR)).unwrap();
        symlink(&outside, legacy(&data, "linked")).unwrap();
        fs::write(legacy(&data, "file"), "x").unwrap();
        for name in [".hidden", "a b", "-x"] {
            fs::create_dir_all(legacy(&data, name)).unwrap();
        }
        old_state(&data, SHARED);
        let report = migrate(&data);
        assert_eq!(report.moved, [SHARED]);
        let mut left: Vec<String> =
            report.problems.iter().map(|p| p.legacy.file_name().unwrap().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["-x", ".hidden", "a b", "file", "linked"]);
        for name in ["linked", "file", ".hidden", "a b", "-x"] {
            assert!(fs::symlink_metadata(legacy(&data, name)).is_ok(), "{name} stays");
            assert!(fs::symlink_metadata(target(&data, name)).is_err(), "{name} is not moved");
        }
        assert!(outside.join("inner").is_dir(), "the link target is not touched");
        assert!(data.join(LEGACY_DIR).is_dir(), "a parent with entries stays");
    }

    #[test]
    fn a_linked_old_parent_is_refused() {
        let data = data();
        let outside = data.join("outside");
        fs::create_dir_all(outside.join(SHARED)).unwrap();
        symlink(&outside, data.join(LEGACY_DIR)).unwrap();
        let report = migrate(&data);
        assert!(report.moved.is_empty());
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].legacy, data.join(LEGACY_DIR));
        assert!(outside.join(SHARED).is_dir());
        assert!(fs::symlink_metadata(data.join(LEGACY_DIR)).unwrap().file_type().is_symlink());
        assert!(!data.join(STATE_DIR).exists());
    }

    #[test]
    fn a_linked_new_parent_is_refused() {
        let data = data();
        old_state(&data, SHARED);
        let outside = data.join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, data.join(STATE_DIR)).unwrap();
        let report = migrate(&data);
        assert!(report.moved.is_empty());
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].target, data.join(STATE_DIR));
        assert!(legacy(&data, SHARED).join("claude/.claude/settings.json").is_file());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn a_linked_old_parent_refuses_every_key_also_when_empty() {
        let data = data();
        let outside = data.join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, data.join(LEGACY_DIR)).unwrap();
        for key in [SHARED, OWN] {
            assert!(matches!(dir(&data, key), Err(Error::VmStatePath { .. })), "{key}");
            assert!(create(&data, key).is_err());
        }
        assert!(!data.join(STATE_DIR).exists(), "no fresh state is made");
    }

    #[test]
    fn a_linked_new_parent_or_dir_is_never_used() {
        let data = data();
        let outside = data.join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, data.join(STATE_DIR)).unwrap();
        let e = resolve(&data, SHARED).unwrap_err();
        assert!(matches!(&e, Error::VmStatePath { path } if *path == data.join(STATE_DIR).display().to_string()));
        assert!(create(&data, SHARED).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "nothing is made outside");
        fs::remove_file(data.join(STATE_DIR)).unwrap();

        fs::create_dir_all(data.join(STATE_DIR)).unwrap();
        symlink(&outside, target(&data, SHARED)).unwrap();
        let inner = resolve(&data, SHARED).unwrap().unwrap_err();
        assert!(matches!(inner, Error::VmStatePath { .. }), "only this key is refused");
        assert!(create(&data, SHARED).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(create(&data, OWN).unwrap(), target(&data, OWN));
        assert!(fs::symlink_metadata(target(&data, OWN)).unwrap().is_dir());
    }

    #[test]
    fn a_malformed_key_is_refused_before_any_path_is_built() {
        let data = data();
        for key in ["", ".", "..", "../x", "a/b", "/abs", "-x", ".hidden", "a\0b", "a b"] {
            assert!(matches!(resolve(&data, key), Err(Error::BadVmKey(_))), "{key:?}");
            assert!(create(&data, key).is_err(), "{key:?}");
        }
        assert!(!data.join(STATE_DIR).exists());
        assert!(dir(&data, "a").is_ok() && dir(&data, "proj_1.x-2").is_ok());
    }

    #[test]
    fn create_makes_the_dir_and_keeps_an_existing_one() {
        let data = data();
        let made = create(&data, SHARED).unwrap();
        fs::write(made.join("f"), "x").unwrap();
        assert_eq!(create(&data, SHARED).unwrap(), made);
        assert_eq!(fs::read_to_string(made.join("f")).unwrap(), "x");
    }
}
