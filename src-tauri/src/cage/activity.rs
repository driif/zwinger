//! The network log of each VM key, in memory and on disk. Each VM key has
//! two JSON Lines segments in `<store dir>/activity`, named by its
//! [`file_id`]: `<id>.jsonl`, the current one, and `<id>.1.jsonl`, the one
//! before it. The entries in memory are the entries of the two segments.
//! All calls run under the state lock, so a late event cannot write the
//! log of a removed key.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use sha2::{Digest as _, Sha256};

use crate::cage::manager::{NetSink, NoticeSink, State, lock};
use crate::cage::vm_state;
use crate::error::{Error, Result};
use crate::proxy::log::Entry;
use crate::store::{self, PrivateEntry};

/// The folder of the logs, below the store dir.
pub(crate) const DIR: &str = "activity";
/// A segment rotates at this many entries.
pub(crate) const SEGMENT_ENTRIES: usize = 1000;
/// A segment rotates before it gets larger than this.
pub(crate) const SEGMENT_BYTES: usize = 2 << 20;
/// The largest line of an entry, with its newline. A larger entry is dropped.
pub(crate) const LINE_LIMIT: usize = 16 << 10;

/// The largest seq. The page reads a seq as a JavaScript number, which
/// is exact up to this value. A saved line with a larger seq is skipped.
pub(crate) const SEQ_LIMIT: u64 = (1 << 53) - 1;

