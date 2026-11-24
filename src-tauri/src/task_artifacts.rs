//! The files of a round, in the duo shape: `events.jsonl`, `turns/plan-*.json`,
//! `turns/t<n>-*.json`, `tiebreak.json`, `implementation.json`, `gates.json`,
//! `crossreview.json`, `revision.json`, `revision-gates.json`, `final-*.json`, `manifest.json`.
//!
//! One reader projects a round folder into a typed [`Room`], for in-app runs
//! and for duo rounds that run outside the app. A round folder can be written
//! while it is read, so a file that does not parse yet counts as absent. Each
//! file is opened relative to its folder without following links, and only up
//! to `FILE_LIMIT` bytes.
//!
//! The writer keeps the authoritative round in host app data. A mirror in the
//! harness gets copies, which the app never reads back.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::AgentKind;
use crate::agent::cli::{iso, now_ms};
use crate::error::{Error, Result};
use crate::session::{TURN_LIMIT, TaskAgent};
use crate::task_participants::{INDEPENDENT, participants};
use crate::task_runs::EXTERNAL;

/// A larger file of a round is not read.
const FILE_LIMIT: u64 = 8 << 20;
const SUMMARY_CHARS: usize = 200;
/// What a reader keeps of a round folder.
const READ_FILES: usize = 64;
const READ_BYTES: u64 = 32 << 20;
/// An event detail keeps its start within this size.
const DETAIL_LIMIT: usize = 4 << 10;
/// The end of `events.jsonl` that only the events of `ESSENTIAL` can use.
const EVENTS_RESERVE: u64 = 64 << 10;
/// The events that the room needs to show the stages and the end of a run.
const ESSENTIAL: [&str; 4] = ["stage", "done", "cancelled", "run_failed"];
/// The exchange turns that a round with no recorded turns shows before it has more.
pub const TURNS: u32 = 2;
pub(crate) const TURNS_DIR: &str = "turns";
/// A full scan checks its cancel flag after this many directory entries.
const CANCEL_EVERY: u64 = 4096;

/// The order of the stages. `exchange` repeats once per turn.
const STAGES: [&str; 10] = [
    "plan",
    "exchange",
    "tiebreak",
    "implement",
    "gates",
    "crossreview",
    "revise",
    "revision-gates",
    "final",
    "independent-review",
];

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    /// The participant ids, in the order of the round.
    pub agents: Vec<String>,
    /// The participants, in the order of the round.
    pub participants: Vec<RoomParticipant>,
    pub stages: Vec<Stage>,
    /// The participant named by the active stage event, when it names one.
    pub active_actor: Option<String>,
    /// The typed messages of the round, in order.
    pub messages: Vec<Message>,
    /// What each agent wants to build now.
    pub positions: Vec<Position>,
    pub turns: Vec<Turn>,
    pub disagreements: Vec<Disagreement>,
    pub concessions: Vec<Concession>,
    pub implementer: Option<String>,
    /// How the implementer was chosen: `agreement`, `tiebreak`, `walkover` or `user`.
    pub decided_by: Option<String>,
    pub tiebreak: Option<Tiebreak>,
    pub crossreview: Option<Review>,
    pub finals: Vec<Final>,
    /// The independent reviewer, if the run has one. It is no participant.
    pub independent: Option<IndependentReview>,
    pub outcome: Option<Outcome>,
    pub degraded: Vec<Notice>,
    pub violations: Vec<Notice>,
    pub gates: Vec<Gate>,
    pub questions: Vec<String>,
    /// `done`, `failed`, `stopped` or `cancelled`. `None` while it runs.
    pub ended: Option<String>,
    /// The time of the end event.
    pub ended_at: Option<String>,
}

/// A participant as the round records it. `kind` is `None` for a name
/// that is no provider kind, such as in an external duo round.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomParticipant {
    pub id: String,
    pub kind: Option<AgentKind>,
    pub model: Option<String>,
    /// False when the round saved no model for this participant, so
    /// `model: None` does not mean the configured default.
    #[serde(default = "recorded")]
    pub model_recorded: bool,
    /// The last model that its CLI reported in the run (a `model` event).
    #[serde(default, skip_deserializing)]
    pub reported_model: Option<String>,
}

fn recorded() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stage {
    /// `plan`, `exchange-1`, `tiebreak`, ...
    pub id: String,
    pub label: String,
    /// `done`, `active`, `pending`, `skipped` or `stopped`.
    pub state: &'static str,
    pub started: Option<String>,
}

