//! Live top-level folders of a project folder, for the project map.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::cage;
use crate::error::{Error, Result};

/// The quiet time that ends a burst of changes.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// The longest time that a list waits for a burst to end.
const MAX_DELAY: Duration = Duration::from_millis(500);
/// How often the worker reads the root without a signal. This finds a
/// removed root, a root that came back, and changes that the OS watcher did
/// not report.
const POLL: Duration = Duration::from_secs(1);

/// The signals to a worker. Flags, so that any number of changes takes the same storage.
#[derive(Default)]
struct Flags {
    changed: bool,
    rescan: bool,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    flags: Mutex<Flags>,
    wake: Condvar,
}

#[derive(PartialEq)]
enum Wake {
    Stop,
    Changed,
    Rescan,
    Poll,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, signal: impl FnOnce(&mut Flags)) {
        signal(&mut self.lock());
        self.wake.notify_one();
    }

    /// Waits for a signal, or for `POLL`, and takes it.
    fn wait(&self) -> Wake {
        let idle = |f: &mut Flags| !f.stop && !f.rescan && !f.changed;
        let (mut flags, _) =
            self.wake.wait_timeout_while(self.lock(), POLL, idle).unwrap_or_else(PoisonError::into_inner);
        let wake = if flags.stop {
            Wake::Stop
        } else if flags.rescan {
            Wake::Rescan
        } else if flags.changed {
            Wake::Changed
        } else {
            Wake::Poll
        };
        flags.rescan = false;
        flags.changed = false;
        wake
    }

    /// Waits until the changes are quiet for `DEBOUNCE`, but not longer than
    /// `MAX_DELAY`, so continuous changes cannot hold back the list.
    /// False if the watch stops meanwhile.
    fn settle(&self) -> bool {
        let deadline = Instant::now() + MAX_DELAY;
        let mut flags = self.lock();
        loop {
            if flags.stop {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if flags.rescan || left.is_zero() {
                flags.rescan = false;
                return true;
            }
            flags.changed = false;
            let (next, quiet) = self
                .wake
                .wait_timeout_while(flags, DEBOUNCE.min(left), |f| !f.stop && !f.rescan && !f.changed)
                .unwrap_or_else(PoisonError::into_inner);
            flags = next;
            if quiet.timed_out() {
                return true;
            }
        }
    }
}

/// Attaches an OS watcher to `root` that signals each change to `shared`.
/// The worker owns the result: when the worker ends, the OS watcher ends.
type Attach = fn(&Path, Weak<Shared>) -> Result<Box<dyn Send>>;

fn attach_notify(root: &Path, shared: Weak<Shared>) -> Result<Box<dyn Send>> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if !matches!(&event, Ok(e) if e.kind.is_access())
            && let Some(shared) = shared.upgrade()
        {
            shared.set(|f| f.changed = true);
        }
    })
    .map_err(|e| Error::FolderWatch(e.to_string()))?;
    watcher.watch(root, RecursiveMode::NonRecursive).map_err(|e| Error::FolderWatch(e.to_string()))?;
    Ok(Box::new(watcher))
}

/// A running watch. A drop stops the worker, which then drops the OS watcher.
pub struct FolderWatch {
    shared: Arc<Shared>,
}

impl Drop for FolderWatch {
    fn drop(&mut self) {
        self.shared.set(|f| f.stop = true);
    }
}

/// An attached watch with its first list, before its worker starts.
struct Ready {
    shared: Arc<Shared>,
    root: PathBuf,
    attach: Attach,
    watcher: Box<dyn Send>,
    first: Vec<String>,
}

/// Attaches the watcher to `root`, then reads the first list, so a change
/// between the two is not lost. A missing root or a failed attach is an error.
fn prepare(root: PathBuf, attach: Attach) -> Result<Ready> {
    let shared = Arc::new(Shared::default());
    let watcher = attach(&root, Arc::downgrade(&shared))?;
    let first = cage::list_folders(&root)?;
    Ok(Ready { shared, root, attach, watcher, first })
}

