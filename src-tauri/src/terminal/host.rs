//! Host shells: the user's login shell on the Mac, in a PTY of this process.

use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::runtime::{PTY_QUEUE, Pty, PtyEvent, PtyInput, SessionKind};

/// How long the exit waits for the last output after the shell exits. A
/// background job can keep the terminal open after its shell has exited.
const DRAIN_LIMIT: Duration = Duration::from_secs(1);

/// How often the reader checks for the stop while no output comes.
const POLL_TICK_MS: libc::c_int = 100;

/// How long a hung-up shell has to hang up its jobs and exit before its
/// group is killed.
pub(crate) const HANGUP_LIMIT: Duration = Duration::from_millis(500);

/// `$SHELL`, else the shell of the user's passwd entry, else `/bin/zsh`.
pub(crate) fn login_shell() -> String {
    let executable = |path: &str| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if let Ok(shell) = std::env::var("SHELL")
        && executable(&shell)
    {
        return shell;
    }
    // SAFETY: getpwuid returns null or a pointer to a static entry that stays
    // valid until the next passwd call; the shell is copied at once.
    let passwd = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        (!entry.is_null() && !(*entry).pw_shell.is_null())
            .then(|| CStr::from_ptr((*entry).pw_shell).to_string_lossy().into_owned())
    };
    passwd.filter(|s| executable(s)).unwrap_or_else(|| "/bin/zsh".to_owned())
}

/// The process group of a host shell. The shell is a session leader
/// (setsid), so its group id is its pid. The id is retired before the shell
/// is reaped: after the reap, the system can give it to a different group.
#[derive(Clone)]
pub(crate) struct HostGroup(Arc<Mutex<Option<libc::pid_t>>>);

impl HostGroup {
    /// Sends SIGHUP to the shell if it is not reaped. An interactive shell
    /// then hangs up its jobs, which run in their own groups, and exits.
    pub(crate) fn hangup(&self) {
        if let Some(pid) = *self.lock() {
            // SAFETY: a plain syscall. The unreaped shell holds the pid.
            unsafe {
                libc::kill(pid, libc::SIGHUP);
            }
        }
    }