/// One message of the round: `{from, to, kind, body}`, which the A2A
/// bridge exposes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Stable: the source of the message in the round, such as
    /// `turns/plan-opus.json` or `messages.jsonl#3` (a line from 1).
    pub id: String,
    /// An agent name, `run`, `user` or `a2a`.
    pub from: String,
    /// An agent name, `all` or `user`.
    pub to: String,
    /// `plan`, `reply`, `tiebreak`, `implementation`, `gates`, `review`,
    /// `revision`, `final`, `user`, `answer`, `external` or `event`.
    pub kind: String,
    pub stage: String,
    pub at: Option<String>,
    pub summary: String,
    /// Markdown.
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub agent: String,
    pub plan: String,
    pub implementer: Option<String>,
    /// `plan` or `turn <n>`.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub turn: u32,
    pub replies: Vec<Reply>,
    pub converged: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    pub agent: String,
    pub agreed: bool,
    pub implementer: Option<String>,
    pub conceded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Disagreement {
    pub turn: u32,
    /// `implementer` or `agreement`.
    pub about: &'static str,
    pub detail: String,
    /// `turn <n>`, `tiebreak`, `user`, or `None` while it is open.
    pub resolved: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Concession {
    pub turn: u32,
    pub agent: String,
    pub point: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tiebreak {
    pub implementer: Option<String>,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub reviewer: Option<String>,
    pub verdict: String,
    pub findings: Vec<Finding>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub severity: String,
    pub file: String,
    pub line: Option<i64>,
    pub what: String,
    pub why: String,
    pub fix: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Final {
    pub agent: String,
    pub ship: bool,
    pub blocking: Vec<Blocking>,
    pub residual: Vec<String>,
    pub task_met: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndependentReview {
    /// Its id is always `independent`.
    pub reviewer: RoomParticipant,
    /// The state of its stage.
    pub state: &'static str,
    pub review: Option<Final>,
    /// `approved`, `denied`, `invalid` or `missing`, from the manifest.
    pub verdict: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Blocking {
    pub what: String,
    pub why: String,
    #[serde(rename = "where")]
    pub at: String,
}

/// The manifest of an ended round.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub ship: bool,
    pub ship_basis: String,
    pub blocking: Vec<Blocking>,
    pub backlog: Vec<String>,
    /// Why the run stopped before its end, if it did.
    pub stopped: Option<String>,
    /// The commit at the start of an in-app run.
    pub base: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub agent: Option<String>,
    pub stage: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gate {
    pub command: String,
    /// `None` if the gate did not exit, as after a timeout.
    pub code: Option<i64>,
    pub output: String,
}

// --- reading ---------------------------------------------------------------

fn nofollow_dir(dir: &Path) -> Option<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC;
    rustix::fs::open(dir, flags, Mode::empty()).ok()
}

/// The identity, the times and the size of a regular file, else `None`.
/// Equal stamps mean an unchanged file, also after an atomic replacement
/// with the same size and modification time.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: (i64, i64),
    changed: (i64, i64),
    size: u64,
    dev: u64,
    ino: u64,
}

fn stamp_at(dir: &OwnedFd, name: &str) -> Option<Stamp> {
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    (FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile).then_some(Stamp {
        modified: (stat.st_mtime as i64, stat.st_mtime_nsec as i64),
        changed: (stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        size: stat.st_size as u64,
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    })
}

/// The file, if it is a regular file of at most `FILE_LIMIT` bytes.
fn read_at(dir: &OwnedFd, name: &str) -> Option<Vec<u8>> {
    // NONBLOCK, so a FIFO does not block the open.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = std::fs::File::from(rustix::fs::openat(dir, name, flags, Mode::empty()).ok()?);
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > FILE_LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// What a reader does with a file name of a round, by its relative name:
/// `Some(true)` reads it, `Some(false)` only keeps its name (a plan prompt
/// names its agent), `None` skips it.
/// The fixed files of a round folder. A reader takes them before the
/// files of each agent.
const ROOT: [&str; 13] = [
    "run.json",
    "checkpoint.json",
    "events.jsonl",
    "messages.jsonl",
    "manifest.json",
    "tiebreak.json",
    "implementation.json",
    "gates.json",
    "crossreview.json",
    "revision.json",
    "revision-gates.json",
    "independent-review.json",
    "open-questions.txt",
];

pub(crate) fn known(key: &str) -> Option<bool> {
    let agent = |rest: &str| !rest.is_empty() && !rest.starts_with('.') && !rest.contains('/');
    if ROOT.contains(&key) || key.strip_prefix("final-").and_then(|r| r.strip_suffix(".json")).is_some_and(agent) {
        return Some(true);
    }
    let turn = key.strip_prefix("turns/t").and_then(|r| r.split_once('-')).is_some_and(|(n, rest)| {
        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && rest.strip_suffix(".json").is_some_and(agent)
    });
    match key.strip_prefix("turns/plan-") {
        Some(rest) if rest.strip_suffix(".json").is_some_and(agent) => Some(true),
        Some(rest) if agent(rest.split('.').next().unwrap_or_default()) => Some(false),
        _ if turn => Some(true),
        _ => None,
    }
}

/// A known file of a round, as the reader keeps it.
struct Entry {
    stamp: Stamp,
    bytes: Option<Vec<u8>>,
    /// The reader tried to read this stamp. A file that is too large or
    /// cannot be read stays `None` until its stamp changes.
    tried: bool,
}

/// A round folder, read again only where a file changed.
pub struct RoundReader {
    dir: PathBuf,
    /// By relative name (`turns/plan-opus.json`).
    files: BTreeMap<String, Entry>,
    /// The full scans and the file reads so far, for tests of the work.
    #[cfg_attr(not(test), allow(dead_code))]
    scans: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    reads: u64,
}

impl RoundReader {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, files: BTreeMap::new(), scans: 0, reads: 0 }
    }

    /// Reads the known files that are new or changed, up to `READ_FILES`
    /// files and `READ_BYTES` bytes. Returns true if any file came, went or
    /// changed.
    pub fn refresh(&mut self) -> bool {
        self.scan(&AtomicBool::new(false))
    }

    /// [`refresh`](Self::refresh), which stops early when `cancel` is set.
    /// The time is linear in the number of entries of the round folder and
    /// of `turns`, also of the entries that are not round files.
    pub fn scan(&mut self, cancel: &AtomicBool) -> bool {
        self.scans += 1;
        let mut seen = BTreeMap::new();
        let mut changed = false;
        let mut bytes_kept = 0;
        let mut entries_seen = 0u64;
        for (sub, prefix) in [(None, ""), (Some(TURNS_DIR), "turns/")] {
            let path = match sub {
                Some(sub) => self.dir.join(sub),
                None => self.dir.clone(),
            };
            let Some(dir) = nofollow_dir(&path) else {
                continue;
            };
            let Ok(entries) = Dir::read_from(&dir) else {
                continue;
            };
            // The fixed files first, so the files of many agents or turns
            // cannot push them out. Each list stays within the file limit.
            let (mut fixed, mut other) = (Vec::new(), Vec::new());
            for entry in entries.flatten() {
                // Also before the first entry, so a cancelled scan reads nothing.
                if entries_seen.is_multiple_of(CANCEL_EVERY) && cancel.load(Ordering::Relaxed) {
                    return false;
                }
                entries_seen += 1;
                let Ok(name) = entry.file_name().to_str() else {
                    continue;
                };
                let key = format!("{prefix}{name}");
                let Some(read) = known(&key) else {
                    continue;
                };
                if ROOT.contains(&key.as_str()) {
                    fixed.push((key, name.to_owned(), read));
                } else if other.len() < READ_FILES {
                    other.push((key, name.to_owned(), read));
                }
            }
            for (key, name, read) in fixed.into_iter().chain(other) {
                if seen.len() == READ_FILES {
                    break;
                }
                let Some(stamp) = stamp_at(&dir, &name) else {
                    continue;
                };
                let old = self.files.remove(&key);
                let entry = self.next(old, stamp, read, bytes_kept, (&dir, &name), &mut changed);
                bytes_kept += entry.bytes.as_ref().map_or(0, |b| b.len() as u64);
                seen.insert(key, entry);
            }
        }
        // Each file that is still there was taken out above, so the rest is gone.
        changed |= !self.files.is_empty();
        self.files = seen;
        changed
    }

    /// Reads again only the files of `keys`, which a watcher reported. A
    /// name that is not a round file is skipped. A new file that does not
    /// fit in `READ_FILES` waits for the next full scan. Returns true if any
    /// of the files came, went or changed.
    pub fn refresh_keys<'a>(&mut self, keys: impl IntoIterator<Item = &'a str>) -> bool {
        let root = nofollow_dir(&self.dir);
        let turns = root.as_ref().and_then(|_| nofollow_dir(&self.dir.join(TURNS_DIR)));
        let mut changed = false;
        for key in keys {
            let Some(read) = known(key) else {
                continue;
            };
            let (dir, name) = match key.strip_prefix("turns/") {
                Some(name) => (turns.as_ref(), name),
                None => (root.as_ref(), key),
            };
            let Some((dir, stamp)) = dir.and_then(|dir| Some((dir, stamp_at(dir, name)?))) else {
                changed |= self.files.remove(key).is_some();
                continue;
            };
            let old = self.files.remove(key);
            if old.is_none() && self.files.len() >= READ_FILES {
                continue;
            }
            let bytes_kept = self.files.values().filter_map(|e| e.bytes.as_ref()).map(|b| b.len() as u64).sum();
            let entry = self.next(old, stamp, read, bytes_kept, (dir, name), &mut changed);
            self.files.insert(key.to_owned(), entry);
        }
        changed
    }

    /// The names of the files that the reader keeps.
    pub fn keys(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    /// The full scans and the file reads so far.
    #[cfg(test)]
    pub fn counts(&self) -> (u64, u64) {
        (self.scans, self.reads)
    }

    /// The entry of a file with `stamp`, from its `old` entry. A kept file
    /// also counts against the budget: one that no longer fits is dropped,
    /// and one that fits again is read.
    fn next(
        &mut self,
        old: Option<Entry>,
        stamp: Stamp,
        read: bool,
        bytes_kept: u64,
        (dir, name): (&OwnedFd, &str),
        changed: &mut bool,
    ) -> Entry {
        let fits = bytes_kept + stamp.size <= READ_BYTES;
        match old {
            Some(old) if old.stamp == stamp && old.bytes.is_some() && fits => old,
            Some(old) if old.stamp == stamp && old.bytes.is_none() && (old.tried || !(read && fits)) => old,
            _ => {
                *changed = true;
                let tried = read && fits;
                let bytes = tried.then(|| {
                    self.reads += 1;
                    read_at(dir, name)
                });
                // A file can grow between the stamp and the read.
                let bytes = bytes.flatten().filter(|b| bytes_kept + b.len() as u64 <= READ_BYTES);
                Entry { stamp, bytes, tried }
            }
        }
    }

    pub fn room(&self) -> Room {
        let files = self
            .files
            .iter()
            .map(|(name, entry)| (name.as_str(), (entry.stamp.modified.0, entry.bytes.as_deref())))
            .collect();
        project(&files)
    }
}

/// Reads a round once.
pub fn read_room(dir: &Path) -> Room {
    let mut reader = RoundReader::new(dir.to_owned());
    reader.refresh();
    reader.room()
}

impl Default for Room {
    fn default() -> Self {
        project(&BTreeMap::new())
    }
}

/// The text of `diff.txt` when the working dir is not a git work tree.
pub const NO_GIT_DIFF: &str = "The working dir is not a git work tree, so the app cannot show its change.";

/// The diff that the run saved after its last write stage.
pub fn read_diff(dir: &Path) -> Option<String> {
    let fd = nofollow_dir(dir)?;
    let checkpoint = read_at(&fd, "checkpoint.json").and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let name = match checkpoint.as_ref() {
        Some(cp) if cp["artifacts"]["revision-diff.txt"].is_string() => "revision-diff.txt",
        Some(cp) if cp["artifacts"]["implementation-diff.txt"].is_string() => "implementation-diff.txt",
        Some(_) => return None,
        None => "diff.txt",
    };
    let bytes = read_at(&fd, name)?;
    if let Some(cp) = &checkpoint
        && cp["artifacts"][name] != crate::task_recovery::digest(&bytes)
    {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// True if `dir` looks like a round folder.
pub fn is_round(dir: &Path) -> bool {
    let Some(fd) = nofollow_dir(dir) else {
        return false;
    };
    stamp_at(&fd, "events.jsonl").is_some() || nofollow_dir(&dir.join(TURNS_DIR)).is_some()
}

// --- projection ------------------------------------------------------------

struct Event {
    line: usize,
    at: String,
    kind: String,
    detail: String,
    /// The run that this event came from, for an event that a continuation inherited.
    inherited: Option<String>,
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().map(|items| items.iter().map(text).filter(|s| !s.trim().is_empty()).collect()).unwrap_or_default()
}

/// The first sentence of `text`, cut to `SUMMARY_CHARS`.
fn summary(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = flat.find(". ").map_or(flat.len(), |i| i + 1);
    let first = &flat[..end];
    if first.chars().count() > SUMMARY_CHARS {
        format!("{}…", first.chars().take(SUMMARY_CHARS).collect::<String>())
    } else {
        first.to_owned()
    }
}

pub(crate) fn stage_label(id: &str) -> String {
    match id.strip_prefix("exchange-") {
        Some(turn) => format!("Exchange {turn}"),
        None => match id {
            "crossreview" => "Cross review".to_owned(),
            "revision-gates" => "Revision gates".to_owned(),
            "final" => "Final review".to_owned(),
            "independent-review" => "Independent review".to_owned(),
            "settle" => "Result".to_owned(),
            other => {
                let mut label = other.to_owned();
                label[..1].make_ascii_uppercase();
                label
            }
        },
    }
}

/// The stage id of a `stage` event detail, such as `exchange turn 2` or
/// `crossreview by sol`.
pub(crate) fn stage_of(detail: &str) -> Option<String> {
    if let Some(turn) = detail.strip_prefix("exchange turn ") {
        return turn.trim().parse::<u32>().ok().map(|n| format!("exchange-{n}"));
    }
    let word = detail.split_whitespace().next()?;
    Some(
        match word {
            "final" => "final",
            other if STAGES.contains(&other) => other,
            _ => return None,
        }
        .to_owned(),
    )
}

fn stage_index(id: &str) -> usize {
    let base = if id.starts_with("exchange-") { "exchange" } else { id };
    STAGES.iter().position(|s| *s == base).unwrap_or(STAGES.len())
}

fn turn_of(id: &str) -> u32 {
    id.strip_prefix("exchange-").and_then(|t| t.parse().ok()).unwrap_or(0)
}

/// The sort key of a stage: its place and, for an exchange, its turn.
pub(crate) fn stage_key(id: &str) -> (usize, u32) {
    (stage_index(id), turn_of(id))
}

/// `files` maps relative names to the mtime in seconds and the bytes, if read.
fn project(files: &BTreeMap<&str, (i64, Option<&[u8]>)>) -> Room {
    let checkpoint =
        files.get("checkpoint.json").and_then(|(_, bytes)| serde_json::from_slice::<Value>(bytes.as_ref()?).ok());
    let cleanup = checkpoint.as_ref().is_some_and(|c| c["cleaning"] == true);
    let visible = |name: &str| {
        !cleanup
            || !(name == "gates.json"
                || name == "revision-gates.json"
                || name == "crossreview.json"
                || name == "independent-review.json"
                || name.starts_with("final-"))
            || checkpoint.as_ref().is_some_and(|c| c["artifacts"][name].is_string())
    };
    let json = |name: &str| -> Option<Value> {
        if !visible(name) {
            return None;
        }
        let value: Value = serde_json::from_slice(files.get(name)?.1?).ok()?;
        value.is_object().then_some(value)
    };
    // Each object line with its line number from 1, which stays the same as the file grows.
    let lines = |name: &str| -> Vec<(usize, Value)> {
        let Some((_, Some(bytes))) = files.get(name) else {
            return Vec::new();
        };
        // The last line can be half written.
        bytes
            .split(|&b| b == b'\n')
            .enumerate()
            .take(if cleanup && name == "events.jsonl" {
                checkpoint.as_ref().and_then(|c| c["events"].as_u64()).unwrap_or_default() as usize
            } else {
                usize::MAX
            })
            .filter_map(|(i, line)| Some((i + 1, serde_json::from_slice::<Value>(line).ok()?)))
            .filter(|(_, value)| value.is_object())
            .collect()
    };
    let mtime = |name: &str| visible(name).then(|| files.get(name).map(|(secs, _)| iso(secs * 1000))).flatten();
    let manifest = json("manifest.json");
    let run = json("run.json");
    // The independent reviewer as the run recorded it. It is no participant.
    let independent_reviewer = manifest
        .as_ref()
        .and_then(|m| serde_json::from_value::<RoomParticipant>(m["independent"].clone()).ok())
        .filter(|p| p.id == INDEPENDENT)
        .or_else(|| {
            let agent = serde_json::from_value::<TaskAgent>(run.as_ref()?["independent"].clone()).ok()?;
            Some(RoomParticipant {
                id: INDEPENDENT.to_owned(),
                kind: Some(agent.kind),
                model: agent.model,
                model_recorded: true,
                reported_model: None,
            })
        });

    let roster = roster(manifest.as_ref(), run.as_ref()).unwrap_or_else(|| {
        let mut names: Vec<String> = Vec::new();
        let mut add = |name: &str| {
            if !name.is_empty() && !names.iter().any(|a| a == name) {
                names.push(name.to_owned());
            }
        };
        if let Some(m) = &manifest {
            for name in strings(&m["planned"]).iter().chain(&strings(&m["degraded"])) {
                add(name);
            }
        }
        let mut named: Vec<&str> = files
            .keys()
            .filter_map(|k| k.strip_prefix("turns/plan-"))
            .map(|rest| rest.split('.').next().unwrap_or_default())
            .collect();
        named.sort_unstable();
        for name in named {
            add(name);
        }
        names
            .into_iter()
            .map(|id| RoomParticipant {
                kind: serde_json::from_value(Value::String(id.clone())).ok(),
                id,
                model: None,
                model_recorded: false,
                reported_model: None,
            })
            .collect()
    });
    let agents: Vec<String> = roster.iter().map(|p| p.id.clone()).collect();
    let peer = |me: &str| agents.iter().find(|a| *a != me).cloned().unwrap_or_else(|| "all".to_owned());
    let known = |name: &str| agents.iter().any(|a| a == name).then(|| name.to_owned());

    let events: Vec<Event> = lines("events.jsonl")
        .iter()
        .map(|(line, e)| Event {
            line: *line,
            at: text(&e["at"]),
            kind: text(&e["kind"]),
            detail: text(&e["detail"]),
            inherited: e["inherited"].as_str().map(str::to_owned),
        })
        .collect();
    // The last model that the CLI of each agent reported.
    let reported = |id: &str| {
        events.iter().rev().filter(|e| e.kind == "model").find_map(|e| {
            let (name, model) = e.detail.split_once(' ')?;
            (name == id).then(|| model.to_owned())
        })
    };
    let roster: Vec<RoomParticipant> =
        roster.into_iter().map(|p| RoomParticipant { reported_model: reported(&p.id), ..p }).collect();
    let independent_reviewer = independent_reviewer.map(|p| RoomParticipant { reported_model: reported(&p.id), ..p });
    // A continuation names the run that it continues, and its first stage
    // if the source started that stage before it stopped.
    let repeated = run
        .as_ref()
        .filter(|r| r["continues"]["repeats"] == true)
        .and_then(|r| r["continues"]["from"].as_str().map(str::to_owned));
    let started_in = |inherited: bool| -> BTreeMap<String, String> {
        events
            .iter()
            .filter(|e| e.kind == "stage" && e.inherited.is_some() == inherited)
            .filter_map(|e| Some((stage_of(&e.detail)?, e.inherited.clone().unwrap_or_default())))
            .collect()
    };
    let (inherited, own) = (started_in(true), started_in(false));

    // Stages, from the `stage` events.
    let mut started: Vec<(String, String)> = Vec::new();
    let mut ended = None;
    let mut ended_at = None;
    let mut questions_open = false;
    for event in &events {
        match event.kind.as_str() {
            "stage" => {
                if let Some(id) = stage_of(&event.detail)
                    && !started.iter().any(|(s, _)| *s == id)
                {
                    started.push((id, event.at.clone()));
                }
            }
            "done" | "run_failed" | "cancelled" | "stopped" => {
                ended = Some(match event.kind.as_str() {
                    "run_failed" => "failed",
                    // duo ends on open questions; an in-app run waits for answers.
                    other => other,
                });
                ended_at = Some(event.at.clone());
            }
            "waiting" => questions_open = true,
            "answered" => questions_open = false,
            _ => {}
        }
    }
    // Only the turns that a file or an event names: a large number in a
    // round folder must not expand into a range.
    let mut turns_seen: BTreeSet<u32> = started.iter().map(|(s, _)| turn_of(s)).filter(|n| *n > 0).collect();
    turns_seen.extend(file_turns(files));
    // A run shows the turns that it recorded; an older or external round
    // shows the default turns.
    let configured = run
        .as_ref()
        .and_then(|r| r["config"]["turns"].as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n <= TURN_LIMIT);
    let mut ids = vec!["plan".to_owned()];
    ids.extend(
        (1..=configured.unwrap_or(TURNS))
            .chain(turns_seen.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|n| format!("exchange-{n}")),
    );
    // Older rounds have no second gate stage. Show it when a round starts it.
    let revision_gates = started.iter().any(|(id, _)| id == "revision-gates")
        || (visible("revision-gates.json") && files.contains_key("revision-gates.json"));
    let independent_stage = independent_reviewer.is_some() || started.iter().any(|(id, _)| id == "independent-review");
    ids.extend(
        STAGES[2..]
            .iter()
            .copied()
            .filter(|id| *id != "revision-gates" || revision_gates)
            .filter(|id| *id != "independent-review" || independent_stage)
            .map(str::to_owned),
    );
    let last = started.iter().map(|(s, _)| s).max_by_key(|s| stage_key(s)).cloned();
    let stages: Vec<Stage> = ids
        .iter()
        .map(|id| {
            let at = started.iter().find(|(s, _)| s == id).map(|(_, at)| at.clone());
            let state = match (&at, &last) {
                (Some(_), Some(last)) if last == id => match ended {
                    Some("done") => "done",
                    Some(_) => "stopped",
                    None => "active",
                },
                (Some(_), _) => "done",
                (None, Some(last)) if stage_key(id) < stage_key(last) => "skipped",
                (None, _) if ended.is_some() => "skipped",
                (None, _) => "pending",
            };
            let label = match inherited.get(id) {
                Some(run) if !own.contains_key(id) => format!("{} (from run {run})", stage_label(id)),
                Some(_) => format!("{} (repeated)", stage_label(id)),
                None if repeated.as_deref() == Some(id) => format!("{} (repeated)", stage_label(id)),
                None => stage_label(id),
            };
            Stage { id: id.clone(), label, state, started: at }
        })
        .collect();
    let active_actor = events
        .iter()
        .rev()
        .find(|e| e.kind == "stage")
        .and_then(|e| e.detail.strip_prefix("crossreview by "))
        .and_then(known);

    // Agents' turns and positions.
    let mut messages = Vec::new();
    let mut message = |id: String,
                       from: &str,
                       to: String,
                       kind: &str,
                       stage: &str,
                       at: Option<String>,
                       summary_of: &str,
                       body: String| {
        messages.push(Message {
            id,
            from: from.to_owned(),
            to,
            kind: kind.to_owned(),
            stage: stage.to_owned(),
            at,
            summary: summary(summary_of),
            body,
        });
    };
    let mut positions = Vec::new();
    for agent in &agents {
        let name = format!("turns/plan-{agent}.json");
        let Some(plan) = json(&name) else {
            continue;
        };
        let body = format!(
            "**Plan.** {}\n\n**Claims implementer:** {} — {}\n\n**Touches:** {}\n\n**Risks:** {}",
            text(&plan["plan"]),
            text(&plan["claim"]),
            text(&plan["claim_reason"]),
            strings(&plan["touches"]).join(", "),
            strings(&plan["risks"]).join("; "),
        );
        message(name.clone(), agent, "all".to_owned(), "plan", "plan", mtime(&name), &text(&plan["plan"]), body);
        positions.push(Position {
            agent: agent.clone(),
            plan: text(&plan["plan"]),
            implementer: known(&text(&plan["claim"])),
            source: "plan".to_owned(),
        });
    }
    let mut turns = Vec::new();
    let mut concessions = Vec::new();
    for n in turns_seen {
        let mut replies = Vec::new();
        for agent in &agents {
            let name = format!("turns/t{n}-{agent}.json");
            let Some(reply) = json(&name) else {
                continue;
            };
            let conceded = strings(&reply["conceded"]);
            let agreed = reply["agreed"].as_bool().unwrap_or(false);
            let implementer = known(&text(&reply["implementer"]));
            let body = format!(
                "{}\n\n**Implementer:** {} · **Agreed:** {agreed} · **Conceded:** {}",
                text(&reply["message"]),
                implementer.as_deref().unwrap_or("?"),
                if conceded.is_empty() { "nothing".to_owned() } else { conceded.join("; ") },
            );
            let stage = format!("exchange-{n}");
            message(name.clone(), agent, peer(agent), "reply", &stage, mtime(&name), &text(&reply["message"]), body);
            let plan = text(&reply["plan"]);
            if let Some(position) = positions.iter_mut().find(|p| p.agent == *agent) {
                if !plan.is_empty() {
                    position.plan = plan;
                }
                position.implementer = implementer.clone().or(position.implementer.take());
                position.source = format!("turn {n}");
            }
            concessions.extend(conceded.iter().map(|point| Concession {
                turn: n,
                agent: agent.clone(),
                point: point.clone(),
            }));
            replies.push(Reply { agent: agent.clone(), agreed, implementer, conceded });
        }
        let converged = (replies.len() == agents.len() && agents.len() == 2 && replies.iter().all(|r| r.agreed))
            .then(|| replies[0].implementer.clone())
            .flatten()
            .filter(|pick| replies.iter().all(|r| r.implementer.as_ref() == Some(pick)));
        turns.push(Turn { turn: n, replies, converged });
    }

    let tiebreak =
        json("tiebreak.json").map(|t| Tiebreak { implementer: known(&text(&t["implementer"])), why: text(&t["why"]) });
    if let Some(t) = &tiebreak {
        // duo's tiebreak is by its first agent; an in-app run names it.
        let by = events.iter().find_map(|e| e.detail.strip_prefix("tiebreak by ")).and_then(known);
        let from = by.or_else(|| agents.first().cloned()).unwrap_or_else(|| "tiebreak".to_owned());
        let body = format!("**Implementer:** {}\n\n{}", t.implementer.as_deref().unwrap_or("?"), t.why);
        let id = "tiebreak.json".to_owned();
        message(id, &from, "all".to_owned(), "tiebreak", "tiebreak", mtime("tiebreak.json"), &t.why, body);
    }

    // The implementer, in the order that the round decides it.
    let decision = events.iter().rev().find_map(|e| {
        let name = match e.kind.as_str() {
            "decision" => e.detail.strip_suffix(" implements")?,
            "walkover" => e.detail.split_whitespace().next()?,
            _ => return None,
        };
        known(name).map(|n| (n, e.detail.contains("user")))
    });
    let walkover =
        events.iter().find(|e| e.kind == "walkover").and_then(|e| known(e.detail.split_whitespace().next()?));
    let converged_turn = turns.iter().find(|t| t.converged.is_some());
    let user_pick = events
        .iter()
        .rev()
        .find(|e| e.kind == "decision" && e.detail.starts_with("user chose "))
        .and_then(|e| known(e.detail.strip_prefix("user chose ")?));
    let implementer = manifest
        .as_ref()
        .and_then(|m| known(&text(&m["implementer"])))
        .or(user_pick.clone())
        .or(decision.map(|(n, _)| n))
        .or_else(|| tiebreak.as_ref().and_then(|t| t.implementer.clone()))
        .or_else(|| converged_turn.and_then(|t| t.converged.clone()));
    let decided_by = implementer.as_ref().map(|_| {
        if user_pick.is_some() {
            "user"
        } else if walkover.is_some() {
            "walkover"
        } else if tiebreak.is_some() {
            "tiebreak"
        } else {
            "agreement"
        }
        .to_owned()
    });

    // Where the agents differed, and how it ended.
    let mut disagreements = Vec::new();
    for turn in turns.iter().filter(|t| t.converged.is_none()) {
        let resolved = turns
            .iter()
            .find(|t| t.turn > turn.turn && t.converged.is_some())
            .map(|t| format!("turn {}", t.turn))
            .or_else(|| user_pick.as_ref().map(|_| "user".to_owned()))
            .or_else(|| tiebreak.as_ref().map(|_| "tiebreak".to_owned()));
        let picks: Vec<String> = turn
            .replies
            .iter()
            .map(|r| format!("{} wants {}", r.agent, r.implementer.as_deref().unwrap_or("?")))
            .collect();
        let differ = turn.replies.windows(2).any(|w| w[0].implementer != w[1].implementer);
        if differ {
            disagreements.push(Disagreement {
                turn: turn.turn,
                about: "implementer",
                detail: picks.join(", "),
                resolved: resolved.clone(),
            });
        }
        for reply in turn.replies.iter().filter(|r| !r.agreed) {
            disagreements.push(Disagreement {
                turn: turn.turn,
                about: "agreement",
                detail: format!("{} does not accept the plan yet", reply.agent),
                resolved: resolved.clone(),
            });
        }
        let silent: Vec<&String> = agents.iter().filter(|a| !turn.replies.iter().any(|r| r.agent == **a)).collect();
        for agent in silent {
            disagreements.push(Disagreement {
                turn: turn.turn,
                about: "agreement",
                detail: format!("{agent} did not reply"),
                resolved: resolved.clone(),
            });
        }
    }

    if let Some(i) = json("implementation.json") {
        let from = implementer.clone().unwrap_or_else(|| "implementer".to_owned());
        let mut body = text(&i["summary"]);
        for (title, key) in [
            ("Deviations", "deviations"),
            ("Left undone", "left_undone"),
            ("Missing dependencies", "missing_dependencies"),
        ] {
            let items = strings(&i[key]);
            if !items.is_empty() {
                body.push_str(&format!("\n\n**{title}:**\n{}", bullets(&items)));
            }
        }
        message(
            "implementation.json".to_owned(),
            &from,
            peer(&from),
            "implementation",
            "implement",
            mtime("implementation.json"),
            &text(&i["summary"]),
            body,
        );
    }

    let mut gates = Vec::new();
    for (stage, file) in [("gates", "gates.json"), ("revision-gates", "revision-gates.json")] {
        if !visible(file) || (stage == "revision-gates" && !revision_gates) {
            continue;
        }
        gates = files
            .get(file)
            .and_then(|(_, b)| serde_json::from_slice::<Vec<Value>>((*b)?).ok())
            .unwrap_or_default()
            .iter()
            .map(|g| Gate { command: text(&g["command"]), code: g["code"].as_i64(), output: text(&g["output"]) })
            .collect();
        if !gates.is_empty() {
            let passed = gates.iter().filter(|g| g.code == Some(0)).count();
            let line = format!("{passed} of {} gates passed", gates.len());
            let body = gates
                .iter()
                .map(|g| {
                    format!("- `{}` → {}", g.command, g.code.map_or("no exit".to_owned(), |c| format!("exit {c}")))
                })
                .collect::<Vec<_>>()
                .join("\n");
            let id = file.to_owned();
            message(id, "run", "all".to_owned(), "gates", stage, mtime(file), &line, format!("{line}.\n\n{body}"));
        }
    }

    let reviewer = manifest
        .as_ref()
        .and_then(|m| known(&text(&m["reviewer"])))
        .or_else(|| implementer.as_ref().map(|i| peer(i)).filter(|p| p != "all"));
    let crossreview = json("crossreview.json").map(|c| Review {
        reviewer: reviewer.clone(),
        verdict: text(&c["verdict"]),
        findings: c["findings"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|f| Finding {
                severity: text(&f["severity"]),
                file: text(&f["file"]),
                line: f["line"].as_i64(),
                what: text(&f["what"]),
                why: text(&f["why"]),
                fix: text(&f["fix"]),
            })
            .collect(),
        message: text(&c["message"]),
    });
    if let Some(review) = &crossreview {
        let from = review.reviewer.clone().unwrap_or_else(|| "reviewer".to_owned());
        let mut body = format!("**Verdict:** {}\n\n{}", review.verdict, review.message);
        for f in &review.findings {
            let line = f.line.map(|l| format!(":{l}")).unwrap_or_default();
            body.push_str(&format!("\n- [{}] `{}{line}` — {}", f.severity, f.file, f.what));
        }
        let to = implementer.clone().unwrap_or_else(|| "all".to_owned());
        let id = "crossreview.json".to_owned();
        message(id, &from, to, "review", "crossreview", mtime("crossreview.json"), &review.message, body);
    }
    if let Some(r) = json("revision.json") {
        let from = implementer.clone().unwrap_or_else(|| "implementer".to_owned());
        let mut body = text(&r["message"]);
        for f in r["fixed"].as_array().map(Vec::as_slice).unwrap_or_default() {
            body.push_str(&format!("\n- fixed: {} → {}", text(&f["finding"]), text(&f["what_changed"])));
        }
        for f in r["rebuttals"].as_array().map(Vec::as_slice).unwrap_or_default() {
            body.push_str(&format!("\n- declined: {} → {}", text(&f["finding"]), text(&f["why_not"])));
        }
        let to = reviewer.clone().unwrap_or_else(|| "all".to_owned());
        let id = "revision.json".to_owned();
        message(id, &from, to, "revision", "revise", mtime("revision.json"), &text(&r["message"]), body);
    }

    let blocking_of = |value: &Value| -> Vec<Blocking> {
        value
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|b| Blocking { what: text(&b["what"]), why: text(&b["why"]), at: text(&b["where"]) })
            .collect()
    };
    let mut finals = Vec::new();
    for agent in &agents {
        let name = format!("final-{agent}.json");
        let Some(f) = json(&name) else {
            continue;
        };
        let review = Final {
            agent: agent.clone(),
            ship: f["ship"].as_bool().unwrap_or(false),
            blocking: blocking_of(&f["blocking"]),
            residual: strings(&f["residual"]),
            task_met: text(&f["task_met"]),
        };
        let mut body = format!("**Ship:** {}\n\n{}", review.ship, review.task_met);
        for b in &review.blocking {
            body.push_str(&format!("\n- blocking: {} ({})", b.what, b.at));
        }
        message(name.clone(), agent, "user".to_owned(), "final", "final", mtime(&name), &review.task_met, body);
        finals.push(review);
    }
    let independent = independent_stage.then(|| {
        let review = json("independent-review.json").map(|f| Final {
            agent: INDEPENDENT.to_owned(),
            ship: f["ship"].as_bool().unwrap_or(false),
            blocking: blocking_of(&f["blocking"]),
            residual: strings(&f["residual"]),
            task_met: text(&f["task_met"]),
        });
        if let Some(review) = &review {
            let mut body = format!("**Ship:** {}\n\n{}", review.ship, review.task_met);
            for b in &review.blocking {
                body.push_str(&format!("\n- blocking: {} ({})", b.what, b.at));
            }
            let name = "independent-review.json";
            let stage = "independent-review";
            message(
                name.to_owned(),
                INDEPENDENT,
                "user".to_owned(),
                "independent",
                stage,
                mtime(name),
                &review.task_met,
                body,
            );
        }
        IndependentReview {
            reviewer: independent_reviewer.clone().unwrap_or(RoomParticipant {
                id: INDEPENDENT.to_owned(),
                kind: None,
                model: None,
                model_recorded: false,
                reported_model: None,
            }),
            state: stages.iter().find(|s| s.id == "independent-review").map_or("pending", |s| s.state),
            review,
            verdict: manifest.as_ref().and_then(|m| m["independent_verdict"].as_str()).map(str::to_owned),
        }
    });
    let outcome = manifest.as_ref().map(|m| Outcome {
        ship: m["ship"].as_bool().unwrap_or(false),
        ship_basis: text(&m["ship_basis"]),
        blocking: blocking_of(&m["blocking"]),
        backlog: strings(&m["backlog"]),
        stopped: m["stopped"].as_str().map(str::to_owned),
        base: m["base"].as_str().map(str::to_owned),
    });

    for (line, entry) in lines("messages.jsonl") {
        // The sender of a contribution is fixed: the line cannot name the user or an agent.
        let (from, kind) = match entry["kind"].as_str() {
            Some("answer") => ("user", "answer"),
            Some("external") => (EXTERNAL, "external"),
            _ => ("user", "user"),
        };
        let body = text(&entry["body"]);
        let stage = text(&entry["stage"]);
        let id = format!("messages.jsonl#{line}");
        message(id, from, "all".to_owned(), kind, &stage, Some(text(&entry["at"])), &body, body.clone());
    }

    // Notable events, at the stage that was active then.
    let mut degraded = Vec::new();
    let mut violations = Vec::new();
    let mut current = "plan".to_owned();
    let name_in = |detail: &str| detail.split_whitespace().next().and_then(known);
    let manifest_degraded = manifest.as_ref().map(|m| strings(&m["degraded"])).unwrap_or_default();
    for name in &manifest_degraded {
        degraded.push(Notice {
            agent: Some(name.clone()),
            stage: "plan".to_owned(),
            detail: format!("{name} did not make a plan"),
        });
    }
    for event in &events {
        if event.kind == "stage" {
            if let Some(id) = stage_of(&event.detail) {
                current = id;
            }
            continue;
        }
        let notice = || Notice { agent: name_in(&event.detail), stage: current.clone(), detail: event.detail.clone() };
        match event.kind.as_str() {
            "agent_degraded" | "agent_stalled" | "agent_timeout" => {
                let notice = notice();
                let planned = current == "plan" && notice.agent.as_ref().is_some_and(|a| manifest_degraded.contains(a));
                if !planned {
                    degraded.push(notice);
                }
            }
            "violation" => violations.push(notice()),
            _ => {}
        }
        let shown = matches!(
            event.kind.as_str(),
            "agent_degraded"
                | "agent_stalled"
                | "agent_timeout"
                | "walkover"
                | "violation"
                | "no_detection"
                | "retry"
                | "run_failed"
                | "cancelled"
                | "stopped"
                | "waiting"
                | "paused"
                | "resumed"
                | "mirror_failed"
                | "decision"
                | "shared_vm"
                | "old_copies"
        );
        if shown {
            let body = format!("**{}** — {}", event.kind.replace('_', " "), event.detail);
            let id = format!("events.jsonl#{}", event.line);
            message(id, "run", "all".to_owned(), "event", &current, Some(event.at.clone()), &event.detail, body);
        }
    }

    let questions = match files.get("open-questions.txt") {
        Some((_, Some(bytes))) if questions_open || ended == Some("stopped") => {
            String::from_utf8_lossy(bytes).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
        }
        _ => Vec::new(),
    };

    messages.sort_by_key(|m| (stage_key(&m.stage), m.at.clone()));
    Room {
        agents: agents.clone(),
        participants: roster,
        stages,
        active_actor,
        messages,
        positions,
        turns,
        disagreements,
        concessions,
        implementer,
        decided_by,
        tiebreak,
        crossreview,
        finals,
        independent,
        outcome,
        degraded,
        violations,
        gates,
        questions,
        ended: ended.map(str::to_owned),
        ended_at,
    }
}

/// The participants that a run saved: the roster of its manifest, else the
/// agents of its `run.json`. A legacy round has neither.
fn roster(manifest: Option<&Value>, run: Option<&Value>) -> Option<Vec<RoomParticipant>> {
    let saved = manifest
        .and_then(|m| serde_json::from_value::<[RoomParticipant; 2]>(m["participants"].clone()).ok())
        .filter(|[a, b]| a.id != b.id);
    let recorded = || {
        let agents = serde_json::from_value::<[TaskAgent; 2]>(run?["agents"].clone()).ok()?;
        Some(participants(&agents).map(|p| RoomParticipant {
            id: p.id,
            kind: Some(p.kind),
            model: p.model,
            model_recorded: true,
            reported_model: None,
        }))
    };
    saved.or_else(recorded).map(Vec::from)
}

/// The highest `t<n>` of the turn files.
fn file_turns<'a>(files: &'a BTreeMap<&str, (i64, Option<&[u8]>)>) -> impl Iterator<Item = u32> + 'a {
    files.keys().filter_map(|k| k.strip_prefix("turns/t")?.split('-').next()?.parse::<u32>().ok()).filter(|n| *n > 0)
}

fn bullets(items: &[String]) -> String {
    items.iter().map(|i| format!("- {i}")).collect::<Vec<_>>().join("\n")
}

// --- writing ---------------------------------------------------------------

/// Opens `root/<parts>`, makes each missing folder, and follows no link on
/// the way. `root` must be a trusted host folder.
fn open_parts(root: &Path, parts: &[String]) -> std::io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut dir = rustix::fs::open(root, flags, Mode::empty())?;
    for part in parts {
        if part.is_empty() || part == "." || part == ".." || part.contains('/') {
            return Err(std::io::Error::other(format!("bad folder name {part}")));
        }
        match rustix::fs::mkdirat(&dir, part.as_str(), Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        dir = rustix::fs::openat(&dir, part.as_str(), flags, Mode::empty())?;
    }
    Ok(dir)
}

fn too_large(size: u64) -> std::io::Error {
    std::io::Error::other(format!("{size} bytes is more than the limit of {FILE_LIMIT}"))
}

/// The start of `bytes` within `FILE_LIMIT`, with a line that says what is omitted.
fn within_limit(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if bytes.len() as u64 <= FILE_LIMIT {
        return bytes.into();
    }
    let keep = FILE_LIMIT as usize - 128;
    let mut out = bytes[..keep].to_vec();
    out.extend_from_slice(
        format!("\n\n[The next {} bytes are omitted: the file is over the limit.]\n", bytes.len() - keep).as_bytes(),
    );
    out.into()
}

/// Writes a regular file in `dir` without following a link at its name.
fn write_at(dir: &OwnedFd, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(dir, name, flags, Mode::from_raw_mode(0o644))?;
    let mut file = std::fs::File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(format!("{name} is not a regular file")));
    }
    file.set_len(0)?;
    file.write_all(bytes)
}