impl Ready {
    /// Starts the worker. It sends the first list, and then each list that
    /// differs from the last one, until the watch drops or `send` returns
    /// false. `None` means that the root cannot be read. Then it drops the
    /// OS watcher and calls `on_exit`.
    fn spawn(
        self,
        mut send: impl FnMut(Option<Vec<String>>) -> bool + Send + 'static,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> FolderWatch {
        let Ready { shared, root, attach, mut watcher, first } = self;
        let worker = shared.clone();
        std::thread::spawn(move || {
            let mut last = Some(first);
            // True when the last attach failed. Each later wake tries again.
            let mut detached = false;
            if send(last.clone()) {
                loop {
                    let wake = worker.wait();
                    if wake == Wake::Stop || (wake == Wake::Changed && !worker.settle()) {
                        break;
                    }
                    let mut folders = cage::list_folders(&root).ok();
                    // A root that came back, or a read that finds a change
                    // that the watcher did not report, gets a new watcher.
                    // The read after the attach finds a change between the two.
                    if folders.is_some() && (last.is_none() || detached || (wake != Wake::Changed && folders != last)) {
                        match attach(&root, Arc::downgrade(&worker)) {
                            Ok(new) => {
                                watcher = new;
                                detached = false;
                                folders = cage::list_folders(&root).ok();
                            }
                            Err(_) => detached = true,
                        }
                    }
                    if folders != last {
                        if !send(folders.clone()) {
                            break;
                        }
                        last = folders;
                    }
                }
            }
            drop(watcher);
            drop(send);
            on_exit();
        });
        FolderWatch { shared }
    }
}

/// The folder watches of the webviews, by token, with the label of their webview.
#[derive(Default)]
pub struct FolderWatches {
    next: AtomicU64,
    watches: Arc<Mutex<HashMap<u64, (String, FolderWatch)>>>,
}

