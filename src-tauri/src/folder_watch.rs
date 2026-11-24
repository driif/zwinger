//! Live top-level folders of a project folder, for the project map.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::cage::{self, FolderList};
use crate::error::{Error, Result};

/// The quiet time that ends a burst of changes.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// The longest time that a list waits for a burst to end.
const MAX_DELAY: Duration = Duration::from_millis(500);
/// How often the worker checks the root without a signal. This finds a
/// removed root, a root that came back, and changes that the OS watcher did
/// not report. The check reads only the root's own metadata ([`Stamp`]);
/// the full list follows only when it changed.
const POLL: Duration = Duration::from_secs(1);

/// What a poll compares to find a change of the root without listing it:
/// the identity of the root, its mtime (an entry added, removed or renamed),
/// its ctime and mode (a permission change). `None`: no metadata.
type Stamp = Option<(u64, u64, i64, i64, i64, i64, u32)>;

fn stamp(root: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(root).ok()?;
    Some((m.dev(), m.ino(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec(), m.mode()))
}

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
    /// The count of the full lists of the root, after the first one.
    lists: AtomicU64,
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

/// A watch with its first list, before its worker starts.
struct Ready {
    shared: Arc<Shared>,
    root: PathBuf,
    attach: Attach,
    /// `None` when the attach failed, such as for a missing root.
    watcher: Option<Box<dyn Send>>,
    first: FolderList,
    /// The stamp of the root, read before `first`.
    stamp: Stamp,
}

/// Attaches the watcher to `root`, then reads the first list, so a change
/// between the two is not lost. A root that cannot be read is watched too:
/// the worker reads it on each poll until it can be read.
fn prepare(root: PathBuf, attach: Attach) -> Ready {
    let shared = Arc::new(Shared::default());
    let watcher = attach(&root, Arc::downgrade(&shared)).ok();
    let stamp = stamp(&root);
    let first = cage::list_folders(&root);
    Ready { shared, root, attach, watcher, first, stamp }
}

impl Ready {
    /// Starts the worker. It sends the first list, and then each list that
    /// differs from the last one, until the watch drops or `send` returns
    /// false. Then it drops the OS watcher and calls `on_exit`.
    fn spawn(
        self,
        mut send: impl FnMut(FolderList) -> bool + Send + 'static,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> FolderWatch {
        let Ready { shared, root, attach, mut watcher, first, stamp: mut last_stamp } = self;
        let worker = shared.clone();
        std::thread::spawn(move || {
            let list = |root: &Path| {
                worker.lists.fetch_add(1, Ordering::Relaxed);
                cage::list_folders(root)
            };
            let mut last = first;
            // True when the last attach failed. Each later wake tries again.
            let mut detached = watcher.is_none();
            if send(last.clone()) {
                loop {
                    let wake = worker.wait();
                    if wake == Wake::Stop || (wake == Wake::Changed && !worker.settle()) {
                        break;
                    }
                    // The stamp is read before the list, so a change during the list shows at the next poll.
                    let now = stamp(&root);
                    let was_readable = matches!(last, FolderList::Listed(_));
                    // An idle poll of a readable root with the same stamp has nothing new to read. An unreadable
                    // root is read on each poll: a read can fail for a reason that the stamp does not show.
                    if wake == Wake::Poll && was_readable && !detached && now.is_some() && now == last_stamp {
                        continue;
                    }
                    let replaced = now.zip(last_stamp).is_some_and(|(n, l)| (n.0, n.1) != (l.0, l.1));
                    last_stamp = now;
                    let mut folders = list(&root);
                    // A root that came back or was replaced, or a read that finds a change that the watcher did
                    // not report, gets a new watcher. The read after the attach finds a change between the two.
                    let readable = matches!(folders, FolderList::Listed(_));
                    if readable && (!was_readable || detached || replaced || (wake != Wake::Changed && folders != last))
                    {
                        match attach(&root, Arc::downgrade(&worker)) {
                            Ok(new) => {
                                watcher = Some(new);
                                detached = false;
                                last_stamp = stamp(&root);
                                folders = list(&root);
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
    pub fn watch(&self, owner: &str, root: PathBuf, send: impl FnMut(FolderList) -> bool + Send + 'static) -> u64 {
        self.watch_with(owner, root, attach_notify, send)
    }

    fn watch_with(
        &self,
        owner: &str,
        root: PathBuf,
        attach: Attach,
        send: impl FnMut(FolderList) -> bool + Send + 'static,
    ) -> u64 {
        let ready = prepare(root, attach);
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
        token
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
    use crate::cage::Unreadable;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

    /// Long enough for FSEvents to report a change.
    const WAIT: Duration = Duration::from_secs(5);
    /// Long enough that a change that was to come has come.
    const QUIET: Duration = Duration::from_millis(1500);

    fn temp_dir() -> PathBuf {
        let dir = crate::test_dir::root().join(format!("zwinger-folders-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// An OS watcher that reports nothing. The test signals the changes.
    fn attach_none(_: &Path, _: Weak<Shared>) -> Result<Box<dyn Send>> {
        Ok(Box::new(()))
    }

    fn start(root: &Path, attach: Attach) -> (FolderWatch, Receiver<FolderList>) {
        let (tx, rx) = mpsc::channel();
        let watch = prepare(root.to_owned(), attach).spawn(move |folders| tx.send(folders).is_ok(), || {});
        (watch, rx)
    }

    fn watch(root: &Path) -> (FolderWatch, Receiver<FolderList>) {
        start(root, attach_notify)
    }

    fn frame(rx: &Receiver<FolderList>) -> FolderList {
        rx.recv_timeout(WAIT).expect("a frame in time")
    }

    fn next(rx: &Receiver<FolderList>) -> Vec<String> {
        match frame(rx) {
            FolderList::Listed(names) => names,
            other => panic!("a list of a readable root, not {other:?}"),
        }
    }

    const MISSING: FolderList = FolderList::Unreadable(Unreadable::Missing);

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

    /// The cost of one idle poll of a root with 10k entries.
    #[test]
    #[ignore = "a measurement: cargo test --release -- --ignored --nocapture idle_poll_cost"]
    fn idle_poll_cost() {
        let root = temp_dir();
        for i in 0..10_000 {
            if i % 2 == 0 {
                std::fs::create_dir(root.join(format!("d{i}"))).unwrap();
            } else {
                std::fs::write(root.join(format!("f{i}")), "").unwrap();
            }
        }
        let runs = 200;
        let begin = Instant::now();
        for _ in 0..runs {
            std::hint::black_box(cage::list_folders(&root));
        }
        let list = begin.elapsed() / runs;
        let begin = Instant::now();
        for _ in 0..runs {
            std::hint::black_box(stamp(&root));
        }
        let check = begin.elapsed() / runs;
        println!("full listing: {list:?} per poll; stamp check: {check:?} per poll");
        std::fs::remove_dir_all(root).unwrap();
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
        assert_eq!(list, FolderList::Listed(vec!["web".to_owned()]));
        assert!(begin.elapsed() < MAX_DELAY + DEBOUNCE * 2);
    }

    #[test]
    fn a_rescan_reads_a_change_that_the_watcher_missed() {
        let root = temp_dir();
        let watches = FolderWatches::default();
        let (tx, rx) = mpsc::channel();
        let token = watches.watch_with("main", root.clone(), attach_none, move |f| tx.send(f).is_ok());
        assert!(next(&rx).is_empty());
        std::fs::create_dir(root.join("web")).unwrap();
        watches.rescan(token).unwrap();
        assert_eq!(rx.recv_timeout(POLL / 2), Ok(FolderList::Listed(vec!["web".to_owned()])));
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
    fn an_idle_poll_reads_only_the_stamp_of_the_root() {
        let root = temp_dir();
        std::fs::create_dir(root.join("web")).unwrap();
        let (watch, rx) = start(&root, attach_none);
        assert_eq!(next(&rx), ["web"]);
        std::thread::sleep(POLL * 3);
        assert_eq!(watch.shared.lists.load(Ordering::Relaxed), 0);
        // A rescan, as on a window focus, still reads the full list.
        watch.shared.set(|f| f.rescan = true);
        until(|| watch.shared.lists.load(Ordering::Relaxed) == 1);
    }

    #[test]
    fn a_poll_finds_a_root_that_became_unreadable_and_readable_again() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir();
        std::fs::create_dir(root.join("web")).unwrap();
        let (_watch, rx) = start(&root, attach_none);
        assert_eq!(next(&rx), ["web"]);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = frame(&rx);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(unreadable, FolderList::Unreadable(Unreadable::PermissionDenied));
        assert_eq!(next(&rx), ["web"]);
    }

    #[test]
    fn a_missing_root_is_missing_until_it_comes_back() {
        let root = temp_dir();
        std::fs::create_dir(root.join("web")).unwrap();
        let (_watch, rx) = watch(&root);
        assert_eq!(next(&rx), ["web"]);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(frame(&rx), MISSING);
        std::fs::create_dir_all(root.join("app")).unwrap();
        assert_eq!(next(&rx), ["app"]);
        std::fs::create_dir(root.join("web")).unwrap();
        assert_eq!(next(&rx), ["app", "web"]);
    }

    #[test]
    fn an_empty_root_that_goes_and_comes_back_sends_missing_then_empty() {
        let root = temp_dir();
        let (_watch, rx) = watch(&root);
        assert!(next(&rx).is_empty());
        std::fs::remove_dir(&root).unwrap();
        assert_eq!(frame(&rx), MISSING);
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
        assert_eq!(frame(&rx), MISSING);
        // The root comes back with `web` in one step, so no read sees it empty.
        let staged = root.with_extension("staged");
        std::fs::create_dir_all(staged.join("web")).unwrap();
        std::fs::rename(&staged, &root).unwrap();
        assert_eq!(next(&rx), ["late", "web"]);
    }

    #[test]
    fn a_root_missing_at_the_start_is_followed_until_it_comes_back() {
        let root = temp_dir().join("late");
        let (_watch, rx) = watch(&root);
        assert_eq!(frame(&rx), MISSING);
        std::fs::create_dir(&root).unwrap();
        assert!(next(&rx).is_empty());
        std::fs::create_dir(root.join("web")).unwrap();
        assert_eq!(next(&rx), ["web"]);
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
        watches.watch_with("main", root, attach_none, |_| false);
        until(|| len(&watches) == 0);
    }

    #[test]
    fn a_failed_later_send_retires_the_watch() {
        let root = temp_dir();
        let watches = FolderWatches::default();
        let mut sends = 0;
        let token = watches.watch_with("main", root.clone(), attach_none, move |_| {
            sends += 1;
            sends == 1
        });
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
        watches.watch("main", root.clone(), move |f| main_tx.send(f).is_ok());
        watches.watch("other", root.clone(), move |f| other_tx.send(f).is_ok());
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
        let old = watches.watch_with("main", root.clone(), attach_none, |_| true);
        watches.drop_owner("main");
        let new = watches.watch_with("main", root, attach_none, |_| true);
        watches.remove(old);
        assert!(watches.rescan(new).is_ok());
    }
}
