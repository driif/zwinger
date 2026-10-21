//! The files of a round, in the duo shape: `events.jsonl`, `turns/plan-*.json`,
//! `turns/t<n>-*.json`, `tiebreak.json`, `implementation.json`, `gates.json`,
//! `crossreview.json`, `revision.json`, `final-*.json`, `manifest.json`.
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
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use serde::Serialize;
use serde_json::{Value, json};

use crate::agent::cli::{iso, now_ms};
use crate::error::{Error, Result};

/// A larger file of a round is not read.
const FILE_LIMIT: u64 = 8 << 20;
const SUMMARY_CHARS: usize = 200;
/// The poll of a round folder: fast after a change, slower while nothing changes.
const POLL: Duration = Duration::from_millis(500);
const POLL_SLOW: Duration = Duration::from_millis(1500);
/// What a reader keeps of a round folder.
const READ_FILES: usize = 64;
const READ_BYTES: u64 = 32 << 20;
/// An event detail keeps its start within this size.
const DETAIL_LIMIT: usize = 4 << 10;
/// The end of `events.jsonl` that only the events of `ESSENTIAL` can use.
const EVENTS_RESERVE: u64 = 64 << 10;
/// The events that the room needs to show the stages and the end of a run.
const ESSENTIAL: [&str; 4] = ["stage", "done", "cancelled", "run_failed"];
/// The exchange turns that a round shows before it has more.
pub const TURNS: u32 = 2;
const TURNS_DIR: &str = "turns";

/// The order of the stages. `exchange` repeats once per turn.
const STAGES: [&str; 8] = ["plan", "exchange", "tiebreak", "implement", "gates", "crossreview", "revise", "final"];

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    /// The two agent names, in the order of the round.
    pub agents: Vec<String>,
    pub stages: Vec<Stage>,
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

/// One message of the round: `{from, to, kind, body}`, so a bridge such as
/// A2A can expose it later.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub from: String,
    /// An agent name, `all` or `user`.
    pub to: String,
    /// `plan`, `reply`, `tiebreak`, `implementation`, `gates`, `review`,
    /// `revision`, `final`, `user`, `answer` or `event`.
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

/// The modification time and the size of a regular file, else `None`.
type Stamp = (i64, i64, u64);

fn stamp_at(dir: &OwnedFd, name: &str) -> Option<Stamp> {
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    (FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile).then_some((
        stat.st_mtime as i64,
        stat.st_mtime_nsec as i64,
        stat.st_size as u64,
    ))
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
fn known(key: &str) -> Option<bool> {
    const ROOT: [&str; 9] = [
        "events.jsonl",
        "messages.jsonl",
        "manifest.json",
        "tiebreak.json",
        "implementation.json",
        "gates.json",
        "crossreview.json",
        "revision.json",
        "open-questions.txt",
    ];
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

/// A round folder, read again only where a file changed.
pub struct RoundReader {
    dir: PathBuf,
    /// By relative name (`turns/plan-opus.json`): its stamp and its bytes.
    files: BTreeMap<String, (Stamp, Option<Vec<u8>>)>,
}

impl RoundReader {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            files: BTreeMap::new(),
        }
    }

    /// Reads the known files that are new or changed, up to `READ_FILES`
    /// files and `READ_BYTES` bytes. Returns true if any file came, went or
    /// changed.
    pub fn refresh(&mut self) -> bool {
        let mut seen = BTreeMap::new();
        let mut changed = false;
        let mut bytes_kept = 0;
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
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().to_str() else {
                    continue;
                };
                let key = format!("{prefix}{name}");
                let Some(read) = known(&key) else {
                    continue;
                };
                if seen.len() == READ_FILES {
                    break;
                }
                let Some(stamp) = stamp_at(&dir, name) else {
                    continue;
                };
                let fits = bytes_kept + stamp.2 <= READ_BYTES;
                // A kept file also counts against the budget: one that no
                // longer fits is dropped, and one that fits again is read.
                let bytes = match self.files.remove(&key) {
                    Some((old, Some(bytes))) if old == stamp && fits => Some(bytes),
                    Some((old, None)) if old == stamp && !(read && fits) => None,
                    _ => {
                        changed = true;
                        (read && fits).then(|| read_at(&dir, name)).flatten()
                    }
                };
                // A file can grow between the stamp and the read.
                let bytes = bytes.filter(|b| bytes_kept + b.len() as u64 <= READ_BYTES);
                bytes_kept += bytes.as_ref().map_or(0, |b| b.len() as u64);
                seen.insert(key, (stamp, bytes));
            }
        }
        // Each file that is still there was taken out above, so the rest is gone.
        changed |= !self.files.is_empty();
        self.files = seen;
        changed
    }

    pub fn room(&self) -> Room {
        let files = self
            .files
            .iter()
            .map(|(name, (stamp, bytes))| (name.as_str(), (stamp.0, bytes.as_deref())))
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

/// Polls `dir` and sends the room each time it changes, and once at once.
/// `extra` adds a value that also counts as a change, such as the state of
/// an in-app run. Ends when `send` returns false.
pub async fn watch<X: PartialEq + Send + 'static>(
    dir: PathBuf,
    mut extra: impl FnMut() -> X + Send + 'static,
    mut send: impl FnMut(Room, &X) -> bool + Send + 'static,
) {
    let mut reader = RoundReader::new(dir);
    let mut last: Option<(Room, X)> = None;
    let mut poll = POLL;
    loop {
        let (r, room) = match tokio::task::spawn_blocking(move || {
            let changed = reader.refresh();
            let room = changed.then(|| reader.room());
            (reader, room)
        })
        .await
        {
            Ok(done) => done,
            Err(_) => return,
        };
        reader = r;
        poll = if room.is_some() { POLL } else { (poll + POLL).min(POLL_SLOW) };
        let x = extra();
        let room = match (room, last.take()) {
            (Some(room), _) => Some(room),
            (None, Some((old, old_x))) if old_x != x => Some(old),
            (None, Some(kept)) => {
                last = Some(kept);
                None
            }
            (None, None) => Some(Room::default()),
        };
        if let Some(room) = room {
            if !send(room.clone(), &x) {
                return;
            }
            last = Some((room, x));
        }
        tokio::time::sleep(poll).await;
    }
}