const DIR_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::NOFOLLOW).union(OFlags::DIRECTORY).union(OFlags::CLOEXEC);

/// The folder of `name` below `dir` and its file name. Only one folder
/// level is allowed (`turns/`), and no link is followed.
fn open_parent(dir: &Path, name: &str) -> rustix::io::Result<(OwnedFd, String)> {
    let mut fd = rustix::fs::open(dir, DIR_FLAGS, Mode::empty())?;
    let file = match name.split_once('/') {
        Some((sub, file)) => {
            if sub != TURNS_DIR {
                return Err(rustix::io::Errno::INVAL);
            }
            fd = rustix::fs::openat(&fd, sub, DIR_FLAGS, Mode::empty())?;
            file
        }
        None => name,
    };
    if file.is_empty() || file.contains('/') || file == "." || file == ".." {
        return Err(rustix::io::Errno::INVAL);
    }
    Ok((fd, file.to_owned()))
}

/// Writes the file at once and makes it durable: the temp file is synced,
/// renamed, then the folder is synced. Data over `FILE_LIMIT` is an error:
/// this write never keeps only a start.
fn write_durable_at(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.len() as u64 > FILE_LIMIT {
        return Err(too_large(bytes.len() as u64));
    }
    let (fd, file) = open_parent(dir, name)?;
    let tmp = format!("{file}.tmp~");
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let mut out = std::fs::File::from(rustix::fs::openat(&fd, tmp.as_str(), flags, Mode::from_raw_mode(0o644))?);
    if !out.metadata()?.is_file() {
        return Err(std::io::Error::other(format!("{tmp} is not a regular file")));
    }
    out.write_all(bytes)?;
    out.sync_all()?;
    rustix::fs::renameat(&fd, tmp.as_str(), &fd, file.as_str())?;
    rustix::fs::fsync(&fd)?;
    Ok(())
}

