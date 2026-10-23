//! Host agents: the agents that are installed on the Mac (Claude Code, Codex,
//! pi and Gemini CLI) and that projects reuse. `detect` finds them, `profile`
//! gives a project a read-only snapshot of their configuration, and `login`
//! reuses their subscription login through the network proxy (ADR 0002).
//! The host filesystem, the host logins and the clock are seams, so tests use
//! fakes.

pub mod detect;
pub mod login;
pub mod profile;

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};

use crate::agent::AgentKind;
use crate::error::Result;
use detect::Detection;
use login::{HostToken, LoginSource};

/// The agents that a project can take from the host.
pub const KINDS: [AgentKind; 4] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Pi, AgentKind::Gemini];
/// How long `<bin> --version` may run.
const VERSION_LIMIT: Duration = Duration::from_secs(3);
/// The bytes of `--version` output that are read.
const VERSION_OUT: u64 = 4 << 10;

/// The host filesystem as detection sees it, and the version probe.
pub trait HostFs: Send + Sync {
    fn home(&self) -> PathBuf;
    /// The folders of the app's `PATH`.
    fn path_dirs(&self) -> Vec<PathBuf>;
    fn is_file(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn read_link(&self, path: &Path) -> Option<PathBuf>;
    fn read_to_string(&self, path: &Path) -> Option<String>;
    /// The names in the folder. Empty if it cannot be read.
    fn list_dir(&self, path: &Path) -> Vec<String>;
    fn canonicalize(&self, path: &Path) -> Option<PathBuf>;
    /// The output of `<bin> --version`, or `None` if it fails or runs longer
    /// than 3 s.
    fn version(&self, bin: &Path) -> Option<String>;
}

pub trait Clock: Send + Sync {
    /// Milliseconds since the epoch.
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
    }
}

pub struct RealFs;

impl HostFs for RealFs {
    fn home(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_default()
    }

    fn path_dirs(&self) -> Vec<PathBuf> {
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }

    fn read_to_string(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn list_dir(&self, path: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(path) else {
            return Vec::new();
        };
        entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect()
    }

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        path.canonicalize().ok()
    }

    /// Polls the child, so a hung binary is killed after the limit. The
    /// folder of `bin` comes first in `PATH`: an npm link such as
    /// `<prefix>/bin/codex` starts with `env node`, and the app's `PATH`
    /// has no node.
    fn version(&self, bin: &Path) -> Option<String> {
        let mut path = bin.parent().map(Path::to_owned).into_iter().collect::<Vec<_>>();
        path.extend(self.path_dirs());
        let mut child = Command::new(bin)
            .arg("--version")
            .env("PATH", std::env::join_paths(path).ok()?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + VERSION_LIMIT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(_)) | Err(_) => return None,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let mut out = String::new();
        child.stdout.take()?.take(VERSION_OUT).read_to_string(&mut out).ok()?;
        Some(out)
    }
}

/// The host agents of the app: the seams and the last detection.
pub struct HostAgents {
    fs: Arc<dyn HostFs>,
    logins: Arc<dyn LoginSource>,
    clock: Arc<dyn Clock>,
    /// The app data dir, where the runtimes are cached.
    data: PathBuf,
    last: Mutex<Option<Detection>>,
}

impl HostAgents {
    pub fn new(fs: Arc<dyn HostFs>, logins: Arc<dyn LoginSource>, clock: Arc<dyn Clock>, data: PathBuf) -> Self {
        Self { fs, logins, clock, data, last: Mutex::new(None) }
    }

    /// Looks for the agents again. It runs `--version`, so call it on a
    /// blocking thread.
    pub fn detect(&self) -> Detection {
        let found = detect::detect(self.fs.as_ref(), self.logins.as_ref(), self.clock.now_ms(), &self.data);
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(found.clone());
        found
    }

    /// The last detection, else a new one.
    pub fn last(&self) -> Detection {
        let last = self.last.lock().unwrap_or_else(PoisonError::into_inner).clone();
        last.unwrap_or_else(|| self.detect())
    }

    pub fn home(&self) -> PathBuf {
        self.fs.home()
    }

    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// The host login of the agent. Fails if there is none or it has expired.
    /// The Keychain read can wait for the macOS access prompt, so call it on
    /// a blocking thread.
    pub fn token(&self, kind: AgentKind) -> Result<HostToken> {
        login::token(self.logins.as_ref(), kind, self.clock.now_ms())
    }
}

// --- writes into the guest-writable state ----------------------------------
//
// The guest can replace any entry of its state dir with a link, also a
// folder on the way. These helpers follow no link: each folder is opened
// with O_NOFOLLOW from a trusted host root, and each change is relative to
// the folder's descriptor.