impl Default for Room {
    fn default() -> Self {
        project(&BTreeMap::new())
    }
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
    at: String,
    kind: String,
    detail: String,
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().map(text).filter(|s| !s.trim().is_empty()).collect())
        .unwrap_or_default()
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

fn stage_label(id: &str) -> String {
    match id.strip_prefix("exchange-") {
        Some(turn) => format!("Exchange {turn}"),
        None => match id {
            "crossreview" => "Cross review".to_owned(),
            "final" => "Final review".to_owned(),
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
fn stage_of(detail: &str) -> Option<String> {
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
fn stage_key(id: &str) -> (usize, u32) {
    (stage_index(id), turn_of(id))
}

/// `files` maps relative names to the mtime in seconds and the bytes, if read.
fn project(files: &BTreeMap<&str, (i64, Option<&[u8]>)>) -> Room {
    let json = |name: &str| -> Option<Value> {
        let value: Value = serde_json::from_slice(files.get(name)?.1?).ok()?;
        value.is_object().then_some(value)
    };
    let lines = |name: &str| -> Vec<Value> {
        let Some((_, Some(bytes))) = files.get(name) else {
            return Vec::new();
        };
        // The last line can be half written.
        bytes
            .split(|&b| b == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter(Value::is_object)
            .collect()
    };
    let mtime = |name: &str| files.get(name).map(|(secs, _)| iso(secs * 1000));
    let manifest = json("manifest.json");

    let mut agents: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        if !name.is_empty() && !agents.iter().any(|a| a == name) {
            agents.push(name.to_owned());
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
    let peer = |me: &str| agents.iter().find(|a| *a != me).cloned().unwrap_or_else(|| "all".to_owned());
    let known = |name: &str| agents.iter().any(|a| a == name).then(|| name.to_owned());

    let events: Vec<Event> = lines("events.jsonl")
        .iter()
        .map(|e| Event {
            at: text(&e["at"]),
            kind: text(&e["kind"]),
            detail: text(&e["detail"]),
        })
        .collect();

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
    let mut ids = vec!["plan".to_owned()];
    ids.extend((1..=TURNS).chain(turns_seen.iter().copied()).collect::<BTreeSet<_>>().into_iter().map(|n| format!("exchange-{n}")));
    ids.extend(STAGES[2..].iter().map(|s| (*s).to_owned()));
    let last = started.iter().map(|(s, _)| s).max_by_key(|s| stage_key(s)).cloned();
    let stages = ids
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
            Stage {
                id: id.clone(),
                label: stage_label(id),
                state,
                started: at,
            }
        })
        .collect();

    // Agents' turns and positions.
    let mut messages = Vec::new();
    let mut message = |from: &str, to: String, kind: &str, stage: &str, at: Option<String>, summary_of: &str, body: String| {
        messages.push(Message {
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
        message(agent, "all".to_owned(), "plan", "plan", mtime(&name), &text(&plan["plan"]), body);
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
            message(agent, peer(agent), "reply", &stage, mtime(&name), &text(&reply["message"]), body);
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
            replies.push(Reply {
                agent: agent.clone(),
                agreed,
                implementer,
                conceded,
            });
        }
        let converged = (replies.len() == agents.len() && agents.len() == 2 && replies.iter().all(|r| r.agreed))
            .then(|| replies[0].implementer.clone())
            .flatten()
            .filter(|pick| replies.iter().all(|r| r.implementer.as_ref() == Some(pick)));
        turns.push(Turn {
            turn: n,
            replies,
            converged,
        });
    }

    let tiebreak = json("tiebreak.json").map(|t| Tiebreak {
        implementer: known(&text(&t["implementer"])),
        why: text(&t["why"]),
    });
    if let Some(t) = &tiebreak {
        // duo's tiebreak is by its first agent; an in-app run names it.
        let by = events.iter().find_map(|e| e.detail.strip_prefix("tiebreak by ")).and_then(known);
        let from = by.or_else(|| agents.first().cloned()).unwrap_or_else(|| "tiebreak".to_owned());
        let body = format!("**Implementer:** {}\n\n{}", t.implementer.as_deref().unwrap_or("?"), t.why);
        message(&from, "all".to_owned(), "tiebreak", "tiebreak", mtime("tiebreak.json"), &t.why, body);
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
    let walkover = events.iter().find(|e| e.kind == "walkover").and_then(|e| known(e.detail.split_whitespace().next()?));
    let converged_turn = turns.iter().find(|t| t.converged.is_some());
    let user_pick = events.iter().rev().find(|e| e.kind == "decision" && e.detail.starts_with("user chose ")).and_then(|e| known(e.detail.strip_prefix("user chose ")?));
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
        for (title, key) in [("Deviations", "deviations"), ("Left undone", "left_undone"), ("Missing dependencies", "missing_dependencies")] {
            let items = strings(&i[key]);
            if !items.is_empty() {
                body.push_str(&format!("\n\n**{title}:**\n{}", bullets(&items)));
            }
        }
        message(&from, peer(&from), "implementation", "implement", mtime("implementation.json"), &text(&i["summary"]), body);
    }

    let gates: Vec<Gate> = files
        .get("gates.json")
        .and_then(|(_, b)| serde_json::from_slice::<Value>((*b)?).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|g| Gate {
            command: text(&g["command"]),
            code: g["code"].as_i64(),
            output: text(&g["output"]),
        })
        .collect();
    if !gates.is_empty() {
        let passed = gates.iter().filter(|g| g.code == Some(0)).count();
        let line = format!("{passed} of {} gates passed", gates.len());
        let body = gates
            .iter()
            .map(|g| format!("- `{}` → {}", g.command, g.code.map_or("no exit".to_owned(), |c| format!("exit {c}"))))
            .collect::<Vec<_>>()
            .join("\n");
        message("run", "all".to_owned(), "gates", "gates", mtime("gates.json"), &line, format!("{line}.\n\n{body}"));
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
        message(&from, to, "review", "crossreview", mtime("crossreview.json"), &review.message, body);
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
        message(&from, to, "revision", "revise", mtime("revision.json"), &text(&r["message"]), body);
    }

    let blocking_of = |value: &Value| -> Vec<Blocking> {
        value
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|b| Blocking {
                what: text(&b["what"]),
                why: text(&b["why"]),
                at: text(&b["where"]),
            })
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
        message(agent, "user".to_owned(), "final", "final", mtime(&name), &review.task_met, body);
        finals.push(review);
    }
    let outcome = manifest.as_ref().map(|m| Outcome {
        ship: m["ship"].as_bool().unwrap_or(false),
        ship_basis: text(&m["ship_basis"]),
        blocking: blocking_of(&m["blocking"]),
        backlog: strings(&m["backlog"]),
        stopped: m["stopped"].as_str().map(str::to_owned),
        base: m["base"].as_str().map(str::to_owned),
    });

    for entry in lines("messages.jsonl") {
        let kind = if entry["kind"] == "answer" { "answer" } else { "user" };
        let body = text(&entry["body"]);
        let stage = text(&entry["stage"]);
        message("user", "all".to_owned(), kind, &stage, Some(text(&entry["at"])), &body, body.clone());
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
        let notice = || Notice {
            agent: name_in(&event.detail),
            stage: current.clone(),
            detail: event.detail.clone(),
        };
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
        );
        if shown {
            let body = format!("**{}** — {}", event.kind.replace('_', " "), event.detail);
            message("run", "all".to_owned(), "event", &current, Some(event.at.clone()), &event.detail, body);
        }
    }

    let questions = match files.get("open-questions.txt") {
        Some((_, Some(bytes))) if questions_open || ended == Some("stopped") => String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };

    messages.sort_by_key(|m| (stage_key(&m.stage), m.at.clone()));
    Room {
        agents: agents.clone(),
        stages,
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
        outcome,
        degraded,
        violations,
        gates,
        questions,
        ended: ended.map(str::to_owned),
        ended_at,
    }
}

/// The highest `t<n>` of the turn files.
fn file_turns<'a>(files: &'a BTreeMap<&str, (i64, Option<&[u8]>)>) -> impl Iterator<Item = u32> + 'a {
    files
        .keys()
        .filter_map(|k| k.strip_prefix("turns/t")?.split('-').next()?.parse::<u32>().ok())
        .filter(|n| *n > 0)
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
    out.extend_from_slice(format!("\n\n[The next {} bytes are omitted: the file is over the limit.]\n", bytes.len() - keep).as_bytes());
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

    /// Why the harness copy is not complete, if it is not.
    pub fn mirror_error(&self) -> Option<&str> {
        self.mirror_error.as_deref()
    }

    /// The first write that failed, if any.
    pub fn failure(&self) -> Option<String> {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn record(&self, path: &Path, written: std::io::Result<()>) {
        if let Err(e) = written {
            let why = format!("cannot write {}: {e}", path.display());
            self.failed.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert(why);
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

    pub fn append(&self, name: &str, value: &Value) {
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
        self.record(&path, written);
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
        let dir = std::env::temp_dir().join(format!("zwinger-round-{}", uuid::Uuid::new_v4()));
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
    fn reads_the_sample_round() {
        assert!(is_round(&sample()));
        let room = read_room(&sample());
        assert_eq!(room.agents, ["opus", "sol"]);
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
        let kinds: Vec<(&str, &str)> = room.messages.iter().filter(|m| m.kind != "event").map(|m| (m.from.as_str(), m.kind.as_str())).collect();
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
        let mirror = Mirror {
            root: root.clone(),
            parts: [".zwinger", "tasks", "fix", "01"].map(str::to_owned).to_vec(),
        };
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
        let first: Value = serde_json::from_str(std::fs::read_to_string(dir.join("events.jsonl")).unwrap().trim()).unwrap();
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
        assert!(reader.files.get("turns/plan-b.prompt.md").is_none_or(|(_, b)| b.is_none()));

        // The bytes: five files of 7 MiB, of which four fit.
        let dir = temp_dir();
        for i in 0..5 {
            std::fs::write(dir.join(format!("final-b{i}.json")), vec![b' '; 7 << 20]).unwrap();
        }
        let mut reader = RoundReader::new(dir);
        reader.refresh();
        let read: u64 = reader.files.values().filter_map(|(_, b)| b.as_ref()).map(|b| b.len() as u64).sum();
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
            let read: u64 = reader.files.values().filter_map(|(_, b)| b.as_ref()).map(|b| b.len() as u64).sum();
            assert!(read <= READ_BYTES);
        }
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
    fn known_names() {
        for name in ["events.jsonl", "messages.jsonl", "final-opus.json", "turns/plan-sol.json", "turns/t12-opus.json"] {
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
}
