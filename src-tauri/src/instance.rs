//! One app instance per app data folder. A second instance with the same data
//! would reap the VMs of the first and write the same stores.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const UPDATE_HELPER_ARG: &str = "--zwinger-update-handoff";

/// Holds the lock until it is dropped or the process exits.
pub struct InstanceLock {
    #[allow(dead_code)]
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `None`: a different process holds the lock. The lock file is never deleted.
pub fn acquire(data: &Path) -> io::Result<Option<InstanceLock>> {
    let path = data.join("zwinger.lock");
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    let file = OpenOptions::new().create(true).write(true).truncate(false).open(&path).map_err(with_path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(InstanceLock { file, path })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(with_path(e)),
    }
}

pub fn bundle_path(exe: &Path) -> io::Result<&Path> {
    let macos = exe.parent().filter(|dir| dir.file_name() == Some(OsStr::new("MacOS")));
    let contents = macos.and_then(Path::parent).filter(|dir| dir.file_name() == Some(OsStr::new("Contents")));
    contents
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("the updated executable is outside a macOS app bundle"))
}

pub fn bundle_executable(bundle: &Path) -> io::Result<PathBuf> {
    let info =
        plist::from_file::<_, plist::Dictionary>(bundle.join("Contents/Info.plist")).map_err(io::Error::other)?;
    let name = info
        .get("CFBundleExecutable")
        .and_then(plist::Value::as_string)
        .ok_or_else(|| io::Error::other("Info.plist has no CFBundleExecutable"))?;
    Ok(bundle.join("Contents/MacOS").join(name))
}

fn wait_for_handoff(lock_path: &Path, mut input: impl io::Read) -> io::Result<()> {
    io::copy(&mut input, &mut io::sink())?;
    let file = OpenOptions::new().write(true).open(lock_path)?;
    file.lock()?;
    drop(file);
    Ok(())
}

fn relaunch(bundle: &Path, original_args: &[OsString]) -> io::Result<()> {
    let open_error = match Command::new("/usr/bin/open")
        .arg("-n")
        .arg(bundle)
        .arg("--args")
        .args(original_args)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => format!(
            "LaunchServices could not reopen zwinger ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => format!("LaunchServices could not reopen zwinger: {e}"),
    };
    let direct = bundle_executable(bundle).and_then(|exe| {
        Command::new(exe)
            .args(original_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map(|_| ())
    });
    direct.map_err(|e| io::Error::other(format!("{open_error}; direct launch failed: {e}")))
}

/// Returns true if this process was started as the private update helper.
pub fn run_update_helper() -> io::Result<bool> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(UPDATE_HELPER_ARG)) {
        return Ok(false);
    }
    let lock = args.next().ok_or_else(|| io::Error::other("update helper has no lock path"))?;
    let bundle = args.next().ok_or_else(|| io::Error::other("update helper has no bundle path"))?;
    let original_args: Vec<OsString> = args.collect();
    wait_for_handoff(Path::new(&lock), io::stdin().lock())?;
    relaunch(Path::new(&bundle), &original_args)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Read as _};
    use std::os::fd::AsRawFd as _;
    use std::path::PathBuf;
    use std::process::{Child, ChildStdout};
    use std::time::{Duration, Instant};

    const HOLDER_DIR: &str = "ZWINGER_LOCK_HOLDER_DIR";
    const HELPER_LOCK: &str = "ZWINGER_HELPER_LOCK";

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[ignore = "run by the lock tests"]
    fn holder() {
        let dir = PathBuf::from(std::env::var(HOLDER_DIR).unwrap());
        let _lock = acquire(&dir).unwrap().expect("the lock is free");
        println!("locked");
        let _ = io::stdin().read_to_end(&mut Vec::new());
    }

    #[test]
    #[ignore = "run by the handoff tests"]
    fn helper() {
        let lock = PathBuf::from(std::env::var(HELPER_LOCK).unwrap());
        println!("waiting");
        wait_for_handoff(&lock, io::stdin().lock()).unwrap();
        println!("ready");
    }

    fn spawn_test(name: &str, env_key: &str, path: &Path) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env(env_key, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn wait_for_line(child: &mut Child, expected: &str) -> BufReader<ChildStdout> {
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.trim_end().ends_with(expected) {
            line.clear();
            assert_ne!(output.read_line(&mut line).unwrap(), 0, "the child stopped before {expected}");
        }
        output
    }

    fn wait_for_exit(child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "child exited with {status}");
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("child did not exit after the handoff");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn contention_is_nonblocking_and_lock_returns_after_exit() {
        let dir = dir("lock-child");
        let mut child = spawn_test("instance::tests::holder", HOLDER_DIR, &dir);
        let _output = wait_for_line(&mut child, "locked");

        let start = Instant::now();
        assert!(acquire(&dir).unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(child.stdin.take());
        wait_for_exit(&mut child);
        assert!(acquire(&dir).unwrap().is_some());
    }

    #[test]
    fn handoff_waits_for_pipe_and_lock() {
        let dir = dir("handoff");
        let mut holder = spawn_test("instance::tests::holder", HOLDER_DIR, &dir);
        let _holder_output = wait_for_line(&mut holder, "locked");
        let mut helper = spawn_test("instance::tests::helper", HELPER_LOCK, &dir.join("zwinger.lock"));
        let _helper_output = wait_for_line(&mut helper, "waiting");

        assert!(helper.try_wait().unwrap().is_none());
        drop(helper.stdin.take());
        std::thread::sleep(Duration::from_millis(100));
        assert!(helper.try_wait().unwrap().is_none());

        drop(holder.stdin.take());
        wait_for_exit(&mut holder);
        wait_for_exit(&mut helper);
    }

    #[test]
    fn helper_does_not_inherit_the_lock_fd() {
        let dir = dir("handoff-fd");
        let lock = acquire(&dir).unwrap().unwrap();
        let flags = unsafe { libc::fcntl(lock.file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);

        let mut helper = spawn_test("instance::tests::helper", HELPER_LOCK, lock.path());
        let _output = wait_for_line(&mut helper, "waiting");
        drop(helper.stdin.take());
        drop(lock);
        wait_for_exit(&mut helper);
    }

    #[test]
    fn the_same_process_acquires_again_after_drop() {
        let dir = dir("lock-again");
        let lock = acquire(&dir).unwrap();
        assert!(lock.is_some());
        drop(lock);
        assert!(acquire(&dir).unwrap().is_some());
    }

    #[test]
    fn two_folders_have_two_locks() {
        let (a, b) = (dir("lock-a"), dir("lock-b"));
        let _a = acquire(&a).unwrap().unwrap();
        assert!(acquire(&b).unwrap().is_some());
    }

    #[test]
    fn a_file_in_place_of_the_folder_is_an_error() {
        let file = dir("lock-file").join("data");
        std::fs::write(&file, "").unwrap();
        let err = acquire(&file).err().expect("an error");
        assert!(err.to_string().contains("zwinger.lock"), "{err}");
    }

    #[test]
    fn bundle_executable_uses_the_updated_plist_name() {
        let bundle = dir("renamed-executable").join("zwinger.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        let mut info = plist::Dictionary::new();
        info.insert("CFBundleExecutable".into(), "zwinger-new".into());
        plist::to_file_xml(bundle.join("Contents/Info.plist"), &info).unwrap();

        assert_eq!(bundle_executable(&bundle).unwrap(), bundle.join("Contents/MacOS/zwinger-new"));
    }
}