/// Adds a line to the file and makes it durable before it returns. A file
/// over `FILE_LIMIT` gets no line.
pub(crate) fn append_durable(dir: &Path, name: &str, value: &Value) -> std::io::Result<()> {
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    let (fd, file) = open_parent(dir, name)?;
    let created =
        matches!(rustix::fs::statat(&fd, file.as_str(), AtFlags::SYMLINK_NOFOLLOW), Err(rustix::io::Errno::NOENT));
    let flags =
        OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let mut out = std::fs::File::from(rustix::fs::openat(&fd, file.as_str(), flags, Mode::from_raw_mode(0o644))?);
    let metadata = out.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other(format!("{name} is not a regular file")));
    }
    let size = metadata.len() + line.len() as u64;
    if size > FILE_LIMIT {
        return Err(too_large(size));
    }
    out.write_all(&line)?;
    out.sync_data()?;
    if created {
        rustix::fs::fsync(&fd)?;
    }
    Ok(())
}

/// A record of a round folder, such as `run.json` or `turns/plan-opus.json`:
/// `None` if it does not exist. A link, a file that is not regular, or a
/// file over `FILE_LIMIT` is an error.
pub(crate) fn read_record(dir: &Path, name: &str) -> std::result::Result<Option<Vec<u8>>, String> {
    let (fd, file) = match open_parent(dir, name) {
        Ok(found) => found,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(format!("cannot open {name}: {e}")),
    };
    let stat = match rustix::fs::statat(&fd, file.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(format!("cannot read {name}: {e}")),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(format!("{name} is not a regular file"));
    }
    if stat.st_size as u64 > FILE_LIMIT {
        return Err(format!("{name} is larger than {} MiB", FILE_LIMIT >> 20));
    }
    read_at(&fd, &file).map(Some).ok_or_else(|| format!("cannot read {name}"))
}