/// The name of the segments of VM `key` without their suffix: the lowercase
/// hex SHA-256 of the key. It has a fixed length, and two keys that differ
/// only in case or in a `.1` suffix get different names.
pub(crate) fn file_id(key: &str) -> String {
    Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn current(key: &str) -> String {
    format!("{}.jsonl", file_id(key))
}

fn previous(key: &str) -> String {
    format!("{}.1.jsonl", file_id(key))
}

/// The [`file_id`] in segment name `name`. `None`: `name` is no segment name.
fn segment_id(name: &str) -> Option<&str> {
    let id = name.strip_suffix(".jsonl")?;
    let id = id.strip_suffix(".1").unwrap_or(id);
    (id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(id)
}

/// A line, with its newline, that a segment can hold.
fn fits(line: &[u8]) -> bool {
    line.len() <= LINE_LIMIT
}

/// The sender of the entries of one VM key, such as its proxy or its runtime
/// log. It stays valid over a stop and a start of the VM, and ends when the
/// log of the key is removed, so it cannot write to a later VM with the same key.
#[derive(Clone)]
pub(crate) struct Source {
    key: String,
    id: u64,
}

/// Records `entry` of `source` and sends it to the net sink. An entry of a
/// key that no session, cage or VM has, or of an ended source, is dropped.
/// A new app notice signals the notice sink.
pub(crate) fn record<V>(
    state: &Mutex<State<V>>,
    net: &OnceLock<NetSink>,
    notice: &OnceLock<NoticeSink>,
    source: &Source,
    entry: Entry,
) {
    let (entry, noticed) = {
        let mut state = lock(state);
        if !state.key_taken(&source.key) || state.net.sources.get(&source.key) != Some(&source.id) {
            return;
        }
        let entry = state.net.record(&source.key, entry);
        (entry, std::mem::take(&mut state.net.noticed))
    };
    if noticed && let Some(sink) = notice.get() {
        sink();
    }
    if let (Some(entry), Some(sink)) = (entry, net.get()) {
        sink(&source.key, &entry);
    }
}

pub(crate) struct Activity {
    /// The store dir. The logs are in its [`DIR`].
    parent: PathBuf,
    /// The loaded streams. A stream loads at its first use.
    streams: HashMap<String, Stream>,
    /// The app notices of the log, not taken yet.
    notices: Vec<String>,
    /// A notice was added since [`record`] last looked.
    noticed: bool,
    /// Per VM key, the id of its valid [`Source`].
    sources: HashMap<String, u64>,
    /// The id of the last source. Ids only go up.
    last_source: u64,
}

#[derive(Default)]
struct Stream {
    /// The entries of the previous segment, then of the current one.
    entries: VecDeque<Entry>,
    /// The number of `entries` in the previous segment.
    previous: usize,
    /// The bytes of the current segment.
    bytes: usize,
    /// The load skipped a line: the segments are written again from
    /// `entries` before the next append.
    rewrite: bool,
    /// A write of this stream failed and the notice was given.
    warned: bool,
    /// An entry was too large and the notice was given.
    oversized: bool,
    /// An entry got no seq, as the last one is [`SEQ_LIMIT`], and the notice was given.
    exhausted: bool,
    /// The saved segments could not be read, for this reason. The stream
    /// then keeps its entries in memory only, from seq 1, until the app
    /// quits, so the unread segments stay as they are.
    detached: Option<String>,
}

/// The valid entries of one segment and their bytes. `skipped`: a line was not used.
#[derive(Default)]
struct Segment {
    entries: Vec<Entry>,
    bytes: usize,
    skipped: bool,
}

/// `e` with `path` in front of its text.
fn at(path: &Path, e: std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

impl Activity {
    pub(crate) fn new(parent: &Path) -> Self {
        Self {
            parent: parent.to_owned(),
            streams: HashMap::new(),
            notices: Vec::new(),
            noticed: false,
            sources: HashMap::new(),
            last_source: 0,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.parent.join(DIR).join(name)
    }

    fn notice(&mut self, text: String) {
        self.notices.push(text);
        self.noticed = true;
    }

    /// The source of VM `key`: the same one until [`Activity::remove`].
    pub(crate) fn source(&mut self, key: &str) -> Source {
        let id = match self.sources.get(key) {
            Some(id) => *id,
            None => {
                self.last_source += 1;
                self.sources.insert(key.to_owned(), self.last_source);
                self.last_source
            }
        };
        Source { key: key.to_owned(), id }
    }

    /// Deletes each segment that is not of a key in `taken`, a set of
    /// [`file_id`]s. Only a name of the segment form goes, and only if it is
    /// ours. A folder or file of another user stays, with a notice.
    pub(crate) fn prune(&mut self, taken: &HashSet<String>) {
        let dir = match store::open_private_existing_dir(&self.parent, DIR) {
            Ok(Some(dir)) => dir,
            Ok(None) => return,
            Err(e) => {
                let path = self.parent.join(DIR);
                self.notice(format!("zwinger did not clean up the network logs in {}: {e}.", path.display()));
                return;
            }
        };
        let names: Vec<String> = match rustix::fs::Dir::read_from(&dir) {
            Ok(names) => names.filter_map(|e| e.ok()?.file_name().to_str().ok().map(str::to_owned)).collect(),
            Err(e) => {
                eprintln!("{} could not be read: {e}", self.parent.join(DIR).display());
                return;
            }
        };
        for name in names {
            if segment_id(&name).is_none_or(|id| taken.contains(id)) {
                continue;
            }
            if let Err(e) = store::remove_own(&dir, &name) {
                self.notice(format!("zwinger did not delete the old network log {}: {e}.", self.path(&name).display()));
            }
        }
    }

    /// The entries of VM `key`, oldest first. A log that cannot be read is
    /// an error, and the next call reads it again.
    pub(crate) fn events(&mut self, key: &str) -> Result<Vec<Entry>> {
        if !vm_state::vm_key(key) {
            return Err(Error::BadVmKey(key.to_owned()));
        }
        if !self.streams.contains_key(key) {
            let stream = self.load(key)?;
            self.streams.insert(key.to_owned(), stream);
        }
        let stream = &self.streams[key];
        if let Some(reason) = &stream.detached {
            return Err(Error::Io(std::io::Error::other(format!(
                "The saved network log of {key} could not be read: {reason}. The new events show until zwinger quits."
            ))));
        }
        Ok(stream.entries.iter().cloned().collect())
    }

    /// The notices since the last call.
    pub(crate) fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// Forgets the stream and the source of VM `key` and deletes its two
    /// segments. A later stream of the key starts empty, at seq 1.
    pub(crate) fn remove(&mut self, key: &str) -> Result<()> {
        self.streams.remove(key);
        self.sources.remove(key);
        if !vm_state::vm_key(key) {
            return Ok(());
        }
        let Some(dir) = store::open_private_existing_dir(&self.parent, DIR)? else {
            return Ok(());
        };
        for name in [current(key), previous(key)] {
            store::remove_own(&dir, &name).map_err(|e| at(&self.path(&name), e))?;
        }
        Ok(())
    }

    /// Numbers the entry, adds it to the stream of VM `key` and appends it
    /// to the current segment. An entry with a line over [`LINE_LIMIT`] is
    /// dropped with one notice for the stream, and its seq is not used. A
    /// failed write keeps the entry in memory and gives one notice for the
    /// stream. A log that cannot be read makes a detached stream.
    pub(crate) fn record(&mut self, key: &str, mut entry: Entry) -> Option<Entry> {
        if !self.streams.contains_key(key) {
            let stream = match vm_state::vm_key(key).then(|| self.load(key)) {
                None => Stream::default(),
                Some(Ok(stream)) => stream,
                Some(Err(e)) => {
                    self.notice(format!(
                        "The saved network log of {key} could not be read: {e}. zwinger does not change it. The new events show until zwinger quits, but they are not saved."
                    ));
                    Stream { detached: Some(e.to_string()), ..Stream::default() }
                }
            };
            self.streams.insert(key.to_owned(), stream);
        }
        let stream = self.streams.get_mut(key).expect("the stream is loaded");
        let last = stream.entries.back().map_or(0, |last| last.seq);
        let Some(seq) = last.checked_add(1).filter(|seq| *seq <= SEQ_LIMIT) else {
            if !std::mem::replace(&mut stream.exhausted, true) {
                self.notice(format!(
                    "zwinger did not log the new events of {key}: its saved log has the last event number. Remove the cage, or its VM, to start a new log."
                ));
            }
            return None;
        };
        entry.seq = seq;
        let mut line = serde_json::to_vec(&entry).expect("an entry serializes");
        line.push(b'\n');
        if !fits(&line) {
            if !std::mem::replace(&mut stream.oversized, true) {
                self.notice(format!(
                    "zwinger did not log an event of {key} of {} bytes: the limit is {LINE_LIMIT} bytes. The network rules still applied to it.",
                    line.len()
                ));
            }
            return None;
        }
        let rotate =
            stream.entries.len() - stream.previous >= SEGMENT_ENTRIES || stream.bytes + line.len() > SEGMENT_BYTES;
        if rotate {
            stream.entries.drain(..stream.previous);
            stream.previous = stream.entries.len();
            stream.bytes = 0;
        }
        let saved = vm_state::vm_key(key) && stream.detached.is_none();
        let written = if saved { self.write(key, &line, rotate) } else { Ok(()) };
        let stream = self.streams.get_mut(key).expect("the stream is loaded");
        stream.entries.push_back(entry.clone());
        stream.bytes += line.len();
        let Err(e) = written else {
            return Some(entry);
        };
        // A partial append or rotation can leave a broken line or a large
        // segment, so the next append writes the segments again first.
        stream.rewrite = true;
        if !std::mem::replace(&mut stream.warned, true) {
            let path = self.path(&current(key));
            self.notice(format!(
                "The network log of {key} could not be saved to {}: {e}. The events show until zwinger quits, but the unsaved ones are lost then.",
                path.display()
            ));
        }
        Some(entry)
    }

    /// The disk part of [`Activity::record`]: the rewrite after a load that
    /// skipped a line, the rotation, then the append.
    fn write(&mut self, key: &str, line: &[u8], rotate: bool) -> Result<()> {
        let dir = store::open_private_dir(&self.parent, DIR)?;
        let stream = self.streams.get_mut(key).expect("the stream is loaded");
        // A rewrite already writes the segments as they are after the rotation.
        let rotate = rotate && !stream.rewrite;
        if stream.rewrite {
            let (old, new) = stream.entries.as_slices();
            let all: Vec<&Entry> = old.iter().chain(new).collect();
            let (before, after) = all.split_at(stream.previous);
            store::replace_in(&dir, &previous(key), &lines(before))?;
            store::replace_in(&dir, &current(key), &lines(after))?;
            stream.rewrite = false;
        }
        if rotate {
            store::rename_in(&dir, &current(key), &previous(key))?;
        }
        store::open_append(&dir, &current(key))?.write_all(line)?;
        Ok(())
    }

    /// Reads the two segments of VM `key`. A missing log is empty. A line
    /// that cannot be used is skipped, and the segments are then written
    /// again before the next append. A read error, or a folder or file of
    /// another user, is an error.
    fn load(&self, key: &str) -> std::io::Result<Stream> {
        let Some(dir) =
            store::open_private_existing_dir(&self.parent, DIR).map_err(|e| at(&self.parent.join(DIR), e))?
        else {
            return Ok(Stream::default());
        };
        let mut seq = 0;
        let old = self.read(&dir, &previous(key), &mut seq)?;
        let new = self.read(&dir, &current(key), &mut seq)?;
        let previous = old.entries.len();
        let mut entries: VecDeque<Entry> = old.entries.into();
        entries.extend(new.entries);
        Ok(Stream { entries, previous, bytes: new.bytes, rewrite: old.skipped || new.skipped, ..Stream::default() })
    }

    /// The valid lines of segment `name`, with seqs above `seq` and at
    /// most [`SEQ_LIMIT`]. It reads at most one segment and one line, and
    /// keeps the newest entries whose lines, as [`lines`] writes them, fit
    /// in [`SEGMENT_ENTRIES`] and [`SEGMENT_BYTES`]. A line that is not
    /// written like that marks the segment to be written again. A link or a special file in its place
    /// holds no line.
    fn read(&self, dir: &OwnedFd, name: &str, seq: &mut u64) -> std::io::Result<Segment> {
        let path = self.path(name);
        let mut segment = Segment::default();
        let mut bytes = Vec::new();
        match store::open_private_read(dir, name).map_err(|e| at(&path, e))? {
            PrivateEntry::Missing => return Ok(segment),
            PrivateEntry::Special => {
                segment.skipped = true;
                return Ok(segment);
            }
            PrivateEntry::File(file) => {
                file.take((SEGMENT_BYTES + LINE_LIMIT + 1) as u64).read_to_end(&mut bytes).map_err(|e| at(&path, e))?;
            }
        }
        // A file over the limit is written again, and the rest of a larger file is not read.
        segment.skipped = bytes.len() > SEGMENT_BYTES;
        // The sizes are of the lines as `lines` writes them again.
        let mut kept: VecDeque<(Entry, usize)> = VecDeque::new();
        let mut sum = 0;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            let parsed = (line.ends_with(b"\n") && fits(line))
                .then(|| serde_json::from_slice::<Entry>(line).ok())
                .flatten()
                .filter(|entry| entry.seq > *seq && entry.seq <= SEQ_LIMIT);
            let Some(entry) = parsed else {
                segment.skipped = true;
                continue;
            };
            let mut normal = serde_json::to_vec(&entry).expect("an entry serializes");
            normal.push(b'\n');
            if !fits(&normal) {
                segment.skipped = true;
                continue;
            }
            *seq = entry.seq;
            if normal != line {
                segment.skipped = true;
            }
            sum += normal.len();
            kept.push_back((entry, normal.len()));
            while kept.len() > SEGMENT_ENTRIES || sum > SEGMENT_BYTES {
                let (_, len) = kept.pop_front().expect("an entry is kept");
                sum -= len;
                segment.skipped = true;
            }
        }
        segment.bytes = sum;
        segment.entries = kept.into_iter().map(|(entry, _)| entry).collect();
        Ok(segment)
    }
}

/// The entries as JSON Lines. An entry whose line does not [`fits`] is not
/// written, as [`Activity::record`] does not keep it.
fn lines(entries: &[&Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        let mut line = serde_json::to_vec(entry).expect("an entry serializes");
        line.push(b'\n');
        if fits(&line) {
            out.extend(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};

    use base64::Engine as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::cage::testing::*;
    use crate::session::VmChoice;

    fn log_dir(manager: &Manager<FakeRuntime>) -> PathBuf {
        manager.store.dir().join(DIR)
    }

    fn seg(dir: &Path, name: String) -> PathBuf {
        dir.join(DIR).join(name)
    }

    fn entry(host: &str) -> Entry {
        Entry::new("GET", host, Some("/x"))
    }

    fn file_lines(path: &Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |s| s.lines().count())
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Records `entry` for `key` through a new source, as the proxy does.
    fn send(manager: &Manager<FakeRuntime>, key: &str, entry: Entry) {
        let source = lock(&manager.state).net.source(key);
        record(&manager.state, &manager.net_sink, &manager.notice_sink, &source, entry);
    }

    /// Sends a plain request through the running proxy of VM `id`, which is offline.
    async fn proxy_request(runtime: &FakeRuntime) {
        let net = runtime.net.lock().unwrap().clone().unwrap();
        let url = net.env.iter().find(|(name, _)| name == "HTTP_PROXY").unwrap().1.clone();
        let userinfo = url.split_once("//").unwrap().1.split_once('@').unwrap().0;
        let auth = base64::engine::general_purpose::STANDARD.encode(userinfo);
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", net.port)).await.unwrap();
        let request = format!(
            "GET http://example.com/q?token=secret HTTP/1.1\r\nhost: example.com\r\nproxy-authorization: Basic {auth}\r\nconnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
    }

    async fn wait_for(manager: &Manager<FakeRuntime>, id: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while manager.list_net_events(id).unwrap().len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the events did not arrive");
    }

    #[tokio::test]
    async fn the_log_of_both_sources_stays_after_a_restart_of_the_app() {
        let (manager, runtime, id) = manager();
        manager.set_offline(&id, true).unwrap();
        manager.start(&id).await.unwrap();
        proxy_request(&runtime).await;
        wait_for(&manager, &id, 1).await;
        net_sender(&runtime, 0).send(dns_denied("2999-01-01T00:00:00Z", "pypi.org")).await.unwrap();
        wait_for(&manager, &id, 2).await;
        manager.stop(&id, None).await.unwrap();
        let before = manager.list_net_events(&id).unwrap();
        assert_eq!(hosts(&before), [(1, "example.com"), (2, "pypi.org")]);
        assert_eq!(before[0].path.as_deref(), Some("/q?token"));
        let saved = std::fs::read_to_string(log_dir(&manager).join(current(&id))).unwrap();
        assert!(!saved.contains("secret"), "{saved}");
        assert_eq!(mode(&log_dir(&manager)), 0o700);
        // The log of a key that no session or cage has goes at the start.
        std::fs::write(log_dir(&manager).join(current("stranger")), "x\n").unwrap();
        std::fs::write(log_dir(&manager).join(previous("stranger")), "x\n").unwrap();
        // A file that is no log stays.
        let other = format!("{}.2.jsonl", file_id("stranger"));
        let kept = ["notes.txt", ".hidden.jsonl", "a b.jsonl", "stranger.jsonl", other.as_str()];
        for name in kept {
            std::fs::write(log_dir(&manager).join(name), "keep\n").unwrap();
        }

        let fresh = Manager::new(
            FakeRuntime::default(),
            Store::new(manager.store.dir().to_owned()),
            vault(),
            temp_dir(),
            host(),
        )
        .unwrap();
        assert_eq!(fresh.list_net_events(&id).unwrap(), before);
        assert!(!log_dir(&manager).join(current("stranger")).exists());
        assert!(!log_dir(&manager).join(previous("stranger")).exists());
        for name in kept {
            assert!(log_dir(&manager).join(name).exists(), "{name}");
        }
        let next = lock(&fresh.state).net.record(&id, entry("a.io")).unwrap();
        assert_eq!(next.seq, 3);
    }

    #[tokio::test]
    async fn the_export_writes_the_whole_log() {
        let (manager, _, id) = manager();
        send(&manager, &id, entry("a.io"));
        let dest = temp_dir().join("out").join("log.json");
        manager.export_net_events(&id, &dest).unwrap();
        let export: serde_json::Value = serde_json::from_slice(&std::fs::read(&dest).unwrap()).unwrap();
        assert_eq!((export["version"].as_u64(), export["vmKey"].as_str()), (Some(1), Some(id.as_str())));
        assert!(export["exportedAt"].as_u64().is_some());
        let events: Vec<Entry> = serde_json::from_value(export["events"].clone()).unwrap();
        assert_eq!(events, manager.list_net_events(&id).unwrap());
        assert!(matches!(manager.export_net_events("../x", &dest), Err(Error::BadVmKey(_))));
    }

    #[test]
    fn the_segments_rotate_and_memory_equals_disk() {
        let dir = temp_dir();
        let mut activity = Activity::new(&dir);
        for n in 0..2500 {
            activity.record("k", entry(&format!("h{n}.io")));
        }
        let events = activity.events("k").unwrap();
        assert_eq!(events.len(), 1500);
        assert_eq!((events[0].seq, events[1499].seq), (1001, 2500));
        assert_eq!((file_lines(&seg(&dir, previous("k"))), file_lines(&seg(&dir, current("k")))), (1000, 500));
        assert_eq!(Activity::new(&dir).events("k").unwrap(), events);

        // Large entries rotate by bytes.
        let mut large = entry("big.io");
        large.note = Some("n".repeat(10_000));
        for _ in 0..500 {
            activity.record("b", large.clone());
        }
        for name in [current("b"), previous("b")] {
            assert!(std::fs::metadata(seg(&dir, name)).unwrap().len() <= SEGMENT_BYTES as u64);
        }
        assert_eq!(Activity::new(&dir).events("b").unwrap(), activity.events("b").unwrap());
        assert!(activity.take_notices().is_empty());
    }

    /// The line of an entry with `seq`, without its newline, padded with its
    /// note to `len` bytes with the newline.
    fn sized_line(seq: u64, len: usize) -> String {
        let mut e = entry(&format!("h{seq}.io"));
        (e.seq, e.at, e.note) = (seq, 7, Some(String::new()));
        let base = serde_json::to_string(&e).unwrap().len() + 1;
        e.note = Some("x".repeat(len - base));
        serde_json::to_string(&e).unwrap()
    }

    /// The lines of `seqs`, each of `len` bytes, as one file.
    fn sized_file(seqs: std::ops::RangeInclusive<u64>, len: usize) -> String {
        seqs.map(|seq| sized_line(seq, len) + "\n").collect()
    }

    /// The segments of `key` are in the bounds, their seqs go up, and a
    /// reload gives `events`.
    fn assert_bounded(dir: &Path, key: &str, events: &[Entry]) {
        let mut seqs = Vec::new();
        for name in [previous(key), current(key)] {
            let text = std::fs::read_to_string(seg(dir, name)).unwrap_or_default();
            assert!(text.len() <= SEGMENT_BYTES, "{}", text.len());
            for line in text.lines() {
                assert!(line.len() < LINE_LIMIT);
                seqs.push(serde_json::from_str::<Entry>(line).unwrap().seq);
            }
        }
        assert!(seqs.len() <= 2 * SEGMENT_ENTRIES);
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(Activity::new(dir).events(key).unwrap(), events);
    }

    // Each segment has fewer than SEGMENT_ENTRIES lines, but more than
    // SEGMENT_BYTES. The load keeps the newest lines that fit.
    #[test]
    fn a_segment_over_the_byte_limit_loads_its_newest_lines_that_fit() {
        let (manager, _, id) = manager();
        let dir = manager.store.dir().to_owned();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let (rows, len) = (900, 2340);
        let size = rows as usize * len;
        assert!(size > SEGMENT_BYTES && size < SEGMENT_BYTES + LINE_LIMIT);
        std::fs::write(seg(&dir, previous(&id)), sized_file(1..=rows, len)).unwrap();
        std::fs::write(seg(&dir, current(&id)), sized_file(rows + 1..=2 * rows, len)).unwrap();

        let fit = (SEGMENT_BYTES / len) as u64;
        let expected: Vec<u64> = (rows - fit + 1..=rows).chain(2 * rows - fit + 1..=2 * rows).collect();
        let loaded = manager.list_net_events(&id).unwrap();
        assert_eq!(loaded.iter().map(|e| e.seq).collect::<Vec<_>>(), expected);

        send(&manager, &id, entry("new.io"));
        let events = manager.list_net_events(&id).unwrap();
        assert_eq!(events.len(), expected.len() + 1);
        assert_eq!(events.last().unwrap().seq, 2 * rows + 1);
        assert_bounded(&dir, &id, &events);
        let dest = temp_dir().join("log.json");
        manager.export_net_events(&id, &dest).unwrap();
        let export: serde_json::Value = serde_json::from_slice(&std::fs::read(&dest).unwrap()).unwrap();
        let exported: Vec<Entry> = serde_json::from_value(export["events"].clone()).unwrap();
        assert_eq!(exported, events);
    }

    // The bound counts the lines as they are written again: a padded line
    // gets shorter, and a line without its optional fields gets longer.
    #[test]
    fn the_byte_limit_counts_the_lines_as_they_are_written_again() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let rows = 900;
        // Padded: 2340 bytes on disk, less when written again.
        let padded: String = (1..=rows)
            .map(|seq| {
                let line = sized_line(seq, 1000).replacen('{', "{ ", 1).replace(",\"", " ,  \"");
                format!("{line}{}\n", " ".repeat(2339 - line.len()))
            })
            .collect();
        assert!(padded.len() > SEGMENT_BYTES && padded.len() < SEGMENT_BYTES + LINE_LIMIT);
        // Missing optional fields: 2330 bytes on disk, more when written again.
        let missing: String = (rows + 1..=2 * rows)
            .map(|seq| {
                let line = |note: &str| {
                    format!(
                        "{{\"seq\":{seq},\"at\":7,\"host\":\"h.io\",\"up\":0,\"down\":0,\"ms\":0,\"allowed\":true,\"reason\":\"r\",\"credentials\":[],\"note\":\"{note}\"}}\n"
                    )
                };
                line(&"x".repeat(2330 - line("").len()))
            })
            .collect();
        assert!(missing.len() <= SEGMENT_BYTES);
        std::fs::write(seg(&dir, previous("k")), &padded).unwrap();
        std::fs::write(seg(&dir, current("k")), &missing).unwrap();

        let mut activity = Activity::new(&dir);
        let loaded = activity.events("k").unwrap();
        let normal = |e: &Entry| serde_json::to_vec(e).unwrap().len() + 1;
        let old: Vec<_> = loaded.iter().filter(|e| e.seq <= rows).collect();
        let new: Vec<_> = loaded.iter().filter(|e| e.seq > rows).collect();
        assert_eq!(old.len(), rows as usize);
        assert!(normal(old[0]) < 2340);
        let fit = SEGMENT_BYTES / normal(new[0]);
        assert!(normal(new[0]) > 2330 && fit < rows as usize);
        assert_eq!(
            new.iter().map(|e| e.seq).collect::<Vec<_>>(),
            (2 * rows - fit as u64 + 1..=2 * rows).collect::<Vec<_>>()
        );

        activity.record("k", entry("new.io")).unwrap();
        let events = activity.events("k").unwrap();
        let written: Vec<&Entry> = events.iter().filter(|e| e.seq <= rows).collect();
        assert_eq!(std::fs::read(seg(&dir, previous("k"))).unwrap(), lines(&written));
        assert_bounded(&dir, "k", &events);
    }

    #[test]
    fn a_rejected_normalized_entry_does_not_hide_valid_following_entries() {
        for split in [false, true] {
            let (manager, _, id) = manager();
            let dir = manager.store.dir().to_owned();
            std::fs::create_dir_all(dir.join(DIR)).unwrap();
            let line = |seq, host| {
                let mut event = entry(host);
                (event.seq, event.at) = (seq, 7);
                serde_json::to_string(&event).unwrap() + "\n"
            };
            let mut rejected = serde_json::json!({
                "seq": 100, "at": 7, "host": "rejected.io", "up": 0, "down": 0,
                "ms": 0, "allowed": false, "reason": "direct", "credentials": [], "note": ""
            });
            let base = serde_json::to_vec(&rejected).unwrap().len() + 1;
            rejected["note"] = serde_json::json!("x".repeat(LINE_LIMIT - base));
            let rejected = serde_json::to_string(&rejected).unwrap() + "\n";
            assert_eq!(rejected.len(), LINE_LIMIT);
            let parsed: Entry = serde_json::from_str(&rejected).unwrap();
            assert!(serde_json::to_vec(&parsed).unwrap().len() + 1 > LINE_LIMIT);
            let first = line(1, "first.io");
            let second = line(2, "second.io");
            if split {
                std::fs::write(seg(&dir, previous(&id)), first + &rejected).unwrap();
                std::fs::write(seg(&dir, current(&id)), second).unwrap();
            } else {
                std::fs::write(seg(&dir, current(&id)), first + &rejected + &second).unwrap();
            }

            assert_eq!(hosts(&manager.list_net_events(&id).unwrap()), [(1, "first.io"), (2, "second.io")]);
            send(&manager, &id, entry("new.io"));
            let events = manager.list_net_events(&id).unwrap();
            assert_eq!(hosts(&events), [(1, "first.io"), (2, "second.io"), (3, "new.io")]);
            assert_bounded(&dir, &id, &events);
            let dest = temp_dir().join("log.json");
            manager.export_net_events(&id, &dest).unwrap();
            let export: serde_json::Value = serde_json::from_slice(&std::fs::read(dest).unwrap()).unwrap();
            let exported: Vec<Entry> = serde_json::from_value(export["events"].clone()).unwrap();
            assert_eq!(exported, events);
        }
    }

    // A file over the read cap gives bounded entries, and the next append
    // writes bounded files.
    #[test]
    fn a_segment_over_the_read_cap_is_bounded_after_the_next_append() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let text = sized_file(1..=990, 3200);
        assert!(text.len() > SEGMENT_BYTES + LINE_LIMIT);
        std::fs::write(seg(&dir, current("k")), &text).unwrap();

        let mut activity = Activity::new(&dir);
        let loaded = activity.events("k").unwrap();
        assert_eq!(loaded.len(), SEGMENT_BYTES / 3200);
        // Only the start of the file is read, and its last line is cut.
        let read = ((SEGMENT_BYTES + LINE_LIMIT + 1) / 3200) as u64;
        assert_eq!(loaded.last().unwrap().seq, read);

        let next = activity.record("k", entry("new.io")).unwrap();
        assert_eq!(next.seq, read + 1);
        assert_bounded(&dir, "k", &activity.events("k").unwrap());
    }

    // DOT_KEY and DOT_KEY_DELETE: `a` and `a.1` are two keys with their own segments.
    #[test]
    fn keys_that_differ_by_a_dot_suffix_or_by_case_keep_their_own_logs() {
        let dir = temp_dir();
        let mut activity = Activity::new(&dir);
        activity.record("a.1", entry("other.io"));
        activity.record("Ab", entry("upper.io"));
        assert!(Activity::new(&dir).events("a").unwrap().is_empty());
        for n in 0..1001 {
            activity.record("a", entry(&format!("h{n}.io")));
        }
        activity.record("ab", entry("lower.io"));
        let fresh = || Activity::new(&dir);
        assert_eq!(hosts(&fresh().events("a.1").unwrap()), [(1, "other.io")]);
        assert_eq!(hosts(&fresh().events("Ab").unwrap()), [(1, "upper.io")]);
        assert_eq!(hosts(&fresh().events("ab").unwrap()), [(1, "lower.io")]);
        assert_eq!(fresh().events("a").unwrap().len(), 1001);
        activity.remove("a").unwrap();
        assert!(fresh().events("a").unwrap().is_empty());
        assert_eq!(hosts(&fresh().events("a.1").unwrap()), [(1, "other.io")]);
        assert!(activity.take_notices().is_empty());
    }

    // LONG_KEY: the longest valid key rotates and loads.
    #[test]
    fn a_key_of_255_bytes_keeps_its_log() {
        let dir = temp_dir();
        let key = "k".repeat(255);
        assert!(vm_state::vm_key(&key));
        let mut activity = Activity::new(&dir);
        for n in 0..1001 {
            activity.record(&key, entry(&format!("h{n}.io")));
        }
        assert!(activity.take_notices().is_empty());
        assert_eq!(Activity::new(&dir).events(&key).unwrap().len(), 1001);
    }

    // REUSED_KEY: a source of a removed own key cannot write to the next VM with that key.
    #[tokio::test]
    async fn a_source_of_a_removed_key_writes_nothing_after_the_key_comes_back() {
        let (manager, _, a) = manager();
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        manager.set_session_cage(&b, Some(&a)).unwrap();
        manager.set_session_vm(&b, true, None).await.unwrap();
        let own = || match &lock(&manager.state).session(&b).unwrap().vm {
            VmChoice::Own { key, .. } => key.clone(),
            VmChoice::Shared => panic!("no own VM"),
        };
        let key = own();
        let old = lock(&manager.state).net.source(&key);
        // A stop and a start keep the source.
        assert_eq!(lock(&manager.state).net.source(&key).id, old.id);
        record(&manager.state, &manager.net_sink, &manager.notice_sink, &old, entry("old.io"));
        assert_eq!(manager.list_net_events(&key).unwrap().len(), 1);

        manager.set_session_vm(&b, false, None).await.unwrap();
        manager.set_session_vm(&b, true, None).await.unwrap();
        assert_eq!(own(), key);
        record(&manager.state, &manager.net_sink, &manager.notice_sink, &old, entry("late.io"));
        assert!(manager.list_net_events(&key).unwrap().is_empty());
        send(&manager, &key, entry("new.io"));
        assert_eq!(hosts(&manager.list_net_events(&key).unwrap()), [(1, "new.io")]);
    }

    // OVERSIZED_RECORD: a too large entry is not kept, sent, saved or exported.
    #[tokio::test]
    async fn an_entry_over_the_line_limit_is_dropped_with_one_notice() {
        let (manager, _, id) = manager();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let signals = Arc::new(Mutex::new(0));
        let (into, count) = (sent.clone(), signals.clone());
        manager.set_net_sink(Box::new(move |_, entry| into.lock().unwrap().push(entry.host.clone())));
        manager.set_notice_sink(Box::new(move || *count.lock().unwrap() += 1));
        let large = entry(&format!("{}.io", "h".repeat(LINE_LIMIT)));
        send(&manager, &id, large.clone());
        send(&manager, &id, large);
        send(&manager, &id, entry("a.io"));
        assert_eq!(hosts(&manager.list_net_events(&id).unwrap()), [(1, "a.io")]);
        assert_eq!(*sent.lock().unwrap(), ["a.io"]);
        assert_eq!(*signals.lock().unwrap(), 1);
        assert!(std::fs::metadata(log_dir(&manager).join(current(&id))).unwrap().len() < LINE_LIMIT as u64);
        let notices = manager.take_app_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains(&format!("{LINE_LIMIT} bytes")), "{notices:?}");
        let dest = temp_dir().join("log.json");
        manager.export_net_events(&id, &dest).unwrap();
        assert!(std::fs::metadata(&dest).unwrap().len() < LINE_LIMIT as u64);
    }

    #[test]
    fn a_removed_key_starts_empty_and_a_late_event_writes_nothing() {
        let dir = temp_dir();
        let mut activity = Activity::new(&dir);
        activity.record("k", entry("a.io"));
        activity.record("k", entry("b.io"));
        activity.remove("k").unwrap();
        assert!(std::fs::read_dir(dir.join(DIR)).unwrap().next().is_none());
        assert!(activity.events("k").unwrap().is_empty());
        assert_eq!(activity.record("k", entry("c.io")).unwrap().seq, 1);
        assert!(matches!(activity.events("../x"), Err(Error::BadVmKey(_))));
    }

    #[tokio::test]
    async fn an_event_of_a_deleted_cage_creates_no_log() {
        let (manager, _, id) = manager();
        let source = lock(&manager.state).net.source(&id);
        record(&manager.state, &manager.net_sink, &manager.notice_sink, &source, entry("a.io"));
        assert_eq!(manager.list_net_events(&id).unwrap().len(), 1);
        manager.delete(&id).await.unwrap();
        manager.delete_cage(&id).unwrap();
        assert!(!log_dir(&manager).join(current(&id)).exists());
        record(&manager.state, &manager.net_sink, &manager.notice_sink, &source, entry("late.io"));
        assert!(!log_dir(&manager).join(current(&id)).exists());
        assert!(manager.list_net_events(&id).unwrap().is_empty());
    }

    #[test]
    fn a_corrupt_or_truncated_tail_is_skipped_and_written_again() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let line = |seq: u64| {
            let mut e = entry(&format!("h{seq}.io"));
            (e.seq, e.at) = (seq, 7);
            serde_json::to_string(&e).unwrap()
        };
        let long = format!("{{\"seq\":9,\"note\":\"{}\"}}", "x".repeat(LINE_LIMIT));
        let text = format!("{}\nnot json\n{}\n{long}\n{}\n{}", line(1), line(3), line(2), &line(4)[..20]);
        std::fs::write(seg(&dir, current("k")), text).unwrap();
        let mut activity = Activity::new(&dir);
        assert_eq!(hosts(&activity.events("k").unwrap()), [(1, "h1.io"), (3, "h3.io")]);
        assert_eq!(activity.record("k", entry("new.io")).unwrap().seq, 4);
        let saved = std::fs::read_to_string(seg(&dir, current("k"))).unwrap();
        assert_eq!(
            saved,
            format!(
                "{}\n{}\n{}\n",
                line(1),
                line(3),
                serde_json::to_string(&activity.events("k").unwrap()[2]).unwrap()
            )
        );
    }

    // A saved seq over the limit is skipped. A log at the limit drops new
    // entries with one notice, and does not overflow.
    #[test]
    fn a_saved_seq_at_or_over_the_limit_does_not_overflow() {
        let line = |seq: u64| {
            let mut e = entry(&format!("h{seq}.io"));
            (e.seq, e.at) = (seq, 7);
            serde_json::to_string(&e).unwrap()
        };
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        std::fs::write(seg(&dir, current("k")), format!("{}\n{}\n", line(1), line(u64::MAX))).unwrap();
        let mut activity = Activity::new(&dir);
        assert_eq!(hosts(&activity.events("k").unwrap()), [(1, "h1.io")]);
        assert_eq!(activity.record("k", entry("a.io")).unwrap().seq, 2);

        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        std::fs::write(seg(&dir, current("k")), format!("{}\n", line(SEQ_LIMIT))).unwrap();
        let mut activity = Activity::new(&dir);
        assert!(activity.record("k", entry("a.io")).is_none());
        assert!(activity.record("k", entry("b.io")).is_none());
        assert_eq!(activity.take_notices().len(), 1);
        assert_eq!(file_lines(&seg(&dir, current("k"))), 1);
    }

    // A log file with a second name is not read, changed or appended to:
    // the other name can be a file outside of the log folder.
    #[test]
    fn a_log_with_a_hard_link_is_refused() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let outside = temp_dir().join("precious");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, "keep\n").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(&outside, seg(&dir, current("k"))).unwrap();
        let mut activity = Activity::new(&dir);
        let err = activity.events("k").unwrap_err().to_string();
        assert!(err.contains("more than one link"), "{err}");
        activity.record("k", entry("a.io"));
        assert_eq!(activity.take_notices().len(), 1);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");
        assert_eq!(mode(&outside), 0o644);

        // A link made after the load is refused at the append.
        let dir = temp_dir();
        let mut activity = Activity::new(&dir);
        activity.record("k", entry("a.io"));
        std::fs::remove_file(seg(&dir, current("k"))).unwrap();
        std::fs::hard_link(&outside, seg(&dir, current("k"))).unwrap();
        activity.record("k", entry("b.io"));
        assert_eq!(activity.take_notices().len(), 1);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");
        assert_eq!(mode(&outside), 0o644);
    }

    #[test]
    fn a_link_or_fifo_in_place_of_a_log_is_not_used() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let outside = temp_dir().join("precious");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, "keep\n").unwrap();
        std::os::unix::fs::symlink(&outside, seg(&dir, current("k"))).unwrap();
        let fifo = std::ffi::CString::new(seg(&dir, current("f")).to_str().unwrap()).unwrap();
        // SAFETY: mkfifo reads the C string that it gets.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let mut activity = Activity::new(&dir);
        for key in ["k", "f"] {
            assert!(activity.events(key).unwrap().is_empty());
            activity.record(key, entry("a.io"));
            let meta = std::fs::symlink_metadata(seg(&dir, current(key))).unwrap();
            assert!(meta.file_type().is_file() && !meta.file_type().is_fifo());
            assert_eq!(file_lines(&seg(&dir, current(key))), 1);
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");
        assert!(activity.take_notices().is_empty());
    }

    #[test]
    fn the_next_append_after_a_failed_write_writes_the_segments_again() {
        let dir = temp_dir();
        let mut activity = Activity::new(&dir);
        activity.record("k", entry("a.io"));
        let path = seg(&dir, current("k"));
        // A folder in place of the segment makes the append fail; a broken
        // line from a partial append is in the place of the segment after it.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        activity.record("k", entry("b.io"));
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "{\"seq\":1,\"broken").unwrap();
        activity.record("k", entry("c.io"));
        let events = activity.events("k").unwrap();
        assert_eq!(hosts(&events), [(1, "a.io"), (2, "b.io"), (3, "c.io")]);
        assert_eq!(file_lines(&path), 3);
        assert_eq!(Activity::new(&dir).events("k").unwrap(), events);
        assert_eq!(activity.take_notices().len(), 1);
    }

    #[test]
    fn existing_storage_gets_private_permissions_on_write() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let path = seg(&dir, current("k"));
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(dir.join(DIR), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut activity = Activity::new(&dir);
        activity.record("k", entry("a.io"));
        assert_eq!((mode(&dir.join(DIR)), mode(&path)), (0o700, 0o600));
        assert_eq!(file_lines(&path), 1);
        assert!(activity.take_notices().is_empty());
    }

    // A list, an export and the prune at the start make the storage private too.
    #[tokio::test]
    async fn existing_storage_gets_private_permissions_on_read() {
        let (manager, _, id) = manager();
        send(&manager, &id, entry("a.io"));
        let (folder, path) = (log_dir(&manager), log_dir(&manager).join(current(&id)));
        let public = || {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let fresh = || {
            Manager::new(
                FakeRuntime::default(),
                Store::new(manager.store.dir().to_owned()),
                vault(),
                temp_dir(),
                host(),
            )
            .unwrap()
        };
        public();
        assert_eq!(fresh().list_net_events(&id).unwrap().len(), 1);
        assert_eq!((mode(&folder), mode(&path)), (0o700, 0o600));
        public();
        let other = fresh();
        assert_eq!(mode(&folder), 0o700);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        other.export_net_events(&id, &temp_dir().join("log.json")).unwrap();
        assert_eq!((mode(&folder), mode(&path)), (0o700, 0o600));
        assert!(other.take_app_notices().is_empty());
    }

    // An owned file of mode 000 is made readable again and keeps its entries.
    #[test]
    fn a_log_of_mode_000_is_repaired_and_stays_intact() {
        let dir = temp_dir();
        let mut activity = Activity::new(&dir);
        activity.record("k", entry("a.io"));
        let path = seg(&dir, current("k"));
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut fresh = Activity::new(&dir);
        assert_eq!(hosts(&fresh.events("k").unwrap()), [(1, "a.io")]);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(fresh.record("k", entry("b.io")).unwrap().seq, 2);
        assert_eq!(file_lines(&path), 2);
    }

    // A read error is an error, is tried again, and the next event detaches
    // the stream: it shows, has its own seqs and changes nothing on disk.
    #[test]
    fn a_log_that_cannot_be_read_is_an_error_and_stays_as_it_is() {
        let dir = temp_dir();
        std::fs::create_dir_all(seg(&dir, current("k"))).unwrap();
        std::fs::write(seg(&dir, current("k")).join("inside"), "keep\n").unwrap();
        let mut activity = Activity::new(&dir);
        assert!(activity.events("k").is_err());
        assert!(activity.events("k").is_err());
        assert!(activity.take_notices().is_empty());
        assert_eq!(activity.record("k", entry("a.io")).unwrap().seq, 1);
        assert_eq!(activity.record("k", entry("b.io")).unwrap().seq, 2);
        assert!(activity.events("k").is_err());
        assert_eq!(activity.take_notices().len(), 1);
        assert_eq!(std::fs::read_to_string(seg(&dir, current("k")).join("inside")).unwrap(), "keep\n");
        assert!(!seg(&dir, previous("k")).exists());

        // A folder in place of the log folder is no log folder.
        let dir = temp_dir();
        std::fs::write(dir.join(DIR), "not a folder").unwrap();
        let mut activity = Activity::new(&dir);
        activity.record("k", entry("a.io"));
        activity.record("k", entry("b.io"));
        assert!(activity.events("k").is_err());
        let notices = activity.take_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains(&dir.join(DIR).display().to_string()), "{notices:?}");
        assert_eq!(std::fs::read_to_string(dir.join(DIR)).unwrap(), "not a folder");
    }

    #[tokio::test]
    async fn an_export_of_a_log_that_cannot_be_read_fails() {
        let (manager, _, id) = manager();
        send(&manager, &id, entry("a.io"));
        let fresh = Manager::new(
            FakeRuntime::default(),
            Store::new(manager.store.dir().to_owned()),
            vault(),
            temp_dir(),
            host(),
        )
        .unwrap();
        let path = log_dir(&manager).join(current(&id));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let dest = temp_dir().join("log.json");
        assert!(fresh.export_net_events(&id, &dest).is_err());
        assert!(!dest.exists());
    }

    // A file of another user is not read, changed or deleted. A hard link to
    // a file of root stands in for it.
    #[test]
    fn a_log_of_another_user_is_refused() {
        let foreign = Path::new("/etc/hosts");
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let path = seg(&dir, current("k"));
        let stranger = seg(&dir, current("stranger"));
        if std::fs::hard_link(foreign, &path).is_err() || std::fs::hard_link(foreign, &stranger).is_err() {
            eprintln!("skipped: no hard link to a file of another user");
            return;
        }
        let (before, before_mode) = (std::fs::read(foreign).unwrap(), mode(foreign));
        let mut activity = Activity::new(&dir);
        activity.prune(&HashSet::from([file_id("k")]));
        assert!(stranger.exists());
        let err = activity.events("k").unwrap_err().to_string();
        assert!(err.contains("another user"), "{err}");
        activity.record("k", entry("a.io"));
        assert!(activity.remove("k").is_err());
        let notices = activity.take_notices();
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(path.exists());
        assert_eq!((std::fs::read(foreign).unwrap(), mode(foreign)), (before, before_mode));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&stranger).unwrap();

        let err = store::open_private_existing_dir(Path::new("/"), "usr").unwrap_err();
        assert!(err.to_string().contains("another user"), "{err}");
        assert_eq!(mode(Path::new("/usr")), 0o755);
    }
}