pub fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC
}

/// Opens `root/<parts>` and makes each missing folder. `root` must be a
/// trusted host folder. A link on the way fails the open.
pub fn open_parts(root: &Path, parts: &[&str]) -> std::io::Result<OwnedFd> {
    let mut dir = rustix::fs::open(root, dir_flags(), Mode::empty())?;
    for part in parts {
        if part.is_empty() || *part == "." || *part == ".." || part.contains('/') {
            return Err(std::io::Error::other(format!("bad folder name {part}")));
        }
        match rustix::fs::mkdirat(&dir, *part, Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        dir = rustix::fs::openat(&dir, *part, dir_flags(), Mode::empty())?;
    }
    Ok(dir)
}

/// The type of the entry `name` in `dir`, not of a link's target. `None` if
/// it does not exist.
pub fn type_at(dir: &OwnedFd, name: impl rustix::path::Arg) -> std::io::Result<Option<FileType>> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(FileType::from_raw_mode(stat.st_mode))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The target of the link `name` in `dir`, or `None` if it is no link.
pub fn link_at(dir: &OwnedFd, name: &str) -> std::io::Result<Option<String>> {
    if type_at(dir, name)? != Some(FileType::Symlink) {
        return Ok(None);
    }
    let target = rustix::fs::readlinkat(dir, name, Vec::new())?;
    Ok(Some(target.to_string_lossy().into_owned()))
}

/// Removes the entry `name` in `dir`, a folder with all its content, and
/// follows no link: a link is removed, not its target. No entry is not an
/// error. The walk uses its own stack, so a deep tree that the guest made
/// cannot overflow the thread's stack. A tree deeper than 64 folders fails,
/// so at most 64 folders are open.
pub fn remove_at(dir: &OwnedFd, name: &str) -> std::io::Result<()> {
    let name = CString::new(name).map_err(std::io::Error::other)?;
    match type_at(dir, name.as_c_str())? {
        None => return Ok(()),
        Some(FileType::Directory) => {}
        Some(_) => return Ok(rustix::fs::unlinkat(dir, name.as_c_str(), AtFlags::empty())?),
    }
    // Each open folder, its name in its parent and the children to remove.
    let mut stack = vec![open_level(dir, name)?];
    loop {
        let depth = stack.len();
        let Some((top, _, children)) = stack.last_mut() else {
            return Ok(());
        };
        let Some(child) = children.pop() else {
            let (fd, name, _) = stack.pop().expect("a level");
            drop(fd);
            let parent = stack.last().map_or(dir, |(fd, ..)| fd);
            rustix::fs::unlinkat(parent, name.as_c_str(), AtFlags::REMOVEDIR)?;
            continue;
        };
        match type_at(top, child.as_c_str())? {
            None => {}
            Some(FileType::Directory) => {
                if depth >= REMOVE_DEPTH {
                    return Err(std::io::Error::other(format!(
                        "a folder in the project state is nested more than {REMOVE_DEPTH} levels deep"
                    )));
                }
                let level = open_level(top, child)?;
                stack.push(level);
            }
            Some(_) => rustix::fs::unlinkat(&*top, child.as_c_str(), AtFlags::empty())?,
        }
    }
}

/// The deepest folder tree that [`remove_at`] removes.
const REMOVE_DEPTH: usize = 64;

/// Opens the folder `name` in `dir` and reads the names in it.
fn open_level(dir: &OwnedFd, name: CString) -> std::io::Result<(OwnedFd, CString, Vec<CString>)> {
    let fd = rustix::fs::openat(dir, name.as_c_str(), dir_flags(), Mode::empty())?;
    let children = Dir::read_from(&fd)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_owned())
        .filter(|n| n.as_bytes() != b"." && n.as_bytes() != b"..")
        .collect();
    Ok((fd, name, children))
}

/// Replaces the entry `name` in `dir` with a link to `target`.
pub fn replace_with_link(dir: &OwnedFd, name: &str, target: &str) -> std::io::Result<()> {
    remove_at(dir, name)?;
    Ok(rustix::fs::symlinkat(target, dir, name)?)
}

/// Writes `bytes` to a new file in `dir` and renames it over `name`, so a
/// link at `name` is replaced and not followed.
pub fn write_at(dir: &OwnedFd, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = format!(".zwinger-{}", uuid::Uuid::new_v4());
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(dir, tmp.as_str(), flags, Mode::from_raw_mode(0o600))?;
    let written = std::fs::File::from(fd).write_all(bytes);
    let renamed = written.and_then(|()| {
        if type_at(dir, name)? == Some(FileType::Directory) {
            remove_at(dir, name)?;
        }
        Ok(rustix::fs::renameat(dir, tmp.as_str(), dir, name)?)
    });
    if renamed.is_err() {
        let _ = rustix::fs::unlinkat(dir, tmp.as_str(), AtFlags::empty());
    }
    renamed
}