/// The harness copy of a round: a trusted host root and the folders below
/// it, which the guest can change.
#[derive(Debug, Clone)]
pub struct Mirror {
    pub root: PathBuf,
    pub parts: Vec<String>,
}

impl Mirror {
    fn display(&self) -> String {
        self.parts.iter().fold(self.root.clone(), |p, part| p.join(part)).display().to_string()
    }

    /// Makes the mirror folder and writes and removes a probe file, so a
    /// run fails before it starts if it cannot write its artifacts.
    pub fn check(&self) -> Result<()> {
        let probe = open_parts(&self.root, &self.parts).and_then(|dir| {
            write_at(&dir, ".zwinger-probe", b"")?;
            rustix::fs::unlinkat(&dir, ".zwinger-probe", AtFlags::empty())?;
            Ok(())
        });
        probe.map_err(|e| Error::MirrorUnwritable(self.display(), e.to_string()))
    }
}

/// What [`remove_old_copies`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OldCopies {
    pub removed: usize,
    /// The entries that stay, relative to the copy folder.
    pub left: Vec<String>,
}

/// Removes the round copies that earlier builds wrote to a harness folder:
/// `<root>/<parts>` is that copy folder, and `parts` ends with
/// `.zwinger/tasks`. `host` is the
/// host folder of the session's task runs. Only the known layout goes: a
/// regular file at `<slug>/<run>/[turns/]<name>` whose host round has a
/// regular file of that name. Then each folder that is empty. Links are not
/// followed. Each other entry stays and is in `left`.
pub fn remove_old_copies(root: &Path, parts: &[String], host: &Path) -> OldCopies {
    let mut found = OldCopies::default();
    let Some(mut dirs) = nofollow_dir(root).map(|d| vec![d]) else {
        return found;
    };
    for part in parts {
        match rustix::fs::openat(&dirs[dirs.len() - 1], part.as_str(), DIR_FLAGS, Mode::empty()) {
            Ok(dir) => dirs.push(dir),
            Err(_) => return found,
        }
    }
    sweep(&dirs[dirs.len() - 1], host, 0, "", &mut found);
    // `.zwinger/tasks` goes if it is empty, the harness folder stays.
    for (parent, part) in dirs.iter().zip(parts).rev().take(2) {
        if rustix::fs::unlinkat(parent, part.as_str(), AtFlags::REMOVEDIR).is_err() {
            break;
        }
    }
    found
}

/// One folder of [`remove_old_copies`]: `depth` 0 holds task slugs, 1 runs,
/// 2 the round files and `turns`, 3 the turn files.
fn sweep(dir: &OwnedFd, host: &Path, depth: usize, prefix: &str, found: &mut OldCopies) {
    let Ok(entries) = Dir::read_from(dir) else {
        found.left.push(prefix.trim_end_matches('/').to_owned());
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().to_str() else {
            found.left.push(format!("{prefix}{}", entry.file_name().to_string_lossy()));
            continue;
        };
        if name == "." || name == ".." {
            continue;
        }
        let shown = format!("{prefix}{name}");
        let on_host = std::fs::symlink_metadata(host.join(name)).ok().map(|m| m.file_type());
        let kind = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map(|s| FileType::from_raw_mode(s.st_mode));
        match kind {
            Ok(FileType::Directory)
                if (depth < 2 || (depth == 2 && name == TURNS_DIR)) && on_host.is_some_and(|t| t.is_dir()) =>
            {
                match rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty()) {
                    Ok(sub) => sweep(&sub, &host.join(name), depth + 1, &format!("{shown}/"), found),
                    Err(_) => found.left.push(shown),
                }
                // Fails if an entry stays in it.
                let _ = rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR);
            }
            Ok(FileType::RegularFile)
                if depth >= 2
                    && on_host.is_some_and(|t| t.is_file())
                    && rustix::fs::unlinkat(dir, name, AtFlags::empty()).is_ok() =>
            {
                found.removed += 1;
            }
            _ => found.left.push(shown),
        }
    }
}

