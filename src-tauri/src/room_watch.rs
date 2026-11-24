//! The live room of a round folder. One worker per room watches the round,
//! its `turns` folder and the parent of the round with notify, and reads
//! again only the round files that an event names.
//!
//! Recovery: a notify error, an overflow, or an event on a folder itself
//! reads the whole round again. The worker attaches again before each full
//! read, so a folder that the read finds is also watched. Each `RECONCILE` a light check stats the two
//! folders and the kept files, and reads the whole round only if a folder
//! changed. It finds an event that the OS lost with no signal. Each full read
//! and each light check also ticks the live output of an external round, so a
//! lost `*.raw.err` event shows at the next check. While notify cannot watch
//! all three folders, the same check runs each `DETACHED` and tries to attach again.
//!
//! An agent can keep its `*.raw.err` file open, and the OS then can report no
//! event for its writes. So while the active stage of an external round has
//! live sources, the worker ticks the live output each `LIVE_TICK`. That tick
//! stats only the two folders and reads only the new bytes of the live files:
//! no scan and no new watcher. A tick that an event caused moves the timer back.
//!
//! Each read that is not a full read first checks the identity of the two
//! folders. A different folder reads nothing: the worker attaches again and
//! reads the whole round, with a new live output and a new generation. Each
//! read checks the identity again at its end, and a read across a replacement
//! sends nothing. When a
//! complete watch reports a new, removed or renamed entry, the keyed read keeps
//! the folder times from before it, so the next light check does not scan
//! again. An event that the OS lost in the same burst can then wait for the
//! next folder change.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use notify::event::{EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::AbortHandle;
use tokio::time::{Instant, sleep_until, timeout};

use crate::external_live::{Tail, Tick};
use crate::task_artifacts::{Room, RoundReader, TURNS_DIR, known};
use crate::task_runs::{LiveRecord, RunHandle, Status};

/// The quiet time that ends a burst of changes.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// The longest time that a room waits for a burst to end.
const MAX_DELAY: Duration = Duration::from_millis(500);
/// The changed names that a worker keeps. More read the whole round.
const DIRTY_LIMIT: usize = 128;
/// The light check while notify watches.
const RECONCILE: Duration = Duration::from_secs(30);
/// The light check, and a new attach, while notify cannot watch.
const DETACHED: Duration = Duration::from_secs(2);
/// The live tick of an external round while its active stage has live output.
const LIVE_TICK: Duration = Duration::from_millis(500);

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RoomEvent {
    /// The whole room, each time a file of the round changes. `run` is the
    /// state of an in-app run of this app session.
    Room { generation: u64, room: Box<Room>, run: Option<Status> },
    /// Clears only one participant's temporary live output.
    Reset { generation: u64, sequence: u64, agent: String },
    /// A live record of the active stage.
    Live { generation: u64, sequence: u64, agent: String, stage: String, record: serde_json::Value },
}

/// What the events since the last read ask for.
#[derive(Default)]
struct Flags {
    /// Round files by relative name, at most `DIRTY_LIMIT`.
    dirty: BTreeSet<String>,
    rescan: bool,
    reattach: bool,
    /// An entry of the round or of `turns` was created, removed or renamed.
    structural: bool,
    /// A live output file (`*.raw.err`) changed.
    live: bool,
}

#[derive(Default)]
struct Shared {
    flags: Mutex<Flags>,
    wake: Notify,
    /// The full scans and the file reads of the reader.
    #[cfg(test)]
    counts: Mutex<(u64, u64)>,
    /// Goes into the next read as its `during` hook.
    #[cfg(test)]
    during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, signal: impl FnOnce(&mut Flags)) {
        signal(&mut self.lock());
        self.wake.notify_one();
    }

    fn signal(&self, root: &Path, event: notify::Result<notify::Event>) {
        if classify(root, event, &mut self.lock()) {
            self.wake.notify_one();
        }
    }

    fn take(&self) -> Flags {
        std::mem::take(&mut *self.lock())
    }
}

/// Adds what `event` means for the round at `root` to `flags`. False if it
/// means nothing for the room: an access, or a name that is not a round file.
fn classify(root: &Path, event: notify::Result<notify::Event>, flags: &mut Flags) -> bool {
    let event = match event {
        Ok(event) if !event.need_rescan() && !event.paths.is_empty() => event,
        _ => {
            flags.rescan = true;
            flags.reattach = true;
            return true;
        }
    };
    if event.kind.is_access() {
        return false;
    }
    let structural =
        matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)));
    let turns = root.join(TURNS_DIR);
    let mut wake = false;
    for path in &event.paths {
        // An event on a folder itself names no file: read the whole round.
        if *path == *root || *path == turns {
            flags.rescan = true;
            flags.reattach |= structural;
            wake = true;
            continue;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
            continue;
        };
        if parent != root && parent != turns {
            continue;
        }
        flags.structural |= structural;
        let key = if parent == root {
            name.to_owned()
        } else if parent == turns {
            format!("{TURNS_DIR}/{name}")
        } else {
            continue;
        };
        if name.ends_with(".raw.err") {
            flags.live = true;
            wake = true;
        }
        if known(&key).is_some() {
            wake = true;
            if !flags.rescan {
                flags.dirty.insert(key);
                if flags.dirty.len() > DIRTY_LIMIT {
                    flags.dirty.clear();
                    flags.rescan = true;
                    flags.reattach = true;
                }
            }
        }
    }
    wake
}

/// The OS watchers of a round. When they drop, they end. `complete` is false
/// when a folder that exists is not watched: the worker then checks and
/// attaches again each `DETACHED`.
struct Attached {
    _watcher: Box<dyn Send>,
    complete: bool,
}

/// Attaches the OS watchers of the round at `root`, which signal to `shared`.
/// The worker owns the result. `None` if notify cannot watch.
type Attach = fn(&Path, Weak<Shared>) -> Option<Attached>;

/// Watches `root` and its `turns` folder, and the parent of `root`, where
/// only the entry of `root` counts. A missing root needs only the parent.
fn attach_notify(root: &Path, shared: Weak<Shared>) -> Option<Attached> {
    attach_filtered(root, shared, |_, _| true)
}

/// `attach_notify` that signals only the events that `keep` accepts.
fn attach_filtered(
    root: &Path,
    shared: Weak<Shared>,
    keep: fn(&Path, &notify::Result<notify::Event>) -> bool,
) -> Option<Attached> {
    let owned = root.to_owned();
    let mut watcher = notify::recommended_watcher(move |event| {
        if let Some(shared) = shared.upgrade()
            && keep(&owned, &event)
        {
            shared.signal(&owned, event);
        }
    })
    .ok()?;
    let parent = root.parent().is_some_and(|p| watcher.watch(p, RecursiveMode::NonRecursive).is_ok());
    // A link in place of a folder is not watched, as the reader does not follow it.
    if dir_stat(root).is_some() {
        watcher.watch(root, RecursiveMode::NonRecursive).ok()?;
        let turns = root.join(TURNS_DIR);
        if dir_stat(&turns).is_some() {
            watcher.watch(&turns, RecursiveMode::NonRecursive).ok()?;
        }
    } else if !parent {
        return None;
    }
    // Without the parent, a new root in place of the old one is not seen.
    let complete = parent || root.parent().is_none();
    Some(Attached { _watcher: Box::new(watcher), complete })
}

/// The identity and the modification time of a folder, without following a link.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DirStat {
    dev: u64,
    ino: u64,
    modified: (i64, i64),
}