#[cfg(test)]
pub mod fake {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// A host filesystem from maps. Each `files` key is also a file for
    /// `is_file`; the parents of each path are folders.
    #[derive(Default)]
    pub struct FakeFs {
        pub home: PathBuf,
        pub path: Vec<PathBuf>,
        pub files: HashMap<PathBuf, String>,
        pub links: HashMap<PathBuf, PathBuf>,
        pub dirs: HashSet<PathBuf>,
        /// The `--version` output per binary.
        pub versions: HashMap<PathBuf, String>,
    }

    impl FakeFs {
        pub fn file(&mut self, path: &str, text: &str) {
            self.files.insert(PathBuf::from(path), text.to_owned());
        }
    }

    impl HostFs for FakeFs {
        fn home(&self) -> PathBuf {
            self.home.clone()
        }

        fn path_dirs(&self) -> Vec<PathBuf> {
            self.path.clone()
        }

        fn is_file(&self, path: &Path) -> bool {
            self.files.contains_key(path) || self.versions.contains_key(path)
        }

        fn is_dir(&self, path: &Path) -> bool {
            self.dirs.contains(path) || self.files.keys().any(|f| f.starts_with(path) && f != path)
        }

        fn read_link(&self, path: &Path) -> Option<PathBuf> {
            self.links.get(path).cloned()
        }

        fn read_to_string(&self, path: &Path) -> Option<String> {
            self.files.get(path).cloned()
        }

        fn list_dir(&self, path: &Path) -> Vec<String> {
            let mut names: Vec<String> = self
                .files
                .keys()
                .chain(self.dirs.iter())
                .filter_map(|p| p.strip_prefix(path).ok()?.components().next())
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names.dedup();
            names
        }

        fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
            Some(self.links.get(path).cloned().unwrap_or_else(|| path.to_owned()))
        }

        fn version(&self, bin: &Path) -> Option<String> {
            self.versions.get(bin).cloned()
        }
    }

    pub struct FakeClock(pub std::sync::atomic::AtomicI64);

    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-host-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn writes_follow_no_link_that_the_guest_planted() {
        let root = temp_dir();
        let outside = temp_dir();
        std::fs::write(outside.join("keep"), "host file").unwrap();
        // A planted link as a folder on the way fails the open.
        std::os::unix::fs::symlink(&outside, root.join("codex")).unwrap();
        assert!(open_parts(&root, &["codex"]).is_err());
        std::fs::remove_file(root.join("codex")).unwrap();

        let dir = open_parts(&root, &["codex"]).unwrap();
        // A planted link at the file name is replaced, not written through.
        std::os::unix::fs::symlink(outside.join("keep"), root.join("codex/auth.json")).unwrap();
        write_at(&dir, "auth.json", b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(outside.join("keep")).unwrap(), "host file");
        assert!(!std::fs::symlink_metadata(root.join("codex/auth.json")).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(root.join("codex/auth.json")).unwrap(), "{}");

        // A folder with a link inside is removed, the link's target stays.
        std::fs::create_dir_all(root.join("codex/skills/a")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("codex/skills/a/out")).unwrap();
        replace_with_link(&dir, "skills", "/opt/zwinger/profile/codex/skills").unwrap();
        assert_eq!(link_at(&dir, "skills").unwrap().as_deref(), Some("/opt/zwinger/profile/codex/skills"));
        assert!(outside.join("keep").exists());
        assert_eq!(link_at(&dir, "auth.json").unwrap(), None);
        remove_at(&dir, "skills").unwrap();
        remove_at(&dir, "missing").unwrap();
        assert_eq!(type_at(&dir, "skills").unwrap(), None);
    }

    #[test]
    fn a_deep_tree_fails_the_removal_and_does_not_crash() {
        let root = temp_dir();
        let dir = open_parts(&root, &[]).unwrap();
        let nest = |name: &str, depth: usize| {
            let mut path = root.join(name);
            for _ in 0..depth {
                path.push("d");
            }
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("f"), "x").unwrap();
        };
        nest("ok", 63);
        remove_at(&dir, "ok").unwrap();
        assert_eq!(type_at(&dir, "ok").unwrap(), None);
        nest("deep", 200);
        let err = remove_at(&dir, "deep").unwrap_err();
        assert!(err.to_string().contains("more than 64 levels"), "{err}");
    }
}