/// Writes a round: the host copy, then the mirror at each sync.
pub struct RoundWriter {
    dir: PathBuf,
    mirror: Option<Mirror>,
    /// The stamps of the files that the mirror has.
    mirrored: HashMap<String, Stamp>,
    /// The first write that failed. Then the round is not complete.
    failed: Mutex<Option<String>>,
    /// `events.jsonl` is at its limit: only the essential events still go there.
    events_capped: Mutex<bool>,
    /// Why the mirror stopped. Then the harness copy is not complete.
    mirror_error: Option<String>,
}

impl RoundWriter {
    pub fn create(dir: PathBuf, mirror: Option<Mirror>) -> Result<Self> {
        std::fs::create_dir_all(dir.join(TURNS_DIR))?;
        Ok(Self {
            dir,
            mirror,
            mirrored: HashMap::new(),
            failed: Mutex::new(None),
            events_capped: Mutex::new(false),
            mirror_error: None,
        })
    }

    /// The host folder of the round.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes a recovery record or an artifact that it names, durably and
    /// whole. A failure also counts as a failed write of the round.
    pub fn write_durable(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        let written = write_durable_at(&self.dir, name, bytes);
        let error = written.as_ref().err().map(|e| std::io::Error::new(e.kind(), e.to_string()));
        self.record(&self.dir.join(name), written);
        error.map_or(Ok(()), Err)
    }

    pub fn append_durable(&self, name: &str, value: &Value) -> std::io::Result<()> {
        let written = append_durable(&self.dir, name, value);
        let error = written.as_ref().err().map(|e| std::io::Error::new(e.kind(), e.to_string()));
        self.record(&self.dir.join(name), written);
        error.map_or(Ok(()), Err)
    }