fn dir_stat(path: &Path) -> Option<DirStat> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.is_dir().then(|| DirStat { dev: meta.dev(), ino: meta.ino(), modified: (meta.mtime(), meta.mtime_nsec()) })
}

/// The round folder and its `turns` folder, as the last full scan saw them.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Dirs {
    root: Option<DirStat>,
    turns: Option<DirStat>,
}

impl Dirs {
    fn observe(root: &Path) -> Self {
        Self { root: dir_stat(root), turns: dir_stat(&root.join(TURNS_DIR)) }
    }

    fn same_folders(&self, other: &Self) -> bool {
        let id = |d: Option<DirStat>| d.map(|d| (d.dev, d.ino));
        id(self.root) == id(other.root) && id(self.turns) == id(other.turns)
    }
}

enum Job {
    /// Nothing to read.
    None,
    /// Read the whole round.
    Full,
    /// Read only these files. `trust`: the events since the last read name
    /// each change of a folder entry, so the folder times after it are known.
    Keys { keys: BTreeSet<String>, trust: bool },
    /// The light check.
    Reconcile,
}

/// What the worker owns between its reads. A read moves it to a blocking thread.
struct State {
    reader: RoundReader,
    dirs: Dirs,
    tail: Option<Tail>,
    room: Room,
    /// Runs in a read after the tick, before the last identity check.
    #[cfg(test)]
    during: Option<Box<dyn FnOnce() + Send>>,
}

/// The outcome of a read: the room changed, a folder is a different one, the live tick.
struct Read {
    changed: bool,
    moved: bool,
    tick: Option<Tick>,
}

impl State {
    fn new(root: &Path, external: bool) -> Self {
        Self {
            reader: RoundReader::new(root.to_owned()),
            dirs: Dirs::default(),
            tail: external.then(Tail::new),
            room: Room::default(),
            #[cfg(test)]
            during: None,
        }
    }

    fn read(&mut self, root: &Path, job: Job, live: bool, cancel: &AtomicBool) -> Read {
        // Before a read, so a change during the read shows at the next check.
        let now = Dirs::observe(root);
        let replaced = !now.same_folders(&self.dirs);
        if replaced && !matches!(job, Job::Full) {
            // The worker attaches again, then reads the whole round.
            return Read { changed: false, moved: true, tick: None };
        }
        let check = matches!(job, Job::Full | Job::Reconcile);
        let changed = match job {
            Job::None => false,
            Job::Full => {
                if replaced && self.tail.is_some() {
                    self.tail = Some(Tail::new());
                }
                self.dirs = now;
                // A different folder always sends its room, also after a discarded read.
                self.reader.scan(cancel) || replaced
            }
            Job::Keys { keys, trust } => {
                let changed = self.reader.refresh_keys(keys.iter().map(String::as_str));
                if trust {
                    self.dirs = now;
                }
                changed
            }
            Job::Reconcile => {
                if now == self.dirs {
                    let keys = self.reader.keys();
                    self.reader.refresh_keys(keys.iter().map(String::as_str))
                } else {
                    self.dirs = now;
                    self.reader.scan(cancel)
                }
            }
        };
        // The room goes into `self.room` only after the last identity check,
        // so a room of two folders is never kept.
        let room = changed.then(|| self.reader.room());
        // A full read and a light check also tick, so a lost live event shows.
        let check = check || live || changed;
        let mut tick = match &mut self.tail {
            Some(tail) if check => Some(tail.tick(root, room.as_ref().unwrap_or(&self.room))),
            _ => None,
        };
        #[cfg(test)]
        if let Some(during) = self.during.take() {
            during();
        }
        // A folder replaced during the read: the files and the tick can come
        // from the new folder. Send nothing. `dirs` keeps the identity from
        // before, so the next full read resets the live output and sends the room.
        if !Dirs::observe(root).same_folders(&now) {
            return Read { changed: false, moved: true, tick: None };
        }
        if let Some(room) = room {
            self.room = room;
        }
        // The live output of another folder starts again in a new generation.
        if let Some(tick) = &mut tick {
            tick.reset |= replaced;
        }
        Read { changed, moved: false, tick }
    }
}

/// The counters of a room's events. A native run's live records share them.
#[derive(Clone)]
struct Feed {
    generation: Arc<AtomicU64>,
    sequence: Arc<AtomicU64>,
    /// Set when the watch ends. A read that ends later sends nothing.
    cancel: Arc<AtomicBool>,
    /// Held over each send and over the end of the watch, so no event goes out
    /// after the end returns.
    gate: Arc<Mutex<()>>,
}

impl Feed {
    /// Sends `event` unless the watch ended. False if it ended or the send failed.
    fn deliver(&self, send: impl FnOnce(RoomEvent) -> bool, event: RoomEvent) -> bool {
        let _gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        !self.cancel.load(Ordering::SeqCst) && send(event)
    }
}

struct Worker<S> {
    root: PathBuf,
    attach: Attach,
    shared: Arc<Shared>,
    watcher: Option<Attached>,
    status: Option<watch::Receiver<Status>>,
    /// False after the run drops its sender. The last status stays in the room.
    status_open: bool,
    feed: Feed,
    send: S,
    /// The time of the last light check.
    checked: Instant,
    /// The time of the last live tick, also of a tick that an event caused.
    ticked: Instant,
}

/// Sends the room of `root` now and on each change, until `send` returns
/// false or `feed.cancel` is set. `status` adds the state of an in-app run;
/// `external` adds the live output of an external round.
async fn watch_room(
    attach: Attach,
    shared: Arc<Shared>,
    root: PathBuf,
    mut status: Option<watch::Receiver<Status>>,
    external: bool,
    feed: Feed,
    send: impl FnMut(RoomEvent) -> bool + Send,
) {
    if let Some(status) = &mut status {
        status.borrow_and_update();
    }
    let now = Instant::now();
    let mut worker = Worker {
        root,
        attach,
        shared,
        watcher: None,
        status,
        status_open: true,
        feed,
        send,
        checked: now,
        ticked: now,
    };
    // Attach before the first read, so a write between the two is not lost.
    worker.attach();
    let state = State::new(&worker.root, external);
    let Some(mut state) = worker.step(state, Job::Full, true, true).await else {
        return;
    };
    loop {
        let period = if worker.complete() { RECONCILE } else { DETACHED };
        let shared = worker.shared.clone();
        let status = worker.status.as_mut().filter(|_| worker.status_open);
        let live_tail = state.tail.as_ref().is_some_and(Tail::is_live);
        let (job, live) = tokio::select! {
            () = shared.wake.notified() => {
                settle(&shared).await;
                let flags = shared.take();
                // Attach before each full read, so a folder that it finds is watched.
                let job = if flags.rescan || flags.reattach {
                    worker.attach();
                    Job::Full
                } else if flags.dirty.is_empty() {
                    Job::None
                } else {
                    Job::Keys { keys: flags.dirty, trust: worker.complete() && flags.structural }
                };
                (job, flags.live)
            }
            open = changed(status) => {
                if !open {
                    worker.status_open = false;
                    continue;
                }
                let run = worker.status.as_mut().map(|s| s.borrow_and_update().clone());
                let room = Box::new(state.room.clone());
                let event = RoomEvent::Room { generation: worker.feed.generation.load(Ordering::SeqCst), room, run };
                if !worker.feed.deliver(&mut worker.send, event) {
                    return;
                }
                continue;
            }
            () = sleep_until(worker.ticked + LIVE_TICK), if live_tail => {
                worker.ticked = Instant::now();
                (Job::None, true)
            }
            () = sleep_until(worker.checked + period) => {
                worker.checked = Instant::now();
                if !worker.complete() {
                    worker.attach();
                }
                let job = if worker.complete() && period == DETACHED { Job::Full } else { Job::Reconcile };
                (job, false)
            }
        };
        let Some(next) = worker.step(state, job, live, false).await else {
            return;
        };
        state = next;
    }
}