fn lock(watches: &Mutex<HashMap<u64, (String, FolderWatch)>>) -> MutexGuard<'_, HashMap<u64, (String, FolderWatch)>> {
    watches.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FolderWatches {
    /// Watches the direct children of `root` for the webview `owner`. The
    /// watch ends at `remove`, at `drop_owner`, or when `send` returns false.
    pub fn watch(
        &self,
        owner: &str,
        root: PathBuf,
        send: impl FnMut(Option<Vec<String>>) -> bool + Send + 'static,
    ) -> Result<u64> {
        self.watch_with(owner, root, attach_notify, send)
    }

    fn watch_with(
        &self,
        owner: &str,
        root: PathBuf,
        attach: Attach,
        send: impl FnMut(Option<Vec<String>>) -> bool + Send + 'static,
    ) -> Result<u64> {
        let ready = prepare(root, attach)?;
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let watches = Arc::downgrade(&self.watches);
        // A worker that ends removes its own entry. The spawn is under the
        // lock, so the entry exists before the worker can remove it.
        let on_exit = move || {
            if let Some(watches) = watches.upgrade() {
                let gone = lock(&watches).remove(&token);
                drop(gone);
            }
        };
        let mut map = lock(&self.watches);
        map.insert(token, (owner.to_owned(), ready.spawn(send, on_exit)));
        Ok(token)
    }

    pub fn remove(&self, token: u64) {
        let gone = lock(&self.watches).remove(&token);
        drop(gone);
    }

    /// Reads the folders again now. An error if the watch has ended.
    pub fn rescan(&self, token: u64) -> Result<()> {
        let watches = lock(&self.watches);
        let (_, watch) = watches.get(&token).ok_or_else(|| Error::FolderWatch("the watch has ended".into()))?;
        watch.shared.set(|f| f.rescan = true);
        Ok(())
    }

    /// Stops the watches of a webview that closed or loaded a new page.
    pub fn drop_owner(&self, owner: &str) {
        let gone: Vec<_> = {
            let mut watches = lock(&self.watches);
            let tokens: Vec<u64> = watches.iter().filter(|(_, (o, _))| o == owner).map(|(t, _)| *t).collect();
            tokens.into_iter().filter_map(|t| watches.remove(&t)).collect()
        };
        drop(gone);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

    /// Long enough for FSEvents to report a change.
    const WAIT: Duration = Duration::from_secs(5);
    /// Long enough that a change that was to come has come.
    const QUIET: Duration = Duration::from_millis(1500);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-folders-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// An OS watcher that reports nothing. The test signals the changes.
    fn attach_none(_: &Path, _: Weak<Shared>) -> Result<Box<dyn Send>> {
        Ok(Box::new(()))
    }

    fn start(root: &Path, attach: Attach) -> (FolderWatch, Receiver<Option<Vec<String>>>) {
        let (tx, rx) = mpsc::channel();
        let watch = prepare(root.to_owned(), attach).unwrap().spawn(move |folders| tx.send(folders).is_ok(), || {});
        (watch, rx)
    }

    fn watch(root: &Path) -> (FolderWatch, Receiver<Option<Vec<String>>>) {
        start(root, attach_notify)
    }

    fn frame(rx: &Receiver<Option<Vec<String>>>) -> Option<Vec<String>> {
        rx.recv_timeout(WAIT).expect("a frame in time")
    }

    fn next(rx: &Receiver<Option<Vec<String>>>) -> Vec<String> {
        frame(rx).expect("a list of a readable root")
    }

    fn until(done: impl Fn() -> bool) {
        let end = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < end, "not in time");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn len(watches: &FolderWatches) -> usize {
        lock(&watches.watches).len()
    }

    #[test]
    fn sends_the_first_list() {
        let root = temp_dir();
        std::fs::create_dir(root.join("api")).unwrap();
        let (_watch, rx) = watch(&root);
        assert_eq!(next(&rx), ["api"]);
    }

    #[test]
    fn follows_a_subdirectory_created_renamed_and_deleted() {
        let root = temp_dir();
        let (_watch, rx) = watch(&root);
        assert!(next(&rx).is_empty());
        std::fs::create_dir(root.join("web")).unwrap();
        assert_eq!(next(&rx), ["web"]);
        std::fs::rename(root.join("web"), root.join("app")).unwrap();
        assert_eq!(next(&rx), ["app"]);
        std::fs::remove_dir(root.join("app")).unwrap();
        assert!(next(&rx).is_empty());
    }

    #[test]
    fn ignores_a_file_and_a_nested_directory() {
        let root = temp_dir();
        std::fs::create_dir(root.join("web")).unwrap();
        let (_watch, rx) = watch(&root);
        assert_eq!(next(&rx), ["web"]);
        std::fs::write(root.join("notes.txt"), "x").unwrap();
        std::fs::create_dir(root.join("web").join("src")).unwrap();
        assert!(rx.recv_timeout(QUIET).is_err());
    }

    #[test]
    fn sends_one_final_list_for_a_burst() {
        let root = temp_dir();
        let (_watch, rx) = watch(&root);
        assert!(next(&rx).is_empty());
        for name in ["a", "b", "c", "d"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        assert_eq!(next(&rx), ["a", "b", "c", "d"]);
        assert!(rx.recv_timeout(QUIET).is_err());
    }

    #[test]
    fn continuous_changes_do_not_hold_back_the_list() {
        let root = temp_dir();
        let (watch, rx) = start(&root, attach_none);
        assert!(next(&rx).is_empty());
        std::fs::create_dir(root.join("web")).unwrap();
        let begin = Instant::now();
        // A change each 20 ms, which is less than the debounce time.
        let list = loop {
            watch.shared.set(|f| f.changed = true);
            if let Ok(list) = rx.recv_timeout(Duration::from_millis(20)) {
                break list;
            }
            assert!(begin.elapsed() < WAIT, "the changes held back the list");
        };
        assert_eq!(list, Some(vec!["web".to_owned()]));
        assert!(begin.elapsed() < MAX_DELAY + DEBOUNCE * 2);
    }

    #[test]
    fn a_rescan_reads_a_change_that_the_watcher_missed() {
        let root = temp_dir();
        let watches = FolderWatches::default();
        let (tx, rx) = mpsc::channel();
        let token = watches.watch_with("main", root.clone(), attach_none, move |f| tx.send(f).is_ok()).unwrap();
        assert!(next(&rx).is_empty());
        std::fs::create_dir(root.join("web")).unwrap();
        watches.rescan(token).unwrap();
        assert_eq!(rx.recv_timeout(POLL / 2), Ok(Some(vec!["web".to_owned()])));
    }

    #[test]
    fn a_poll_reads_a_change_that_the_watcher_missed() {
        let root = temp_dir();
        let (_watch, rx) = start(&root, attach_none);
        assert!(next(&rx).is_empty());
        std::fs::create_dir(root.join("web")).unwrap();
        assert_eq!(next(&rx), ["web"]);
    }

    #[test]
    fn a_missing_root_is_unknown_until_it_comes_back() {
        let root = temp_dir();
        std::fs::create_dir(root.join("web")).unwrap();
        let (_watch, rx) = watch(&root);
        assert_eq!(next(&rx), ["web"]);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(frame(&rx), None);
        std::fs::create_dir_all(root.join("app")).unwrap();
        assert_eq!(next(&rx), ["app"]);
        std::fs::create_dir(root.join("web")).unwrap();
        assert_eq!(next(&rx), ["app", "web"]);
    }

    #[test]
    fn an_empty_root_that_goes_and_comes_back_sends_none_then_empty() {
        let root = temp_dir();
        let (_watch, rx) = watch(&root);
        assert!(next(&rx).is_empty());
        std::fs::remove_dir(&root).unwrap();
        assert_eq!(frame(&rx), None);
        std::fs::create_dir(&root).unwrap();
        assert!(next(&rx).is_empty());
        std::fs::create_dir(root.join("web")).unwrap();
        assert_eq!(next(&rx), ["web"]);
    }

    /// Creates `late` while it attaches to a root that has `web`, so a
    /// folder is created between the read and the attach.
    fn attach_late(root: &Path, _: Weak<Shared>) -> Result<Box<dyn Send>> {
        if root.join("web").exists() {
            let _ = std::fs::create_dir(root.join("late"));
        }
        Ok(Box::new(()))
    }

    #[test]
    fn a_folder_created_during_the_recovery_attach_is_in_the_first_list() {
        let root = temp_dir();
        let (_watch, rx) = start(&root, attach_late);
        assert!(next(&rx).is_empty());
        std::fs::remove_dir(&root).unwrap();
        assert_eq!(frame(&rx), None);
        // The root comes back with `web` in one step, so no read sees it empty.
        let staged = root.with_extension("staged");
        std::fs::create_dir_all(staged.join("web")).unwrap();
        std::fs::rename(&staged, &root).unwrap();
        assert_eq!(next(&rx), ["late", "web"]);
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let root = temp_dir().join("gone");
        assert!(prepare(root, attach_notify).is_err());
    }

    #[test]
    fn sends_nothing_after_the_watch_drops() {
        let root = temp_dir();
        let (watch, rx) = watch(&root);
        assert!(next(&rx).is_empty());
        drop(watch);
        std::fs::create_dir(root.join("late")).unwrap();
        // The worker drops its sender when it exits, so the channel closes.
        assert_eq!(rx.recv_timeout(WAIT), Err(RecvTimeoutError::Disconnected));
    }

    #[test]
    fn a_failed_first_send_retires_the_watch() {
        let root = temp_dir();
        let watches = FolderWatches::default();
        watches.watch_with("main", root, attach_none, |_| false).unwrap();
        until(|| len(&watches) == 0);
    }

    #[test]
    fn a_failed_later_send_retires_the_watch() {
        let root = temp_dir();
        let watches = FolderWatches::default();
        let mut sends = 0;
        let token = watches
            .watch_with("main", root.clone(), attach_none, move |_| {
                sends += 1;
                sends == 1
            })
            .unwrap();
        assert_eq!(len(&watches), 1);
        std::fs::create_dir(root.join("web")).unwrap();
        watches.rescan(token).unwrap();
        until(|| len(&watches) == 0);
        assert!(watches.rescan(token).is_err());
    }

    #[test]
    fn an_owner_drop_ends_only_its_watches() {
        let root = temp_dir();
        let watches = FolderWatches::default();
        let (main_tx, main_rx) = mpsc::channel();
        let (other_tx, other_rx) = mpsc::channel();
        watches.watch("main", root.clone(), move |f| main_tx.send(f).is_ok()).unwrap();
        watches.watch("other", root.clone(), move |f| other_tx.send(f).is_ok()).unwrap();
        next(&main_rx);
        next(&other_rx);
        watches.drop_owner("main");
        assert_eq!(main_rx.recv_timeout(WAIT), Err(RecvTimeoutError::Disconnected));
        std::fs::create_dir(root.join("web")).unwrap();
        assert_eq!(next(&other_rx), ["web"]);
        assert_eq!(len(&watches), 1);
    }

    #[test]
    fn a_stale_token_does_not_end_a_new_watch() {
        let root = temp_dir();
        let watches = FolderWatches::default();
        let old = watches.watch_with("main", root.clone(), attach_none, |_| true).unwrap();
        watches.drop_owner("main");
        let new = watches.watch_with("main", root, attach_none, |_| true).unwrap();
        watches.remove(old);
        assert!(watches.rescan(new).is_ok());
    }
}