    /// Removes a file of the round. A missing file is not an error; another
    /// failure counts as a failed write of the round.
    pub fn remove(&self, name: &str) {
        let path = self.dir.join(name);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            written => self.record(&path, written),
        }
    }

    /// Why the harness copy is not complete, if it is not.
    pub fn mirror_error(&self) -> Option<&str> {
        self.mirror_error.as_deref()
    }

    /// The first write that failed, if any.
    pub fn failure(&self) -> Option<String> {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Keeps the first failed write. The first one also leaves a best-effort
    /// `write-failed` line in the budget, so recovery refuses the round even
    /// if the run stops before it records the failure.
    fn record(&self, path: &Path, written: std::io::Result<()>) {
        let Err(e) = written else { return };
        let why = format!("cannot write {}: {e}", path.display());
        let first = {
            let mut failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
            let first = failed.is_none();
            failed.get_or_insert(why);
            first
        };
        if first {
            let _ = append_durable(&self.dir, crate::task_recovery::BUDGET, &json!({ "kind": "write-failed" }));
        }
    }

    /// Writes the file at once: the room sees a whole file or the old one.
    /// A file over the limit keeps its start.
    pub fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.dir.join(name);
        let tmp = path.with_extension("tmp~");
        let written = std::fs::write(&tmp, within_limit(bytes)).and_then(|()| std::fs::rename(&tmp, &path));
        self.record(&path, written);
    }

    pub fn write_json(&self, name: &str, value: &Value) {
        match serde_json::to_vec_pretty(value) {
            Ok(bytes) => self.write(name, &bytes),
            Err(e) => self.record(&self.dir.join(name), Err(e.into())),
        }
    }

    /// Adds a line to the file. Returns false if the write failed.
    pub fn append(&self, name: &str, value: &Value) -> bool {
        let mut line = value.to_string().into_bytes();
        line.push(b'\n');
        let path = self.dir.join(name);
        let written = std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| {
            let size = f.metadata()?.len() + line.len() as u64;
            if size > FILE_LIMIT {
                return Err(too_large(size));
            }
            f.write_all(&line)
        });
        let ok = written.is_ok();
        self.record(&path, written);
        ok
    }

    /// Adds an event with the start of `detail`. Near its limit,
    /// `events.jsonl` gets one `events_capped` event, then only the
    /// essential events.
    pub fn event(&self, kind: &str, detail: &str) {
        let mut cut = detail.len().min(DETAIL_LIMIT);
        while !detail.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut capped = self.events_capped.lock().unwrap_or_else(PoisonError::into_inner);
        if !*capped {
            let size = std::fs::metadata(self.dir.join("events.jsonl")).map_or(0, |m| m.len());
            if size + (DETAIL_LIMIT as u64) + EVENTS_RESERVE > FILE_LIMIT {
                *capped = true;
                let detail = "events.jsonl is near its size limit: only the stages and the end of the run follow";
                self.append("events.jsonl", &json!({ "at": iso(now_ms()), "kind": "events_capped", "detail": detail }));
            }
        }
        if !*capped || ESSENTIAL.contains(&kind) {
            self.append("events.jsonl", &json!({ "at": iso(now_ms()), "kind": kind, "detail": &detail[..cut] }));
        }
    }

    /// Copies the changed files to the mirror. The first failure is an
    /// event, and the mirror stops: the host copy is the record.
    pub fn sync_mirror(&mut self) {
        let Some(mirror) = &self.mirror else {
            return;
        };
        let result = (|| -> std::io::Result<()> {
            let root = open_parts(&mirror.root, &mirror.parts)?;
            for sub in [None, Some(TURNS_DIR)] {
                let src_path = sub.map_or_else(|| self.dir.clone(), |s| self.dir.join(s));
                let Some(src) = nofollow_dir(&src_path) else {
                    continue;
                };
                let dst = match sub {
                    Some(sub) => open_parts(&mirror.root, &[mirror.parts.clone(), vec![sub.to_owned()]].concat())?,
                    None => root.try_clone()?,
                };
                for entry in Dir::read_from(&src)?.flatten() {
                    let Ok(name) = entry.file_name().to_str() else {
                        continue;
                    };
                    let key = format!("{}{name}", sub.map(|s| format!("{s}/")).unwrap_or_default());
                    let Some(stamp) = stamp_at(&src, name) else {
                        continue;
                    };
                    if name.ends_with("tmp~") || self.mirrored.get(&key) == Some(&stamp) {
                        continue;
                    }
                    if let Some(bytes) = read_at(&src, name) {
                        write_at(&dst, name, &bytes)?;
                        self.mirrored.insert(key, stamp);
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            let detail = format!("{}: {e}", mirror.display());
            self.mirror = None;
            self.event("mirror_failed", &detail);
            self.mirror_error = Some(detail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/reference/duo-round-sample")
    }

    fn temp_dir() -> PathBuf {
        let dir = crate::test_dir::root().join(format!("zwinger-round-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &to.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }

    fn states(room: &Room) -> Vec<(&str, &str)> {
        room.stages.iter().map(|s| (s.id.as_str(), s.state)).collect()
    }

    #[test]
    fn the_room_reads_its_participants_from_the_manifest_then_run_json_then_files() {
        let dir = temp_dir();
        let plan = json!({ "plan": "P.", "claim": "codex" }).to_string();
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        std::fs::write(dir.join("turns/plan-codex.json"), &plan).unwrap();
        std::fs::write(dir.join("turns/plan-claude.json"), &plan).unwrap();
        std::fs::write(dir.join("manifest.json"), json!({ "planned": ["claude", "codex"] }).to_string()).unwrap();
        // A legacy in-app round: names from the manifest, kinds from the names, no models.
        let room = read_room(&dir);
        assert_eq!(room.agents, ["claude", "codex"]);
        let kinds: Vec<_> = room.participants.iter().map(|p| (p.kind, p.model.clone())).collect();
        assert_eq!(kinds, [(Some(AgentKind::Claude), None), (Some(AgentKind::Codex), None)]);
        assert!(room.participants.iter().all(|p| !p.model_recorded));

        // An incomplete run: its run.json gives the order and the models.
        let run = json!({ "agents": [{ "kind": "codex", "model": "o3" }, { "kind": "claude", "model": null }] });
        std::fs::write(dir.join("run.json"), run.to_string()).unwrap();
        let room = read_room(&dir);
        assert_eq!(room.agents, ["codex", "claude"]);
        assert_eq!(room.participants[0].model.as_deref(), Some("o3"));
        assert!(room.participants.iter().all(|p| p.model_recorded));

        // The manifest roster wins.
        let roster = json!([
            { "id": "pi-1", "kind": "pi", "model": "a/b.c" },
            { "id": "pi-2", "kind": "pi", "model": null },
        ]);
        std::fs::write(dir.join("manifest.json"), json!({ "participants": roster }).to_string()).unwrap();
        let room = read_room(&dir);
        assert_eq!(room.agents, ["pi-1", "pi-2"]);
        assert_eq!(room.participants[0].model.as_deref(), Some("a/b.c"));
        assert!(room.participants.iter().all(|p| p.model_recorded));
    }

    #[test]
    fn the_room_shows_the_last_model_that_each_cli_reported() {
        let dir = temp_dir();
        let run = json!({ "agents": [{ "kind": "claude", "model": null }, { "kind": "codex", "model": null }] });
        std::fs::write(dir.join("run.json"), run.to_string()).unwrap();
        let events = [
            json!({ "at": "t", "kind": "model", "detail": "claude claude-opus-5-5" }),
            json!({ "at": "t", "kind": "model", "detail": "claude claude-sonnet-4-5" }),
        ];
        let lines: String = events.iter().map(|e| format!("{e}\n")).collect();
        std::fs::write(dir.join("events.jsonl"), lines).unwrap();
        let room = read_room(&dir);
        let reported: Vec<_> = room.participants.iter().map(|p| p.reported_model.as_deref()).collect();
        assert_eq!(reported, [Some("claude-sonnet-4-5"), None]);
        assert!(!room.messages.iter().any(|m| m.kind == "event"));
    }

    #[test]
    fn reads_the_sample_round() {
        assert!(is_round(&sample()));
        let room = read_room(&sample());
        assert_eq!(room.agents, ["opus", "sol"]);
        assert_eq!(
            room.participants.iter().map(|p| (p.kind, p.model.clone())).collect::<Vec<_>>(),
            [(None, None), (None, None)]
        );
        assert!(room.participants.iter().all(|p| !p.model_recorded));
        assert_eq!(
            states(&room),
            [
                ("plan", "done"),
                ("exchange-1", "done"),
                ("exchange-2", "done"),
                ("tiebreak", "skipped"),
                ("implement", "done"),
                ("gates", "skipped"),
                ("crossreview", "done"),
                ("revise", "done"),
                ("final", "done"),
            ]
        );
        assert_eq!(room.ended.as_deref(), Some("done"));

        // Turn 1: no agreement. Turn 2: both agree on opus.
        assert_eq!(room.turns.len(), 2);
        assert_eq!(room.turns[0].converged, None);
        assert_eq!(room.turns[1].converged.as_deref(), Some("opus"));
        assert!(!room.disagreements.is_empty());
        assert!(room.disagreements.iter().all(|d| d.turn == 1 && d.resolved.as_deref() == Some("turn 2")));
        assert!(room.concessions.iter().any(|c| c.turn == 1 && c.agent == "opus"));
        assert_eq!(room.implementer.as_deref(), Some("opus"));
        assert_eq!(room.decided_by.as_deref(), Some("agreement"));

        // Both opening plans, then the replies, in stage order.
        let kinds: Vec<(&str, &str)> =
            room.messages.iter().filter(|m| m.kind != "event").map(|m| (m.from.as_str(), m.kind.as_str())).collect();
        assert_eq!(
            kinds,
            [
                ("opus", "plan"),
                ("sol", "plan"),
                ("opus", "reply"),
                ("sol", "reply"),
                ("opus", "reply"),
                ("sol", "reply"),
                ("opus", "implementation"),
                ("sol", "review"),
                ("opus", "revision"),
                ("opus", "final"),
                ("sol", "final"),
            ]
        );
        assert_eq!(room.messages.iter().find(|m| m.kind == "review").unwrap().to, "opus");
        assert_eq!(room.positions.iter().map(|p| p.source.as_str()).collect::<Vec<_>>(), ["turn 2", "turn 2"]);

        let review = room.crossreview.as_ref().unwrap();
        assert_eq!((review.reviewer.as_deref(), review.verdict.as_str()), (Some("sol"), "revise"));
        assert_eq!(review.findings[0].severity, "blocking");
        let ships: Vec<(&str, bool)> = room.finals.iter().map(|f| (f.agent.as_str(), f.ship)).collect();
        assert_eq!(ships, [("opus", true), ("sol", false)]);
        let outcome = room.outcome.as_ref().unwrap();
        assert!(!outcome.ship);
        assert_eq!(outcome.ship_basis, "both reviewers");
        assert_eq!(outcome.blocking.len(), 1);
        assert!(room.degraded.is_empty());
    }

    #[test]
    fn a_partial_round_shows_what_it_has() {
        let dir = temp_dir();
        copy(&sample(), &dir);
        std::fs::remove_file(dir.join("manifest.json")).unwrap();
        // A half-written final review and a half-written event line.
        let full = std::fs::read(dir.join("final-sol.json")).unwrap();
        std::fs::write(dir.join("final-sol.json"), &full[..full.len() / 2]).unwrap();
        let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        let cut = events.rfind("{\"at\"").unwrap();
        std::fs::write(dir.join("events.jsonl"), format!("{}{{\"at\":\"2026", &events[..cut])).unwrap();

        let mut reader = RoundReader::new(dir.clone());
        assert!(reader.refresh());
        let room = reader.room();
        assert_eq!(room.ended, None);
        assert_eq!(room.stages.last().unwrap().state, "active");
        assert_eq!(room.finals.len(), 1);
        assert!(room.outcome.is_none());
        assert_eq!(room.implementer.as_deref(), Some("opus"));

        // Nothing changed: no new room. The finished file is read again.
        assert!(!reader.refresh());
        std::fs::write(dir.join("final-sol.json"), &full).unwrap();
        std::fs::write(dir.join("events.jsonl"), &events).unwrap();
        assert!(reader.refresh());
        let room = reader.room();
        assert_eq!(room.finals.len(), 2);
        assert_eq!(room.ended.as_deref(), Some("done"));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_followed() {
        let dir = temp_dir();
        let outside = temp_dir();
        std::fs::write(outside.join("final-x.json"), r#"{"ship":true}"#).unwrap();
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        std::fs::write(dir.join("turns/plan-x.json"), r#"{"plan":"p"}"#).unwrap();
        std::os::unix::fs::symlink(outside.join("final-x.json"), dir.join("final-x.json")).unwrap();
        let room = read_room(&dir);
        assert_eq!(room.agents, ["x"]);
        assert!(room.finals.is_empty());
    }

    #[test]
    fn the_mirror_gets_the_files_and_never_a_link() {
        let host = temp_dir().join("round");
        let root = temp_dir();
        let mirror =
            Mirror { root: root.clone(), parts: [".zwinger", "tasks", "fix", "01"].map(str::to_owned).to_vec() };
        mirror.check().unwrap();
        let mut writer = RoundWriter::create(host.clone(), Some(mirror)).unwrap();
        writer.write_json("turns/plan-a.json", &json!({ "plan": "p" }));
        writer.event("stage", "plan");
        writer.sync_mirror();
        let copied = root.join(".zwinger/tasks/fix/01");
        assert!(copied.join("turns/plan-a.json").is_file());
        assert!(copied.join("events.jsonl").is_file());

        // The guest swaps a file for a link: the mirror stops, the host copy goes on.
        let target = temp_dir().join("victim");
        std::fs::write(&target, "keep").unwrap();
        std::fs::remove_file(copied.join("events.jsonl")).unwrap();
        std::os::unix::fs::symlink(&target, copied.join("events.jsonl")).unwrap();
        writer.event("stage", "exchange turn 1");
        writer.sync_mirror();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        let events = std::fs::read_to_string(host.join("events.jsonl")).unwrap();
        assert!(events.contains("mirror_failed"));
    }

    #[test]
    fn a_write_over_the_limit_keeps_its_start() {
        let dir = temp_dir();
        let writer = RoundWriter::create(dir.clone(), None).unwrap();
        writer.write("big.raw", &vec![b'x'; FILE_LIMIT as usize + 1]);
        assert_eq!(writer.failure(), None);
        let kept = std::fs::read(dir.join("big.raw")).unwrap();
        assert!(kept.len() as u64 <= FILE_LIMIT);
        assert!(String::from_utf8_lossy(&kept).ends_with("bytes are omitted: the file is over the limit.]\n"));
    }

    #[test]
    fn events_near_the_limit_keep_only_the_essential_ones() {
        let dir = temp_dir();
        let writer = RoundWriter::create(dir.clone(), None).unwrap();
        writer.event("stage", &"é".repeat(DETAIL_LIMIT));
        let first: Value =
            serde_json::from_str(std::fs::read_to_string(dir.join("events.jsonl")).unwrap().trim()).unwrap();
        assert!(text(&first["detail"]).len() <= DETAIL_LIMIT);

        std::fs::write(dir.join("events.jsonl"), vec![b'\n'; (FILE_LIMIT - EVENTS_RESERVE) as usize]).unwrap();
        writer.event("retry", "one");
        writer.event("retry", "two");
        writer.event("done", "ship=false");
        assert_eq!(writer.failure(), None);
        let kinds: Vec<String> = std::fs::read_to_string(dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|e| text(&e["kind"]))
            .collect();
        assert_eq!(kinds, ["events_capped", "done"]);
    }

    #[test]
    fn the_reader_keeps_known_files_within_its_limits() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        for name in ["notes.json", "plan.md", "implement.prompt.md", "turns/t1-a.raw", "turns/x.json"] {
            std::fs::write(dir.join(name), "{}").unwrap();
        }
        std::fs::write(dir.join("turns/plan-b.prompt.md"), "p").unwrap();
        for i in 0..100 {
            std::fs::write(dir.join(format!("final-a{i}.json")), r#"{"ship":true}"#).unwrap();
        }
        let mut reader = RoundReader::new(dir.clone());
        reader.refresh();
        assert_eq!(reader.files.len(), READ_FILES);
        assert!(reader.files.keys().all(|k| k.starts_with("final-a") || k == "turns/plan-b.prompt.md"));
        assert!(reader.files.get("turns/plan-b.prompt.md").is_none_or(|e| e.bytes.is_none()));

        // The bytes: five files of 7 MiB, of which four fit.
        let dir = temp_dir();
        for i in 0..5 {
            std::fs::write(dir.join(format!("final-b{i}.json")), vec![b' '; 7 << 20]).unwrap();
        }
        let mut reader = RoundReader::new(dir);
        reader.refresh();
        let read: u64 = reader.files.values().filter_map(|e| e.bytes.as_ref()).map(|b| b.len() as u64).sum();
        assert_eq!(read, 4 * (7 << 20));
        assert!(read <= READ_BYTES);

        // Kept files count against the budget on each refresh. The root is
        // read before `turns/`, so the new file comes first.
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        for i in 1..=4 {
            std::fs::write(dir.join(format!("turns/t{i}-a.json")), vec![b' '; 7 << 20]).unwrap();
        }
        let mut reader = RoundReader::new(dir.clone());
        reader.refresh();
        std::fs::write(dir.join("final-a.json"), vec![b' '; 7 << 20]).unwrap();
        for _ in 0..3 {
            reader.refresh();
            let read: u64 = reader.files.values().filter_map(|e| e.bytes.as_ref()).map(|b| b.len() as u64).sum();
            assert!(read <= READ_BYTES);
        }
    }

    #[test]
    fn a_scan_reads_only_known_files_and_stops_when_cancelled() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        for i in 0..=CANCEL_EVERY {
            std::fs::write(dir.join(format!("notes-{i}.txt")), "x").unwrap();
            std::fs::write(dir.join(format!("turns/notes-{i}.txt")), "x").unwrap();
        }
        let names = ["events.jsonl", "manifest.json", "turns/plan-opus.prompt.md"];
        for name in names {
            std::fs::write(dir.join(name), "{}").unwrap();
        }
        // A prompt is only stat'ed: its content is not read.
        let reads = names.iter().filter(|n| known(n) == Some(true)).count() as u64;
        assert_eq!(reads, 2);
        let mut reader = RoundReader::new(dir);
        assert!(!reader.scan(&AtomicBool::new(true)));
        assert_eq!(reader.counts(), (1, 0), "a cancelled scan reads nothing");
        assert!(reader.files.is_empty());
        assert!(reader.scan(&AtomicBool::new(false)));
        assert_eq!(reader.counts(), (2, reads), "only the known files");
        assert_eq!(reader.keys(), names.map(String::from));
        assert!(!reader.scan(&AtomicBool::new(false)));
        assert_eq!(reader.counts(), (3, reads), "no new content reads");
    }

    fn exchange_ids(room: &Room) -> Vec<&str> {
        room.stages.iter().map(|s| s.id.as_str()).filter(|s| s.starts_with("exchange-")).collect()
    }

    #[test]
    fn a_recorded_run_shows_its_configured_turns_and_an_older_round_the_default() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        assert_eq!(exchange_ids(&read_room(&dir)), ["exchange-1", "exchange-2"]);
        let agents = json!([{ "kind": "claude", "model": null }, { "kind": "codex", "model": null }]);
        let record =
            |turns: u32| json!({ "agents": agents, "config": { "turns": turns, "idleMs": 1000, "hardMs": 1000 } });
        for (turns, expected) in [
            (0, vec![]),
            (1, vec!["exchange-1"]),
            (5, vec!["exchange-1", "exchange-2", "exchange-3", "exchange-4", "exchange-5"]),
            // A damaged count is not expanded: the default turns show.
            (4_000_000, vec!["exchange-1", "exchange-2"]),
        ] {
            std::fs::write(dir.join("run.json"), record(turns).to_string()).unwrap();
            assert_eq!(exchange_ids(&read_room(&dir)), expected, "{turns}");
        }
        // An observed turn shows too.
        std::fs::write(dir.join("run.json"), record(1).to_string()).unwrap();
        std::fs::write(dir.join("turns/t2-claude.json"), r#"{"message":"m"}"#).unwrap();
        assert_eq!(exchange_ids(&read_room(&dir)), ["exchange-1", "exchange-2"]);
        let room = read_room(&dir);
        assert!(room.independent.is_none());
        assert!(!room.stages.iter().any(|s| s.id == "independent-review"));
    }

    #[test]
    fn the_independent_reviewer_shows_from_the_record_or_its_stage() {
        let dir = temp_dir();
        let agents = json!([{ "kind": "claude", "model": "opus" }, { "kind": "claude", "model": null }]);
        let run = json!({ "agents": agents, "independent": { "kind": "claude", "model": "opus" } });
        std::fs::write(dir.join("run.json"), run.to_string()).unwrap();
        let room = read_room(&dir);
        // The same kind and model as a participant, in its own role.
        assert_eq!(room.agents, ["claude-1", "claude-2"]);
        let independent = room.independent.unwrap();
        assert_eq!(independent.reviewer.id, "independent");
        assert_eq!(
            (independent.reviewer.kind, independent.reviewer.model.as_deref()),
            (Some(AgentKind::Claude), Some("opus"))
        );
        assert_eq!((independent.state, independent.review, independent.verdict), ("pending", None, None));
        assert_eq!(room.stages.last().map(|s| (s.id.as_str(), s.state)), Some(("independent-review", "pending")));

        // A round with no record shows the stage once it starts it.
        let dir = temp_dir();
        let events = [
            json!({ "at": "t", "kind": "stage", "detail": "final review" }),
            json!({ "at": "t", "kind": "stage", "detail": "independent-review" }),
        ];
        let lines: String = events.iter().map(|e| format!("{e}\n")).collect();
        std::fs::write(dir.join("events.jsonl"), lines).unwrap();
        std::fs::write(
            dir.join("independent-review.json"),
            json!({ "ship": false, "blocking": [{ "what": "Bug.", "why": "W.", "where": "a.rs" }], "task_met": "No." })
                .to_string(),
        )
        .unwrap();
        let room = read_room(&dir);
        let independent = room.independent.unwrap();
        assert_eq!((independent.reviewer.kind, independent.state), (None, "active"));
        assert_eq!(independent.review.unwrap().blocking[0].what, "Bug.");
        assert_eq!(room.active_actor, None);
    }

    /// A complete round with ten exchange turns and an independent review.
    fn ten_turn_round() -> PathBuf {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        let write = |name: &str, value: Value| std::fs::write(dir.join(name), value.to_string()).unwrap();
        let agents = json!([{ "kind": "claude", "model": null }, { "kind": "codex", "model": null }]);
        let config = json!({ "turns": 10, "idleMs": 1000, "hardMs": 1000 });
        write(
            "run.json",
            json!({ "agents": agents, "config": config, "independent": { "kind": "pi", "model": null } }),
        );
        write("checkpoint.json", json!({ "groups": [] }));
        let mut events = String::new();
        for detail in ["plan"].into_iter().map(str::to_owned).chain((1..=10).map(|n| format!("exchange turn {n}"))) {
            events.push_str(&format!("{}\n", json!({ "at": "t", "kind": "stage", "detail": detail })));
        }
        events.push_str(&format!("{}\n", json!({ "at": "t", "kind": "done", "detail": "ship=false" })));
        std::fs::write(dir.join("events.jsonl"), events).unwrap();
        write(
            "manifest.json",
            json!({ "ship": false, "ship_basis": "withheld — the independent reviewer did not approve", "independent_verdict": "denied" }),
        );
        for name in ["implementation.json", "crossreview.json", "revision.json", "tiebreak.json"] {
            write(name, json!({ "message": "m" }));
        }
        for name in ["gates.json", "revision-gates.json"] {
            write(name, json!([]));
        }
        write("independent-review.json", json!({ "ship": false, "blocking": [], "task_met": "No." }));
        std::fs::write(dir.join("open-questions.txt"), "Q?").unwrap();
        let message = json!({ "at": "t", "stage": "plan", "kind": "user", "body": "Hi." });
        std::fs::write(dir.join("messages.jsonl"), format!("{message}\n")).unwrap();
        for agent in ["claude", "codex"] {
            write(&format!("final-{agent}.json"), json!({ "ship": true, "blocking": [], "task_met": "Yes." }));
            write(&format!("turns/plan-{agent}.json"), json!({ "plan": "P.", "claim": agent }));
            std::fs::write(dir.join(format!("turns/plan-{agent}.prompt.md")), "p").unwrap();
            std::fs::write(dir.join(format!("turns/plan-{agent}.raw")), "r").unwrap();
            for n in 1..=10 {
                write(&format!("turns/t{n}-{agent}.json"), json!({ "message": "m", "implementer": "claude" }));
            }
        }
        dir
    }

    #[test]
    fn a_ten_turn_room_stays_within_the_reader_limits() {
        let dir = ten_turn_round();
        let mut reader = RoundReader::new(dir.clone());
        reader.refresh();
        assert!(reader.files.len() < READ_FILES, "{}", reader.files.len());
        let room = reader.room();
        assert_eq!(room.turns.len(), 10);
        assert!(room.turns.iter().all(|t| t.replies.len() == 2));
        assert_eq!(exchange_ids(&room).len(), 10);
        assert_eq!(room.finals.len(), 2);
        let independent = room.independent.unwrap();
        assert_eq!((independent.reviewer.kind, independent.verdict.as_deref()), (Some(AgentKind::Pi), Some("denied")));
        assert_eq!(room.outcome.unwrap().ship_basis, "withheld — the independent reviewer did not approve");

        // Many agent files cannot push out the fixed files of the round.
        for i in 0..200 {
            std::fs::write(dir.join(format!("final-x{i}.json")), "{}").unwrap();
        }
        let mut reader = RoundReader::new(dir);
        reader.refresh();
        assert_eq!(reader.files.len(), READ_FILES);
        for name in ROOT {
            assert!(reader.files.get(name).is_some_and(|e| e.bytes.is_some()), "{name}");
        }
        let room = reader.room();
        assert_eq!(exchange_ids(&room).len(), 10);
        assert!(room.independent.is_some() && room.outcome.is_some());
    }

    #[test]
    fn a_large_turn_number_does_not_expand_into_a_range() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        std::fs::write(dir.join("turns/t4294967295-a.json"), r#"{"message":"m"}"#).unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            r#"{"at":"t","kind":"stage","detail":"exchange turn 4294967294"}"#.to_owned() + "\n",
        )
        .unwrap();
        let room = read_room(&dir);
        assert!(room.stages.len() < 16);
        assert!(room.turns.len() <= 2);
    }

    #[test]
    fn a_keyed_refresh_reads_only_its_files_and_matches_a_full_read() {
        let dir = temp_dir();
        copy(&sample(), &dir);
        std::fs::write(dir.join("final-big.json"), vec![b' '; FILE_LIMIT as usize + 1]).unwrap();
        let mut reader = RoundReader::new(dir.clone());
        assert!(reader.refresh());
        let (_, reads) = reader.counts();
        // A file that is too large is not read again while its stamp stays.
        assert!(!reader.refresh());
        assert!(!reader.refresh_keys(["final-big.json"]));
        assert_eq!(reader.counts(), (2, reads));

        std::fs::remove_file(dir.join("final-sol.json")).unwrap();
        let events = std::fs::read(dir.join("events.jsonl")).unwrap();
        // The same size, another inode.
        std::fs::write(dir.join("events.staged"), &events).unwrap();
        std::fs::rename(dir.join("events.staged"), dir.join("events.jsonl")).unwrap();
        assert!(reader.refresh_keys(["final-sol.json", "events.jsonl", "notes.txt"]));
        assert_eq!(reader.counts(), (2, reads + 1));
        assert_eq!(reader.room(), read_room(&dir));
    }

    #[test]
    fn known_names() {
        for name in ["events.jsonl", "messages.jsonl", "final-opus.json", "turns/plan-sol.json", "turns/t12-opus.json"]
        {
            assert_eq!(known(name), Some(true), "{name}");
        }
        assert_eq!(known("turns/plan-sol.prompt.md"), Some(false));
        for name in ["final-.json", "plan.md", "turns/t-x.json", "turns/tx-a.json", "turns/t1-a.raw", "x.json"] {
            assert_eq!(known(name), None, "{name}");
        }
    }

    #[test]
    fn an_unwritable_mirror_fails_the_check() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir();
        let parts = [".zwinger", "tasks", "fix", "01"].map(str::to_owned).to_vec();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        let mirror = Mirror { root: root.clone(), parts: parts.clone() };
        assert!(matches!(mirror.check(), Err(Error::MirrorUnwritable(..))));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        // A link that the guest put on the path.
        let elsewhere = temp_dir();
        std::os::unix::fs::symlink(&elsewhere, root.join(".zwinger")).unwrap();
        assert!(matches!(Mirror { root, parts }.check(), Err(Error::MirrorUnwritable(..))));
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn old_copies_go_only_in_the_known_layout() {
        let host = temp_dir();
        for file in ["fix/01/events.jsonl", "fix/01/turns/plan-a.json", "fix/02/events.jsonl"] {
            std::fs::create_dir_all(host.join(file).parent().unwrap()).unwrap();
            std::fs::write(host.join(file), "x").unwrap();
        }
        let root = temp_dir();
        let old = root.join("sub/.zwinger/tasks");
        let outside = temp_dir();
        std::fs::write(outside.join("events.jsonl"), "keep").unwrap();
        for file in ["fix/01/events.jsonl", "fix/01/turns/plan-a.json", "fix/01/notes.md", "other/01/events.jsonl"] {
            std::fs::create_dir_all(old.join(file).parent().unwrap()).unwrap();
            std::fs::write(old.join(file), "y").unwrap();
        }
        // A link that the guest put in the layout is not followed.
        std::os::unix::fs::symlink(&outside, old.join("fix/02")).unwrap();
        let parts = ["sub", ".zwinger", "tasks"].map(str::to_owned);
        let found = remove_old_copies(&root, &parts, &host);
        assert_eq!(found.removed, 2);
        let mut left = found.left;
        left.sort();
        assert_eq!(left, ["fix/01/notes.md", "fix/02", "other"]);
        assert!(!old.join("fix/01/turns").exists());
        assert_eq!(std::fs::read_to_string(outside.join("events.jsonl")).unwrap(), "keep");

        // With all known files gone, `.zwinger/tasks` goes too, the harness stays.
        std::fs::remove_dir_all(root.join("sub/.zwinger")).unwrap();
        std::fs::create_dir_all(old.join("fix/02")).unwrap();
        std::fs::write(old.join("fix/02/events.jsonl"), "y").unwrap();
        assert_eq!(remove_old_copies(&root, &parts, &host), OldCopies { removed: 1, left: Vec::new() });
        assert!(!root.join("sub/.zwinger").exists() && root.join("sub").is_dir());
    }
}