/// True when the status changed, false when the run is gone. Never ends without a status.
async fn changed(status: Option<&mut watch::Receiver<Status>>) -> bool {
    match status {
        Some(status) => status.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Waits until the events are quiet for `DEBOUNCE`, but not longer than
/// `MAX_DELAY`, so continuous changes cannot hold back the room.
async fn settle(shared: &Shared) {
    let deadline = Instant::now() + MAX_DELAY;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || timeout(DEBOUNCE.min(left), shared.wake.notified()).await.is_err() {
            return;
        }
    }
}

impl<S: FnMut(RoomEvent) -> bool + Send> Worker<S> {
    fn attach(&mut self) {
        // The old watchers end first, so they cannot signal a stale folder.
        self.watcher = None;
        self.watcher = (self.attach)(&self.root, Arc::downgrade(&self.shared));
    }

    fn complete(&self) -> bool {
        self.watcher.as_ref().is_some_and(|w| w.complete)
    }

    fn is_cancelled(&self) -> bool {
        self.feed.cancel.load(Ordering::SeqCst)
    }

    /// Reads the round for `job` on a blocking thread and sends what changed.
    /// The first step always sends the room. `None` when the watch ends.
    async fn step(&mut self, mut state: State, job: Job, live: bool, first: bool) -> Option<State> {
        if matches!(job, Job::None) && !(live && state.tail.is_some()) {
            return Some(state);
        }
        #[cfg(test)]
        if let Some(during) = self.shared.during.lock().unwrap_or_else(PoisonError::into_inner).take() {
            state.during = Some(during);
        }
        let root = self.root.clone();
        let cancel = self.feed.cancel.clone();
        let (state, read) = tokio::task::spawn_blocking(move || {
            let read = state.read(&root, job, live, &cancel);
            (state, read)
        })
        .await
        .ok()?;
        #[cfg(test)]
        {
            *self.shared.counts.lock().unwrap_or_else(PoisonError::into_inner) = state.reader.counts();
        }
        if self.is_cancelled() {
            return None;
        }
        if read.tick.is_some() {
            self.ticked = Instant::now();
        }
        if read.moved {
            self.shared.set(|f| {
                f.reattach = true;
                f.rescan = true;
            });
        }
        // A first read across a replacement sends nothing. The full read that follows sends the room.
        let mut send_room = (first && !read.moved) || read.changed;
        if let Some(tick) = &read.tick
            && tick.reset
        {
            self.feed.generation.fetch_add(1, Ordering::SeqCst);
            self.feed.sequence.store(0, Ordering::SeqCst);
            send_room = true;
        }
        let generation = self.feed.generation.load(Ordering::SeqCst);
        if send_room {
            let run = self.status.as_ref().map(|s| s.borrow().clone());
            let event = RoomEvent::Room { generation, room: Box::new(state.room.clone()), run };
            if !self.feed.deliver(&mut self.send, event) {
                return None;
            }
        }
        let Some(tick) = read.tick else {
            return Some(state);
        };
        for agent in tick.reset_agents {
            let sequence = self.feed.sequence.fetch_add(1, Ordering::SeqCst);
            if !self.feed.deliver(&mut self.send, RoomEvent::Reset { generation, sequence, agent }) {
                return None;
            }
        }
        if let Some(stage) = state.room.stages.iter().find(|s| s.state == "active") {
            for (agent, record) in tick.records {
                let sequence = self.feed.sequence.fetch_add(1, Ordering::SeqCst);
                let stage = stage.id.clone();
                let event = RoomEvent::Live { generation, sequence, agent, stage, record };
                if !self.feed.deliver(&mut self.send, event) {
                    return None;
                }
            }
        }
        // A backlog continues on the next debounce turn, not at once.
        if tick.more {
            self.shared.set(|f| f.live = true);
        }
        Some(state)
    }
}

/// Sends the live records of an in-app run: the recent ones, then each new one.
async fn live_records(
    (recent, mut rx): (Vec<LiveRecord>, broadcast::Receiver<LiveRecord>),
    feed: Feed,
    send: impl Fn(RoomEvent) -> bool,
    retire: impl FnOnce(),
) {
    let live = |record: LiveRecord| {
        let event = RoomEvent::Live {
            generation: feed.generation.load(Ordering::SeqCst),
            sequence: feed.sequence.fetch_add(1, Ordering::SeqCst),
            agent: record.agent,
            stage: record.stage,
            record: record.record,
        };
        feed.deliver(&send, event)
    };
    for record in recent {
        if !live(record) {
            return retire();
        }
    }
    loop {
        match rx.recv().await {
            Ok(record) => {
                if !live(record) {
                    return retire();
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The tasks of one room. A drop ends them, and with them the OS watchers.
struct Guard {
    feed: Feed,
    tasks: Vec<AbortHandle>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // An abort does not stop a task during a send. The gate waits for that
        // send, and each later send sees the cancel flag.
        {
            let _gate = self.feed.gate.lock().unwrap_or_else(PoisonError::into_inner);
            self.feed.cancel.store(true, Ordering::SeqCst);
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

type Entries = Mutex<HashMap<u64, (String, Guard)>>;

fn lock(entries: &Entries) -> MutexGuard<'_, HashMap<u64, (String, Guard)>> {
    entries.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Ends the watch of `token`. A second call, or a call for an ended watch, does nothing.
fn retire(entries: &Entries, token: u64) {
    let gone = lock(entries).remove(&token);
    drop(gone);
}

/// The rooms that the webviews watch, by token, with the label of their webview.
#[derive(Default)]
pub struct Watches {
    next: AtomicU64,
    entries: Arc<Entries>,
}

impl Watches {
    /// Sends the room of `root` now and on each change, for the webview
    /// `owner`. `run` adds its status and live records; `external` adds the
    /// live output of an external round. The watch ends at `retire`, at
    /// `drop_owner`, or when a send fails.
    pub fn watch(
        &self,
        owner: &str,
        root: PathBuf,
        run: Option<Arc<RunHandle>>,
        external: bool,
        send: impl Fn(RoomEvent) -> bool + Clone + Send + Sync + 'static,
    ) -> u64 {
        self.watch_with(attach_notify, owner, root, run, external, send)
    }

    fn watch_with(
        &self,
        attach: Attach,
        owner: &str,
        root: PathBuf,
        run: Option<Arc<RunHandle>>,
        external: bool,
        send: impl Fn(RoomEvent) -> bool + Clone + Send + Sync + 'static,
    ) -> u64 {
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let feed = Feed {
            generation: Arc::new(AtomicU64::new((token + 1) << 32)),
            sequence: Arc::new(AtomicU64::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            gate: Arc::default(),
        };
        let entries = Arc::downgrade(&self.entries);
        let end = move || {
            if let Some(entries) = entries.upgrade() {
                retire(&entries, token);
            }
        };
        // The tasks start under the lock, so the entry exists before a task can retire it.
        let mut map = lock(&self.entries);
        let mut tasks = Vec::new();
        if let Some(run) = &run {
            let records = live_records(run.subscribe(), feed.clone(), send.clone(), end.clone());
            tasks.push(tokio::spawn(records).abort_handle());
        }
        let status = run.map(|run| run.status_rx());
        let ended = feed.clone();
        let room = watch_room(attach, Arc::default(), root, status, external, feed, send);
        tasks.push(
            tokio::spawn(async move {
                room.await;
                end();
            })
            .abort_handle(),
        );
        map.insert(token, (owner.to_owned(), Guard { feed: ended, tasks }));
        token
    }

    pub fn retire(&self, token: u64) {
        retire(&self.entries, token);
    }

    /// Ends the watches of a webview that closed or loaded a new page.
    pub fn drop_owner(&self, owner: &str) {
        let gone: Vec<_> = {
            let mut entries = lock(&self.entries);
            let tokens: Vec<u64> = entries.iter().filter(|(_, (o, _))| o == owner).map(|(t, _)| *t).collect();
            tokens.into_iter().filter_map(|t| entries.remove(&t)).collect()
        };
        drop(gone);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runs::RunState;
    use std::io::Write as _;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    /// Long enough for FSEvents to report a change.
    const WAIT: Duration = Duration::from_secs(5);
    /// Long enough that a frame that was to come has come.
    const QUIET: Duration = Duration::from_millis(1500);

    fn temp_round() -> PathBuf {
        let dir = crate::test_dir::root().join(format!("zwinger-room-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        stage(&dir, "plan");
        dir
    }

    fn stage(dir: &Path, stage: &str) {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl")).unwrap();
        writeln!(file, r#"{{"at":"2026-01-01T00:00:00Z","kind":"stage","detail":"{stage}"}}"#).unwrap();
    }

    fn active(room: &Room) -> Option<&str> {
        room.stages.iter().find(|s| s.state == "active").map(|s| s.id.as_str())
    }

    /// An OS watcher that reports nothing. The test signals the events.
    fn attach_none(_: &Path, _: Weak<Shared>) -> Option<Attached> {
        Some(Attached { _watcher: Box::new(()), complete: true })
    }

    fn attach_fail(_: &Path, _: Weak<Shared>) -> Option<Attached> {
        None
    }

    /// Watches the round but not its parent.
    fn attach_partial(_: &Path, _: Weak<Shared>) -> Option<Attached> {
        Some(Attached { _watcher: Box::new(()), complete: false })
    }

    fn feed() -> Feed {
        Feed {
            generation: Arc::new(AtomicU64::new(1)),
            sequence: Arc::new(AtomicU64::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            gate: Arc::default(),
        }
    }

    struct Probe {
        root: PathBuf,
        shared: Arc<Shared>,
        rx: UnboundedReceiver<RoomEvent>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn start_with(root: &Path, attach: Attach, status: Option<watch::Receiver<Status>>, external: bool) -> Probe {
        start_feed(root, attach, status, external, feed(), None)
    }

    /// `start_with` with its own feed and a hook for the first read.
    fn start_feed(
        root: &Path,
        attach: Attach,
        status: Option<watch::Receiver<Status>>,
        external: bool,
        feed: Feed,
        during: Option<Box<dyn FnOnce() + Send>>,
    ) -> Probe {
        let shared = Arc::new(Shared::default());
        *shared.during.lock().unwrap() = during;
        let (tx, rx) = unbounded_channel();
        let task =
            tokio::spawn(watch_room(attach, shared.clone(), root.to_owned(), status, external, feed, move |e| {
                tx.send(e).is_ok()
            }));
        Probe { root: root.to_owned(), shared, rx, task }
    }

    fn start(root: &Path, attach: Attach) -> Probe {
        start_with(root, attach, None, false)
    }

    impl Probe {
        async fn room(&mut self) -> (Room, Option<Status>) {
            loop {
                match timeout(WAIT, self.rx.recv()).await.expect("a room in time").expect("the watch is open") {
                    RoomEvent::Room { room, run, .. } => return (*room, run),
                    _ => continue,
                }
            }
        }

        async fn until(&mut self, done: impl Fn(&Room) -> bool) -> Room {
            loop {
                let (room, _) = self.room().await;
                if done(&room) {
                    return room;
                }
            }
        }

        async fn quiet(&mut self) -> bool {
            timeout(QUIET, self.rx.recv()).await.is_err()
        }

        fn event(&self, kind: EventKind, name: &str) {
            let event = notify::Event::new(kind).add_path(self.root.join(name));
            self.shared.signal(&self.root, Ok(event));
        }

        fn counts(&self) -> (u64, u64) {
            *self.shared.counts.lock().unwrap()
        }
    }

    fn create() -> EventKind {
        EventKind::Create(notify::event::CreateKind::File)
    }

    // --- real notify --------------------------------------------------------

    #[tokio::test]
    async fn an_ordinary_write_sends_the_room() {
        let root = temp_round();
        let mut probe = start(&root, attach_notify);
        assert_eq!(active(&probe.room().await.0), Some("plan"));
        stage(&root, "implement");
        assert_eq!(active(&probe.until(|r| active(r) != Some("plan")).await), Some("implement"));
    }

    #[tokio::test]
    async fn an_atomic_rename_sends_the_room() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        let mut probe = start(&root, attach_notify);
        assert!(probe.room().await.0.agents.is_empty());
        std::fs::write(root.join("turns/staged.tmp"), "p").unwrap();
        std::fs::rename(root.join("turns/staged.tmp"), root.join("turns/plan-opus.prompt.md")).unwrap();
        probe.until(|r| r.agents == ["opus"]).await;
    }

    #[tokio::test]
    async fn a_burst_sends_one_final_room() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        let mut probe = start(&root, attach_notify);
        probe.room().await;
        for agent in ["a", "b", "c", "d"] {
            std::fs::write(root.join(format!("turns/plan-{agent}.prompt.md")), "p").unwrap();
        }
        probe.until(|r| r.agents.len() == 4).await;
        assert!(probe.quiet().await, "no frame after the last change");
    }

    #[tokio::test]
    async fn unknown_names_send_nothing() {
        let root = temp_round();
        let mut probe = start(&root, attach_notify);
        probe.room().await;
        // A positive control: the watch reports a round file.
        stage(&root, "implement");
        probe.until(|r| active(r) == Some("implement")).await;
        for i in 0..20 {
            std::fs::write(root.join(format!("notes-{i}.txt")), "x").unwrap();
        }
        assert!(probe.quiet().await);
    }

    #[tokio::test]
    async fn a_late_turns_folder_is_watched() {
        let root = temp_round();
        let mut probe = start(&root, attach_notify);
        probe.room().await;
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "p").unwrap();
        probe.until(|r| r.agents == ["opus"]).await;
        // The new folder is watched, not only read once.
        std::fs::write(root.join("turns/plan-sol.prompt.md"), "p").unwrap();
        probe.until(|r| r.agents.len() == 2).await;
    }

    static ATTACHES: AtomicU64 = AtomicU64::new(0);

    /// Real notify that drops the events for the `turns` folder itself, and counts its calls.
    fn attach_blind_to_turns(root: &Path, shared: Weak<Shared>) -> Option<Attached> {
        ATTACHES.fetch_add(1, Ordering::SeqCst);
        attach_filtered(
            root,
            shared,
            |root, event| !matches!(event, Ok(e) if e.paths.iter().any(|p| *p == root.join(TURNS_DIR))),
        )
    }

    #[tokio::test]
    async fn a_rescan_attaches_again_before_its_full_read() {
        let root = temp_round();
        let mut probe = start(&root, attach_blind_to_turns);
        probe.room().await;
        let before = ATTACHES.load(Ordering::SeqCst);
        // The event of the new folder is lost. Only a rescan comes.
        std::fs::create_dir(root.join("turns")).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let event = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        probe.shared.signal(&root, Ok(event));
        probe.room().await;
        assert!(ATTACHES.load(Ordering::SeqCst) > before, "the rescan attaches again");
        // The new folder is watched: a write shows long before the light check.
        let begin = Instant::now();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "p").unwrap();
        probe.until(|r| r.agents == ["opus"]).await;
        assert!(begin.elapsed() < WAIT);
    }

    #[tokio::test]
    async fn a_replaced_file_and_folder_are_read_again() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "p").unwrap();
        let mut probe = start(&root, attach_notify);
        assert_eq!(probe.room().await.0.agents, ["opus"]);
        // A file: the same name, the same size, another inode.
        let staged = root.join("events.staged");
        std::fs::write(&staged, std::fs::read(root.join("events.jsonl")).unwrap()).unwrap();
        let mut file = std::fs::OpenOptions::new().append(true).open(&staged).unwrap();
        writeln!(file, r#"{{"at":"2026-01-01T00:00:01Z","kind":"stage","detail":"gates"}}"#).unwrap();
        std::fs::rename(&staged, root.join("events.jsonl")).unwrap();
        probe.until(|r| active(r) == Some("gates")).await;
        // A folder: `turns` goes away and a new one takes its place.
        let next = root.join("turns.next");
        std::fs::create_dir(&next).unwrap();
        std::fs::write(next.join("plan-sol.prompt.md"), "p").unwrap();
        std::fs::rename(root.join("turns"), root.join("turns.old")).unwrap();
        std::fs::rename(&next, root.join("turns")).unwrap();
        probe.until(|r| r.agents == ["sol"]).await;
        std::fs::write(root.join("turns/plan-zed.prompt.md"), "p").unwrap();
        probe.until(|r| r.agents.len() == 2).await;
    }

    #[tokio::test]
    async fn a_missing_root_is_empty_until_it_comes_back() {
        let root = temp_round();
        let mut probe = start(&root, attach_notify);
        assert_eq!(active(&probe.room().await.0), Some("plan"));
        std::fs::remove_dir_all(&root).unwrap();
        probe.until(|r| r.stages.iter().all(|s| s.state != "active")).await;
        let staged = root.with_extension("staged");
        std::fs::create_dir_all(&staged).unwrap();
        stage(&staged, "gates");
        std::fs::rename(&staged, &root).unwrap();
        probe.until(|r| active(r) == Some("gates")).await;
        // The new root is watched.
        stage(&root, "final");
        probe.until(|r| active(r) == Some("final")).await;
    }

    #[tokio::test]
    async fn external_live_output_follows_the_file_and_resets_on_a_stage_change() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "public").unwrap();
        std::fs::write(root.join("turns/plan-sol.prompt.md"), "public").unwrap();
        let mut probe = start_with(&root, attach_notify, None, true);
        let first = match timeout(WAIT, probe.rx.recv()).await.unwrap().unwrap() {
            RoomEvent::Room { generation, room, .. } if room.agents.len() == 2 => generation,
            _ => panic!("the first event is the room"),
        };
        let line = b"{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Public live text\"}]},\"parent_tool_use_id\":null}\n";
        std::fs::write(root.join("turns/plan-opus.raw.err"), line).unwrap();
        let live = timeout(WAIT, async {
            loop {
                if let RoomEvent::Live { generation, sequence, agent, record, .. } = probe.rx.recv().await.unwrap() {
                    break (generation, sequence, agent, record);
                }
            }
        })
        .await
        .unwrap();
        assert_eq!((live.0, live.1, live.2.as_str()), (first, 0, "opus"));
        assert_eq!(live.3["message"]["content"][0]["text"], "Public live text");
        let sol = root.join("turns/plan-sol.raw.err");
        std::fs::write(&sol, "exec\nfirst\n").unwrap();
        timeout(WAIT, async {
            while !matches!(probe.rx.recv().await, Some(RoomEvent::Live { ref agent, .. }) if agent == "sol") {}
        })
        .await
        .unwrap();
        std::fs::write(&sol, "exec\nsecond\n").unwrap();
        timeout(WAIT, async {
            while !matches!(probe.rx.recv().await, Some(RoomEvent::Reset { generation, ref agent, .. }) if generation == first && agent == "sol")
            {}
        })
        .await
        .unwrap();
        stage(&root, "gates");
        let room = timeout(WAIT, async {
            loop {
                if let Some(RoomEvent::Room { generation, room, .. }) = probe.rx.recv().await
                    && generation > first
                {
                    break room;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(active(&room), Some("gates"));
    }

    // --- the seam: exact work -------------------------------------------------

    #[tokio::test]
    async fn a_named_file_is_read_alone() {
        let root = temp_round();
        let mut probe = start(&root, attach_none);
        probe.room().await;
        assert_eq!(probe.counts(), (1, 1));
        stage(&root, "gates");
        probe.event(create(), "events.jsonl");
        assert_eq!(active(&probe.room().await.0), Some("gates"));
        assert_eq!(probe.counts(), (1, 2), "no scan, one read");
    }

    #[tokio::test]
    async fn an_unknown_name_and_an_access_do_no_work() {
        let root = temp_round();
        let mut probe = start(&root, attach_none);
        probe.room().await;
        probe.event(create(), "notes.txt");
        probe.event(EventKind::Access(notify::event::AccessKind::Read), "events.jsonl");
        assert!(probe.quiet().await);
        assert_eq!(probe.counts(), (1, 1));
        // A positive control: the same probe reports a round file.
        stage(&root, "gates");
        probe.event(create(), "events.jsonl");
        assert_eq!(active(&probe.room().await.0), Some("gates"));
    }

    #[tokio::test]
    async fn an_unchanged_file_sends_nothing() {
        let root = temp_round();
        let mut probe = start(&root, attach_none);
        probe.room().await;
        probe.event(EventKind::Modify(ModifyKind::Any), "events.jsonl");
        assert!(probe.quiet().await);
        assert_eq!(probe.counts(), (1, 1), "a stat, no read");
    }

    #[tokio::test]
    async fn an_overflow_reads_the_whole_round() {
        let root = temp_round();
        let mut probe = start(&root, attach_none);
        probe.room().await;
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "p").unwrap();
        for i in 0..=DIRTY_LIMIT {
            probe.event(create(), &format!("turns/t{i}-opus.json"));
        }
        assert_eq!(probe.room().await.0.agents, ["opus"]);
        assert_eq!(probe.counts().0, 2);
    }

    #[tokio::test]
    async fn a_notify_error_and_a_rescan_flag_read_the_whole_round() {
        let root = temp_round();
        let mut probe = start(&root, attach_none);
        probe.room().await;
        stage(&root, "gates");
        probe.shared.signal(&root, Err(notify::Error::generic("lost")));
        assert_eq!(active(&probe.room().await.0), Some("gates"));
        assert_eq!(probe.counts().0, 2);
        stage(&root, "final");
        let event = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        probe.shared.signal(&root, Ok(event));
        assert_eq!(active(&probe.room().await.0), Some("final"));
        assert_eq!(probe.counts().0, 3);
    }

    #[tokio::test]
    async fn a_failed_attach_falls_back_to_the_light_check() {
        let root = temp_round();
        let mut probe = start(&root, attach_fail);
        probe.room().await;
        stage(&root, "gates");
        let begin = Instant::now();
        assert_eq!(active(&probe.room().await.0), Some("gates"));
        assert!(begin.elapsed() < DETACHED * 2);
    }

    #[tokio::test]
    async fn a_partial_attach_falls_back_to_the_light_check() {
        let root = temp_round();
        let mut probe = start(&root, attach_partial);
        probe.room().await;
        stage(&root, "gates");
        let begin = Instant::now();
        assert_eq!(active(&probe.room().await.0), Some("gates"));
        assert!(begin.elapsed() < DETACHED * 2);
    }

    #[test]
    fn a_full_read_and_a_light_check_tick_the_live_output() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "public").unwrap();
        stage(&root, "tiebreak");
        // `tiebreak.raw.err` is not a round file. It exists before the scan,
        // so a write to it changes no folder and no round file.
        let live = root.join("tiebreak.raw.err");
        std::fs::write(&live, "").unwrap();
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Live\"}]},\"parent_tool_use_id\":null}\n";
        let cancel = AtomicBool::new(false);
        let mut state = State::new(&root, true);
        state.read(&root, Job::Full, false, &cancel);
        assert_eq!(active(&state.room), Some("tiebreak"));
        // The event of this write is lost: no `live` flag.
        std::fs::write(&live, line).unwrap();
        let read = state.read(&root, Job::Reconcile, false, &cancel);
        assert!(!read.changed && !read.moved);
        assert!(!read.tick.expect("a light check ticks").records.is_empty());
        std::fs::write(&live, line.repeat(2)).unwrap();
        let read = state.read(&root, Job::Full, false, &cancel);
        assert!(!read.changed);
        assert!(!read.tick.expect("a full read ticks").records.is_empty());
        // No job and no live flag do not tick.
        assert!(state.read(&root, Job::None, false, &cancel).tick.is_none());
    }

    #[test]
    fn the_light_check_stats_and_scans_only_on_a_folder_change() {
        let root = temp_round();
        let cancel = AtomicBool::new(false);
        let mut state = State::new(&root, false);
        state.read(&root, Job::Full, false, &cancel);
        assert_eq!(state.reader.counts(), (1, 1));
        let read = state.read(&root, Job::Reconcile, false, &cancel);
        assert!(!read.changed && !read.moved);
        assert_eq!(state.reader.counts(), (1, 1), "unchanged: stats only");
        stage(&root, "gates");
        assert!(state.read(&root, Job::Reconcile, false, &cancel).changed);
        assert_eq!(state.reader.counts(), (1, 2), "a kept file: one read, no scan");
        std::fs::create_dir(root.join("turns")).unwrap();
        // A new folder entry changes the folder's time: one full scan.
        let read = state.read(&root, Job::Reconcile, false, &cancel);
        assert!(read.moved, "a new `turns` folder asks for a new attach");
        state.read(&root, Job::Full, false, &cancel);
        std::fs::write(root.join("manifest.json"), "{}").unwrap();
        assert!(state.read(&root, Job::Reconcile, false, &cancel).changed);
        assert_eq!(state.reader.counts().0, 3);
    }

    const LIVE_LINE: &str = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Live\"}]},\"parent_tool_use_id\":null}\n";

    /// A round in the plan stage with one agent and its live output.
    fn live_round(dir: &Path, text: &str) {
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        stage(dir, "plan");
        std::fs::write(dir.join("turns/plan-opus.prompt.md"), "public").unwrap();
        std::fs::write(dir.join("turns/plan-opus.raw.err"), LIVE_LINE.replace("Live", text)).unwrap();
    }

    /// Moves the round away and puts a new folder with other live output in its place.
    fn replace_round(root: &Path) {
        std::fs::rename(root, root.with_extension("old")).unwrap();
        std::fs::create_dir(root).unwrap();
        live_round(root, "New");
    }

    fn external_state(root: &Path) -> State {
        State::new(root, true)
    }

    fn keys(trust: bool, names: &[&str]) -> Job {
        Job::Keys { keys: names.iter().map(|n| (*n).to_owned()).collect(), trust }
    }

    #[test]
    fn a_replaced_folder_reads_nothing_until_a_full_read_resets_the_live_output() {
        let root = temp_round();
        live_round(&root, "Old");
        let cancel = AtomicBool::new(false);
        let mut state = external_state(&root);
        assert!(!state.read(&root, Job::Full, false, &cancel).tick.unwrap().records.is_empty());
        replace_round(&root);
        for job in [keys(true, &["turns/plan-opus.raw.err"]), Job::Reconcile, Job::None] {
            let read = state.read(&root, job, true, &cancel);
            assert!(read.moved && !read.changed && read.tick.is_none());
        }
        let read = state.read(&root, Job::Full, false, &cancel);
        let tick = read.tick.unwrap();
        assert!(!read.moved && tick.reset);
        assert!(tick.records.iter().any(|(_, r)| r["message"]["content"][0]["text"] == "New"));
        // The same folder again: no reset.
        assert!(!state.read(&root, Job::Reconcile, false, &cancel).tick.unwrap().reset);
    }

    #[test]
    fn a_folder_replaced_during_a_read_sends_nothing_until_a_full_read_resets() {
        let new_text = |tick: &Tick| tick.records.iter().any(|(_, r)| r["message"]["content"][0]["text"] == "New");
        for job in [|| keys(true, &["turns/plan-opus.raw.err"]), || Job::Reconcile, || Job::None, || Job::Full] {
            let root = temp_round();
            live_round(&root, "Old");
            let cancel = AtomicBool::new(false);
            let mut state = external_state(&root);
            state.read(&root, Job::Full, false, &cancel);
            // The replacement comes after the first identity check, before the last one.
            let moved = root.clone();
            state.during = Some(Box::new(move || replace_round(&moved)));
            let read = state.read(&root, job(), true, &cancel);
            assert!(read.moved && !read.changed && read.tick.is_none(), "a read across a replacement sends nothing");
            let read = state.read(&root, Job::Full, false, &cancel);
            assert!(!read.moved && read.changed, "the new folder sends its room");
            let tick = read.tick.unwrap();
            assert!(tick.reset, "the new folder starts a new generation");
            assert!(new_text(&tick));
        }
    }

    #[tokio::test]
    async fn a_replaced_folder_sends_its_live_output_in_a_new_generation() {
        let root = temp_round();
        live_round(&root, "Old");
        let mut probe = start_with(&root, attach_none, None, true);
        let first = match timeout(WAIT, probe.rx.recv()).await.unwrap().unwrap() {
            RoomEvent::Room { generation, .. } => generation,
            _ => panic!("the first event is the room"),
        };
        replace_round(&root);
        // A live event, not a folder event, comes first.
        probe.event(EventKind::Modify(ModifyKind::Any), "turns/plan-opus.raw.err");
        let mut room = None;
        timeout(WAIT, async {
            loop {
                match probe.rx.recv().await.unwrap() {
                    RoomEvent::Room { generation, .. } => room = room.or(Some(generation)),
                    RoomEvent::Reset { generation, .. } => assert!(generation > first),
                    RoomEvent::Live { generation, record, .. } => {
                        if record["message"]["content"][0]["text"] == "New" {
                            assert!(generation > first, "no new-folder record in the old generation");
                            assert_eq!(room, Some(generation), "the room comes first");
                            break;
                        }
                    }
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_open_live_file_ticks_with_no_event_until_the_stage_ends() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "public").unwrap();
        let mut probe = start_with(&root, attach_none, None, true);
        probe.room().await;
        let counts = probe.counts();
        // The agent keeps its file open. No event is signalled.
        let mut file = std::fs::File::create(root.join("turns/plan-opus.raw.err")).unwrap();
        for text in ["One", "Two"] {
            file.write_all(LIVE_LINE.replace("Live", text).as_bytes()).unwrap();
            timeout(Duration::from_secs(2), async {
                while !matches!(probe.rx.recv().await, Some(RoomEvent::Live { ref record, .. })
                    if record["message"]["content"][0]["text"] == text)
                {}
            })
            .await
            .expect("a live record with no event");
        }
        assert_eq!(probe.counts(), counts, "the live tick scans and reads no round file");
        // The stage ends: no live sources, so no live tick.
        stage(&root, "gates");
        probe.event(EventKind::Modify(ModifyKind::Any), "events.jsonl");
        probe.until(|r| active(r) == Some("gates")).await;
        // A live tick would see the new folder and send its room.
        std::fs::rename(root.join("turns"), root.join("turns.old")).unwrap();
        std::fs::create_dir(root.join("turns")).unwrap();
        assert!(probe.quiet().await, "no live tick after the stage ends");
        // A positive control: the next read finds the new folder.
        probe.event(EventKind::Modify(ModifyKind::Any), "events.jsonl");
        probe.room().await;
    }

    #[test]
    fn a_keyed_entry_change_needs_no_scan_at_the_light_check() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut state = State::new(&root, false);
        state.read(&root, Job::Full, false, &cancel);
        assert_eq!(state.reader.counts().0, 1);
        // A create.
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "p").unwrap();
        assert!(state.read(&root, keys(true, &["turns/plan-opus.prompt.md"]), false, &cancel).changed);
        assert!(!state.read(&root, Job::Reconcile, false, &cancel).changed);
        // A rename.
        std::fs::write(root.join("turns/staged.tmp"), "p").unwrap();
        std::fs::rename(root.join("turns/staged.tmp"), root.join("turns/plan-sol.prompt.md")).unwrap();
        assert!(state.read(&root, keys(true, &["turns/plan-sol.prompt.md"]), false, &cancel).changed);
        state.read(&root, Job::Reconcile, false, &cancel);
        assert_eq!(state.reader.counts().0, 1, "no scan after a trusted keyed read");
        // A positive control: an untrusted keyed read leaves the scan to the light check.
        std::fs::write(root.join("turns/plan-zed.prompt.md"), "p").unwrap();
        state.read(&root, keys(false, &["turns/plan-zed.prompt.md"]), false, &cancel);
        state.read(&root, Job::Reconcile, false, &cancel);
        assert_eq!(state.reader.counts().0, 2);
    }

    #[test]
    fn a_replaced_folder_still_needs_a_scan() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        let cancel = AtomicBool::new(false);
        let mut state = State::new(&root, false);
        state.read(&root, Job::Full, false, &cancel);
        std::fs::rename(root.join("turns"), root.join("turns.old")).unwrap();
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "p").unwrap();
        assert!(state.read(&root, keys(true, &["turns/plan-opus.prompt.md"]), false, &cancel).moved);
        assert!(state.read(&root, Job::Reconcile, false, &cancel).moved);
        assert!(state.read(&root, Job::Full, false, &cancel).changed);
        assert_eq!(state.reader.counts().0, 2);
        assert_eq!(state.room.agents, ["opus"]);
    }

    #[test]
    fn a_create_remove_or_rename_of_an_entry_is_structural() {
        let root = Path::new("/round");
        for (kind, structural) in [
            (create(), true),
            (EventKind::Remove(notify::event::RemoveKind::File), true),
            (EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Any)), true),
            (EventKind::Modify(ModifyKind::Any), false),
        ] {
            let mut flags = Flags::default();
            let event = notify::Event::new(kind).add_path(root.join("turns/plan-opus.prompt.md"));
            classify(root, Ok(event), &mut flags);
            assert_eq!(flags.structural, structural);
        }
    }

    #[tokio::test]
    async fn the_last_status_stays_after_the_run_drops_its_sender() {
        let root = temp_round();
        let status = |state| Status {
            state,
            stage: None,
            pausing: false,
            picked: None,
            pickable: Vec::new(),
            decided: false,
            ship: None,
            base: None,
        };
        let (tx, rx) = watch::channel(status(RunState::Running));
        let mut probe = start_with(&root, attach_none, Some(rx), false);
        probe.room().await;
        tx.send_replace(status(RunState::Done));
        drop(tx);
        assert_eq!(probe.room().await.1.unwrap().state, RunState::Done);
        stage(&root, "gates");
        probe.event(create(), "events.jsonl");
        let (room, run) = probe.room().await;
        assert_eq!(active(&room), Some("gates"));
        assert_eq!(run, Some(status(RunState::Done)));
    }

    #[tokio::test]
    async fn a_status_change_sends_the_last_room_without_a_read() {
        let root = temp_round();
        let status = |state| Status {
            state,
            stage: None,
            pausing: false,
            picked: None,
            pickable: Vec::new(),
            decided: false,
            ship: None,
            base: None,
        };
        let (tx, rx) = watch::channel(status(RunState::Running));
        let mut probe = start_with(&root, attach_none, Some(rx), false);
        let (_, run) = probe.room().await;
        assert_eq!(run.unwrap().state, RunState::Running);
        tx.send_replace(status(RunState::Paused));
        let (room, run) = probe.room().await;
        assert_eq!(run.unwrap().state, RunState::Paused);
        assert_eq!(active(&room), Some("plan"));
        assert_eq!(probe.counts(), (1, 1));
    }

    // --- lifecycle ----------------------------------------------------------

    fn len(watches: &Watches) -> usize {
        lock(&watches.entries).len()
    }

    async fn until(done: impl Fn() -> bool) {
        let end = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < end, "not in time");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_failed_send_retires_the_watch() {
        let root = temp_round();
        let watches = Watches::default();
        watches.watch_with(attach_none, "main", root, None, false, |_| false);
        until(|| len(&watches) == 0).await;
    }

    #[tokio::test]
    async fn retire_is_idempotent_and_drop_owner_ends_only_its_watches() {
        let root = temp_round();
        let watches = Watches::default();
        let (tx, mut rx) = unbounded_channel();
        let main = watches.watch_with(attach_none, "main", root.clone(), None, false, |_| true);
        watches.watch_with(attach_none, "other", root.clone(), None, false, move |e| tx.send(e).is_ok());
        watches.watch_with(attach_none, "main", root, None, false, |_| true);
        assert_eq!(len(&watches), 3);
        watches.retire(main);
        watches.retire(main);
        assert_eq!(len(&watches), 2);
        watches.drop_owner("main");
        assert_eq!(len(&watches), 1);
        assert!(matches!(timeout(WAIT, rx.recv()).await, Ok(Some(RoomEvent::Room { .. }))));
    }

    #[tokio::test]
    async fn a_retired_watch_sends_nothing() {
        let root = temp_round();
        let watches = Watches::default();
        let (tx, mut rx) = unbounded_channel();
        let token = watches.watch("main", root.clone(), None, false, move |e| tx.send(e).is_ok());
        assert!(matches!(timeout(WAIT, rx.recv()).await, Ok(Some(RoomEvent::Room { .. }))));
        watches.retire(token);
        stage(&root, "gates");
        // The aborted worker drops its sender, so the channel closes.
        assert!(matches!(timeout(WAIT, rx.recv()).await, Ok(None)));
    }

    #[test]
    fn the_end_of_a_watch_waits_for_a_send_and_blocks_later_sends() {
        let feed = feed();
        let sent = Arc::new(AtomicU64::new(0));
        let reset = || RoomEvent::Reset { generation: 1, sequence: 0, agent: "opus".into() };
        let (entered, started) = std::sync::mpsc::channel();
        let sender = {
            let (feed, sent) = (feed.clone(), sent.clone());
            std::thread::spawn(move || {
                let send = |_| {
                    entered.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                    sent.fetch_add(1, Ordering::SeqCst);
                    true
                };
                feed.deliver(send, reset())
            })
        };
        started.recv().unwrap();
        // The end of the watch waits for the send in progress.
        drop(Guard { feed: feed.clone(), tasks: Vec::new() });
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        let late = |_| {
            sent.fetch_add(1, Ordering::SeqCst);
            true
        };
        assert!(!feed.deliver(late, reset()));
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        assert!(sender.join().unwrap());
    }

    fn status(state: RunState) -> Status {
        Status {
            state,
            stage: None,
            pausing: false,
            picked: None,
            pickable: Vec::new(),
            decided: false,
            ship: None,
            base: None,
        }
    }

    #[tokio::test]
    async fn a_replacement_during_the_first_read_sends_only_the_new_room() {
        let root = temp_round();
        stage(&root, "gates");
        let moved = root.clone();
        let during: Box<dyn FnOnce() + Send> = Box::new(move || replace_round(&moved));
        let mut probe = start_feed(&root, attach_none, None, false, feed(), Some(during));
        // The old folder is in the gates stage, the new one in the plan stage.
        let (room, _) = probe.room().await;
        assert_eq!(active(&room), Some("plan"), "no room of the replaced folder");
    }

    #[test]
    fn a_replacement_during_a_keyed_read_keeps_the_old_room() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "p").unwrap();
        let cancel = AtomicBool::new(false);
        let mut state = State::new(&root, false);
        state.read(&root, Job::Full, false, &cancel);
        let before = state.room.clone();
        std::fs::write(root.join("turns/plan-sol.prompt.md"), "p").unwrap();
        let moved = root.clone();
        state.during = Some(Box::new(move || replace_round(&moved)));
        assert!(state.read(&root, keys(true, &["turns/plan-sol.prompt.md"]), false, &cancel).moved);
        assert_eq!(state.room, before);
    }

    #[tokio::test]
    async fn a_status_change_after_a_replaced_keyed_read_sends_no_mixed_room() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        std::fs::write(root.join("turns/plan-opus.prompt.md"), "p").unwrap();
        let (tx, rx) = watch::channel(status(RunState::Running));
        let mut probe = start_with(&root, attach_none, Some(rx), false);
        assert_eq!(probe.room().await.0.agents, ["opus"]);
        // The keyed read sees `sol` in the old folder, then the folder is replaced.
        std::fs::write(root.join("turns/plan-sol.prompt.md"), "p").unwrap();
        let moved = root.clone();
        *probe.shared.during.lock().unwrap() = Some(Box::new(move || {
            replace_round(&moved);
            tx.send_replace(status(RunState::Paused));
        }));
        probe.event(create(), "turns/plan-sol.prompt.md");
        // The status frame and the full read can come in each order. No frame has `sol`.
        let mut paused = false;
        while let Ok(Some(event)) = timeout(QUIET, probe.rx.recv()).await {
            if let RoomEvent::Room { room, run, .. } = event {
                assert_eq!(room.agents, ["opus"], "no room of two folders");
                paused |= run.is_some_and(|r| r.state == RunState::Paused);
            }
        }
        assert!(paused, "the status change sends a room");
    }

    /// Starts a room whose end is the guard that `end` holds, like a watch of `Watches`.
    fn start_ended(root: &Path, end: &Arc<Mutex<Option<Guard>>>, during: Option<Box<dyn FnOnce() + Send>>) -> Probe {
        let feed = feed();
        let probe = start_feed(root, attach_none, None, false, feed.clone(), during);
        *end.lock().unwrap() = Some(Guard { feed, tasks: vec![probe.task.abort_handle()] });
        probe
    }

    #[tokio::test]
    async fn an_end_during_a_full_read_sends_nothing() {
        let root = temp_round();
        std::fs::create_dir(root.join("turns")).unwrap();
        for i in 0..500 {
            std::fs::write(root.join(format!("turns/notes-{i}.txt")), "x").unwrap();
        }
        let end = Arc::new(Mutex::new(None));
        let retire = end.clone();
        let during: Box<dyn FnOnce() + Send> = Box::new(move || drop(retire.lock().unwrap().take()));
        let mut probe = start_ended(&root, &end, Some(during));
        assert!(matches!(timeout(WAIT, probe.rx.recv()).await, Ok(None)), "no frame, and the channel closes");
    }

    #[tokio::test]
    async fn an_end_during_the_debounce_sends_nothing() {
        let root = temp_round();
        let end = Arc::new(Mutex::new(None));
        let mut probe = start_ended(&root, &end, None);
        probe.room().await;
        stage(&root, "gates");
        probe.event(create(), "events.jsonl");
        // The worker wakes and waits for the burst to end.
        tokio::task::yield_now().await;
        drop(end.lock().unwrap().take());
        assert!(matches!(timeout(WAIT, probe.rx.recv()).await, Ok(None)), "no frame, and the channel closes");
    }

    static WATCHERS: AtomicU64 = AtomicU64::new(0);
    static WATCHERS_ENDED: AtomicU64 = AtomicU64::new(0);

    struct Counted;

    impl Drop for Counted {
        fn drop(&mut self) {
            WATCHERS_ENDED.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn attach_counted(_: &Path, _: Weak<Shared>) -> Option<Attached> {
        WATCHERS.fetch_add(1, Ordering::SeqCst);
        Some(Attached { _watcher: Box::new(Counted), complete: true })
    }

    #[tokio::test]
    async fn retire_and_drop_owner_end_the_os_watchers() {
        let root = temp_round();
        let watches = Watches::default();
        let (tx, mut rx) = unbounded_channel();
        let send = move |e| tx.send(e).is_ok();
        let token = watches.watch_with(attach_counted, "main", root.clone(), None, false, send.clone());
        assert!(matches!(timeout(WAIT, rx.recv()).await, Ok(Some(RoomEvent::Room { .. }))));
        assert_eq!(WATCHERS.load(Ordering::SeqCst), 1);
        assert_eq!(WATCHERS_ENDED.load(Ordering::SeqCst), 0);
        watches.retire(token);
        until(|| WATCHERS_ENDED.load(Ordering::SeqCst) == 1).await;
        watches.watch_with(attach_counted, "other", root, None, false, send);
        assert!(matches!(timeout(WAIT, rx.recv()).await, Ok(Some(RoomEvent::Room { .. }))));
        assert_eq!(WATCHERS.load(Ordering::SeqCst), 2);
        watches.drop_owner("other");
        until(|| WATCHERS_ENDED.load(Ordering::SeqCst) == 2).await;
    }
}