    /// Kills the group if its shell is not reaped. It needs no async runtime.
    pub(crate) fn kill(&self) {
        if let Some(pgid) = *self.lock() {
            // SAFETY: a plain syscall. The unreaped leader holds the id, so
            // the group is still the shell's.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }

    /// The shell is reaped.
    pub(crate) fn is_retired(&self) -> bool {
        self.lock().is_none()
    }

    /// Hangs up the shell, and kills its group in a thread if the shell has
    /// not exited after [`HANGUP_LIMIT`].
    pub(crate) fn terminate(&self) {
        self.hangup();
        let group = self.clone();
        std::thread::spawn(move || {
            wait_retired(std::slice::from_ref(&group), std::time::Instant::now() + HANGUP_LIMIT);
            group.kill();
        });
    }

    /// The current folder of the shell, if it is not reaped and macOS tells it.
    pub(crate) fn cwd(&self) -> Option<PathBuf> {
        let pid = self.lock();
        let pid = (*pid)?;
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes to the buffer. The
        // held lock keeps the shell unreaped, so the pid is still the shell's.
        let written =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, info.as_mut_ptr().cast(), size) };
        if written != size {
            return None;
        }
        // SAFETY: the buffer was zeroed and then written in full.
        let info = unsafe { info.assume_init() };
        let bytes: Vec<u8> =
            info.pvi_cdir.vip_path.as_flattened().iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        (!bytes.is_empty()).then(|| PathBuf::from(OsString::from_vec(bytes)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<libc::pid_t>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn id(&self) -> Option<libc::pid_t> {
        *self.0.lock().unwrap()
    }
}

/// Waits until each shell of `groups` is reaped, or until `deadline`.
pub(crate) fn wait_retired(groups: &[HostGroup], deadline: std::time::Instant) {
    while !groups.iter().all(HostGroup::is_retired) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The `LANG` for a shell whose env sets no locale, as from Finder: zsh in
/// the C locale breaks UTF-8 text and Nerd Font glyphs.
fn fallback_lang(var: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    ["LANG", "LC_ALL", "LC_CTYPE"].iter().all(|name| var(name).is_none_or(|v| v.is_empty())).then_some("en_US.UTF-8")
}

/// Starts the login shell in `folder`. Returns the PTY and the shell's
/// process group, so an app exit can kill it without the async runtime.
/// The env is the app's env with the terminal names; it has no proxy and
/// no VM env.
pub(crate) fn open_host_pty(folder: &Path, rows: u16, cols: u16) -> Result<(Pty, HostGroup)> {
    let pair =
        native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(pty_error)?;
    let mut command = CommandBuilder::from_argv(SessionKind::Host.argv().into_iter().map(Into::into).collect());
    command.cwd(folder);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "zwinger");
    if let Some(lang) = fallback_lang(|name| std::env::var(name).ok()) {
        command.env("LANG", lang);
    }
    let child = pair.slave.spawn_command(command).map_err(pty_error)?;
    // The reader sees the end only when no process holds the slave side.
    drop(pair.slave);
    let pid = child.process_id().ok_or(Error::SessionClosed)? as libc::pid_t;
    let group = HostGroup(Arc::new(Mutex::new(Some(pid))));
    let master = pair.master;
    // An own copy of the master fd: the reader polls it, so it can stop
    // while a background job keeps the terminal open.
    let master_fd = master.as_raw_fd().ok_or(Error::SessionClosed)?;
    // SAFETY: the master is open while this function runs.
    let reader = File::from(unsafe { BorrowedFd::borrow_raw(master_fd) }.try_clone_to_owned()?);
    let writer = master.take_writer().map_err(pty_error)?;

    let (input, mut input_rx) = mpsc::unbounded_channel();
    let (events_tx, events) = mpsc::channel(PTY_QUEUE);
    let (data_tx, data_rx) = std_mpsc::channel::<Vec<u8>>();
    let (drained_tx, drained_rx) = std_mpsc::channel::<()>();
    let (exited_tx, mut exited_rx) = oneshot::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));

    // A write blocks while the process does not read, so it has its own
    // thread; a resize or kill never waits behind it.
    std::thread::spawn(move || {
        let mut writer = writer;
        while let Ok(data) = data_rx.recv() {
            if writer.write_all(&data).and_then(|()| writer.flush()).is_err() {
                break;
            }
        }
    });
    // The task ends at the shell exit. It drops the master and the data
    // sender, so the writer thread ends too.
    let input_group = group.clone();
    tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                msg = input_rx.recv() => msg,
                _ = &mut exited_rx => None,
            };
            let Some(msg) = msg else { break };
            // A failed write or resize only means the process is gone; the
            // wait thread reports the exit.
            match msg {
                PtyInput::Data(data) => {
                    let _ = data_tx.send(data);
                }
                PtyInput::Resize { rows, cols } => {
                    let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                }
                PtyInput::Kill => input_group.terminate(),
            }
        }
    });
    let output_tx = events_tx.clone();
    let reader_stop = stop.clone();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0; 64 << 10];
        while !reader_stop.load(Ordering::Acquire) {
            let mut fd = libc::pollfd { fd: reader.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd on an fd that this thread owns.
            match unsafe { libc::poll(&mut fd, 1, POLL_TICK_MS) } {
                0 => continue,
                ..0 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => continue,
                ..0 => break,
                _ => {}
            }
            // A read error is the end: macOS gives EIO when the slave closes.
            let Ok(n @ 1..) = reader.read(&mut buf) else { break };
            // The pump drops the receiver after the exit.
            if output_tx.blocking_send(PtyEvent::Output(buf[..n].to_vec())).is_err() {
                break;
            }
        }
        let _ = drained_tx.send(());
    });
    let wait_group = group.clone();
    std::thread::spawn(move || {
        let mut child = child;
        // Retire the group id while the exited shell is not reaped yet.
        wait_unreaped(pid);
        let code = {
            let mut id = wait_group.lock();
            *id = None;
            match child.wait() {
                Ok(_) => exit_code(&mut child),
                Err(_) => -1,
            }
        };
        let _ = exited_tx.send(());
        let _ = drained_rx.recv_timeout(DRAIN_LIMIT);
        stop.store(true, Ordering::Release);
        let _ = events_tx.blocking_send(PtyEvent::Exit(code));
    });

    Ok((Pty { input, events }, group))
}

/// Waits until the process `pid` exits, and does not reap it.
fn wait_unreaped(pid: libc::pid_t) {
    loop {
        // SAFETY: an all-zero siginfo_t is valid, and waitid only writes it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: a plain syscall on our own child with a valid out pointer.
        let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if r == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

fn pty_error(e: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}

/// The code of the reaped shell, or 128 + its signal, as a shell reports it.
fn exit_code(child: &mut Box<dyn portable_pty::Child + Send + Sync>) -> i32 {
    let child: &mut dyn portable_pty::Child = child.as_mut();
    match child.downcast_mut::<std::process::Child>().and_then(|c| c.try_wait().ok().flatten()) {
        Some(status) => status.code().or(status.signal().map(|s| 128 + s)).unwrap_or(-1),
        None => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cage::testing::host_sh;

    async fn read_until_exit(pty: &mut Pty) -> (String, i32) {
        let mut out = Vec::new();
        let read = async {
            while let Some(event) = pty.events.recv().await {
                match event {
                    PtyEvent::Output(bytes) => out.extend(bytes),
                    PtyEvent::Exit(code) => return code,
                }
            }
            panic!("no exit");
        };
        let code = tokio::time::timeout(Duration::from_secs(10), read).await.expect("the shell did not exit");
        (String::from_utf8_lossy(&out).into_owned(), code)
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-host-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[tokio::test]
    async fn a_host_shell_runs_in_the_folder_and_reports_its_exit() {
        let dir = temp_dir();
        let (mut pty, _) = {
            host_sh();
            open_host_pty(&dir, 24, 80)
        }
        .unwrap();
        pty.input.send(PtyInput::Data(b"echo \"in $(pwd) as $TERM_PROGRAM\"; exit 3\n".to_vec())).unwrap();
        let (out, code) = read_until_exit(&mut pty).await;
        assert!(out.contains(&format!("in {} as zwinger", dir.display())), "{out}");
        assert_eq!(code, 3);
    }

    #[tokio::test]
    async fn kill_ends_the_process_group_and_the_background_jobs() {
        let dir = temp_dir();
        let (mut pty, group) = {
            host_sh();
            open_host_pty(&dir, 24, 80)
        }
        .unwrap();
        let pgid = group.id().unwrap();
        pty.input.send(PtyInput::Data(b"sleep 1000 & echo job=$!\n".to_vec())).unwrap();
        let mut out = String::new();
        let job: libc::pid_t = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match pty.events.recv().await {
                    Some(PtyEvent::Output(bytes)) => out.push_str(&String::from_utf8_lossy(&bytes)),
                    event => panic!("no job pid: {event:?} after {out}"),
                }
                // The echo of the typed line has `job=$!`, the output the
                // pid and the line end.
                let pid = out.split("job=").skip(1).find_map(|s| {
                    let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                    (digits > 0 && digits < s.len()).then(|| s[..digits].parse().ok()).flatten()
                });
                if let Some(pid) = pid {
                    return pid;
                }
            }
        })
        .await
        .expect("no job pid");
        pty.input.send(PtyInput::Kill).unwrap();
        read_until_exit(&mut pty).await;
        assert_eq!(group.id(), None, "the reaped shell's group id is retired");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only checks that the group and the job exist.
        while unsafe { libc::killpg(pgid, 0) == 0 || libc::kill(job, 0) == 0 } {
            assert!(std::time::Instant::now() < deadline, "the group or the job is still running");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_shell_with_no_locale_gets_utf8() {
        let env = |set: &'static [(&str, &str)]| {
            move |name: &str| set.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_owned())
        };
        assert_eq!(fallback_lang(env(&[])), Some("en_US.UTF-8"));
        assert_eq!(fallback_lang(env(&[("LANG", "")])), Some("en_US.UTF-8"));
        assert_eq!(fallback_lang(env(&[("LANG", "de_CH.UTF-8")])), None);
        assert_eq!(fallback_lang(env(&[("LC_ALL", "C")])), None);
        assert_eq!(fallback_lang(env(&[("LC_CTYPE", "UTF-8")])), None);
    }
}
