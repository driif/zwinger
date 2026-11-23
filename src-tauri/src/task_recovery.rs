//! The recovery records of a task run, so that an ended run can continue
//! in a new run. The host round owns two versioned records; the guest copy
//! is an output mirror only and is never read back.
//!
//! - `run.json`: what the run started with. It is written before any agent
//!   starts, and a task edit does not change it.
//! - `checkpoint.json`: the completed groups of stages, the agents that
//!   reported in each, the decisions, and the SHA-256 of each artifact that
//!   a later prompt reads. The run writes an empty one directly after
//!   `run.json`, and rewrites it at each group boundary.
//! - `budget.jsonl`: a durable reservation before each accepted message is
//!   queued, plus durable apply-start and apply-complete entries around its
//!   message write. The first failed write of the round adds `write-failed`.
//!
//! A continuation is a new run. It gets `run.json` with a link to its
//! source, the validated artifacts, the messages and the events of the
//! completed groups, and its own `checkpoint.json` as the last write.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::run_git::Origin;
use crate::session::{Task, TaskAgent};
use crate::task_agents::Limits;
use crate::task_artifacts::{RoundWriter, read_record, stage_label, stage_of};
use crate::task_participants::participants;
use crate::task_runs::{Config, MESSAGE_LIMIT, MESSAGES_BYTES, MESSAGES_COUNT, converged};

pub const VERSION: u32 = 2;
pub const RUN: &str = "run.json";
pub const CHECKPOINT: &str = "checkpoint.json";
pub const BUDGET: &str = "budget.jsonl";
/// The `from` of a continuation whose stages are all complete, but whose
/// source stopped before it recorded its result.
pub const SETTLE: &str = "settle";
const MESSAGES: &str = "messages.jsonl";
const WRITE_FAILED: &str = "the run could not write its record, so its state is not known; start a new run instead";
const EVENTS: &str = "events.jsonl";
/// The groups in which agents report, each with its own result file.
const AGENT_GROUPS: [&str; 6] = ["plan", "tiebreak", "implement", "crossreview", "revise", "final"];
/// The artifacts of a round that a checkpoint can name, besides the ones
/// of each agent.
const FIXED: [&str; 9] = [
    "tiebreak.json",
    "plan.md",
    "implementation.json",
    "implementation-diff.txt",
    "revision-diff.txt",
    "gates.json",
    "crossreview.json",
    "revision.json",
    "revision-gates.json",
];

/// What a run started with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunRecord {
    pub version: u32,
    pub title: String,
    pub request: String,
    /// In order. A `null` model is the default model of the CLI, not an
    /// exact version.
    pub agents: [TaskAgent; 2],
    pub gates: Vec<String>,
    /// The working dir as the task names it, before the VM binding.
    pub workdir: String,
    pub config: RecordedConfig,
    #[serde(default)]
    pub continues: Option<Link>,
    /// Where the working dir was at the start. A record from before has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordedConfig {
    pub turns: u32,
    pub idle_ms: u64,
    pub hard_ms: u64,
}

/// The run that a continuation continues, and its first group. `repeats`
/// is true if the source started that group before it stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Link {
    pub run: String,
    pub from: String,
    pub repeats: bool,
}

impl RunRecord {
    /// `task.workdir` is the working dir as the task names it.
    pub fn new(task: &Task, config: Config) -> Self {
        Self {
            version: VERSION,
            title: task.title.clone(),
            request: task.request.clone(),
            agents: task.agents.clone(),
            gates: task.gates.clone(),
            workdir: task.workdir.clone(),
            config: RecordedConfig {
                turns: config.turns,
                idle_ms: config.limits.idle.as_millis() as u64,
                hard_ms: config.limits.hard.as_millis() as u64,
            },
            continues: None,
            origin: None,
        }
    }

    pub fn config(&self) -> Config {
        Config {
            turns: self.config.turns,
            limits: Limits {
                idle: Duration::from_millis(self.config.idle_ms),
                hard: Duration::from_millis(self.config.hard_ms),
            },
        }
    }

    /// The task as the run started it, with the working dir of the
    /// current VM binding.
    pub fn task(&self, id: &str, slug: &str, workdir: String) -> Task {
        Task {
            id: id.to_owned(),
            slug: slug.to_owned(),
            title: self.title.clone(),
            request: self.request.clone(),
            agents: self.agents.clone(),
            gates: self.gates.clone(),
            workdir,
        }
    }

    /// The participant ids of the run, from its recorded agents.
    fn names(&self) -> [String; 2] {
        participants(&self.agents).map(|p| p.id)
    }
}

/// The state of a run at its last group boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    pub version: u32,
    /// The completed groups, in order: `plan`, `exchange-<n>`, `tiebreak`,
    /// `decision`, `implement`, `gates`, `crossreview`, `revise`,
    /// `revision-gates`, `final`.
    pub groups: Vec<String>,
    /// By completed agent group: the agents that reported. The other
    /// agents of the group were degraded.
    pub outcomes: BTreeMap<String, Vec<String>>,
    pub decisions: Decisions,
    /// By relative name: the SHA-256 of the artifact, as hex.
    pub artifacts: BTreeMap<String, String>,
    /// The complete lines of `events.jsonl` at the boundary.
    pub events: usize,
    /// By completed check group, the files it saw. A missing fence means
    /// its snapshot failed or the working dir was outside Git.
    pub fences: BTreeMap<String, Fence>,
    /// Files left from checks dropped before their cleanup completed.
    #[serde(default)]
    pub cleanup: Vec<String>,
    #[serde(default)]
    pub cleaning: bool,
}

/// The git state of the working dir, without the round copies of the task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Fence {
    pub head: String,
    /// Covers the worktree, the index and the untracked files.
    pub hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Decisions {
    /// The agents with a plan. The others are degraded.
    pub planned: Vec<String>,
    /// The user's pick that the decision used.
    pub picked: Option<String>,
    pub implementer: Option<String>,
    /// `accept`, `revise` or `missing`.
    pub verdict: Option<String>,
    pub base: Option<String>,
    pub detection: Option<bool>,
    pub backlog: Vec<String>,
    pub violations: Vec<String>,
}

impl Checkpoint {
    /// The checkpoint of a run before its first group.
    pub fn empty() -> Self {
        Self {
            version: VERSION,
            groups: Vec::new(),
            outcomes: BTreeMap::new(),
            decisions: Decisions::default(),
            artifacts: BTreeMap::new(),
            events: 0,
            fences: BTreeMap::new(),
            cleanup: Vec::new(),
            cleaning: false,
        }
    }
}

/// The checks after the last write stage. A `revise` verdict whose revision
/// did not run has none: the revision changes the files anyway, and its
/// prompt needs the original findings.
fn check_groups(checkpoint: &Checkpoint) -> impl Iterator<Item = &String> {
    let groups = &checkpoint.groups;
    let pending_revise =
        checkpoint.decisions.verdict.as_deref() == Some("revise") && !groups.iter().any(|g| g == "revise");
    let start = groups.iter().rposition(|g| g == "implement" || g == "revise").map_or(0, |i| i + 1);
    let start = if pending_revise { groups.len() } else { start };
    groups[start..].iter().filter(|g| matches!(g.as_str(), "gates" | "revision-gates" | "crossreview" | "final"))
}

pub fn first_check(checkpoint: &Checkpoint) -> Option<&str> {
    check_groups(checkpoint).next().map(String::as_str)
}

fn first_unfenced(checkpoint: &Checkpoint) -> Option<String> {
    check_groups(checkpoint).find(|group| !checkpoint.fences.contains_key(*group)).cloned()
}

/// The first check that did not see the current files. A missing fence is
/// deliberately different from a matching fence.
pub fn first_invalid(checkpoint: &Checkpoint, current: Option<&Fence>) -> Option<String> {
    check_groups(checkpoint).find(|group| checkpoint.fences.get(*group) != current || current.is_none()).cloned()
}

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether an ended run can continue, for the room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Resume {
    /// `from` is the first group that the continuation runs, and `label`
    /// its name for the user. `repeats` is true if the run started that
    /// group before it stopped.
    Ready {
        from: String,
        label: String,
        repeats: bool,
        recheck: Option<String>,
    },
    Unavailable {
        unavailable: String,
    },
}

/// The validated state that a continuation takes over.
#[derive(Debug, Clone)]
pub struct Recovered {
    pub record: RunRecord,
    /// The run whose records these are. A continuation that stopped before
    /// its checkpoint resolves back to its source.
    pub source: String,
    pub checkpoint: Checkpoint,
    /// By relative name, each with the digest of the checkpoint.
    pub artifacts: BTreeMap<String, Vec<u8>>,
    /// The complete lines of `messages.jsonl`.
    pub messages: Vec<Value>,
    /// The complete lines of `budget.jsonl`, with an interrupted message
    /// write settled.
    pub ledger: Vec<Value>,
    /// The events of the completed groups.
    pub events: Vec<Value>,
    /// The last `pick` event, while the implementer is not decided: its
    /// line index and the event.
    pub pick: Option<(usize, Value)>,
    pub from: String,
    pub repeats: bool,
}

impl Recovered {
    /// The user's pick that the decision has not used yet.
    pub fn picked(&self) -> Option<String> {
        let (_, event) = self.pick.as_ref()?;
        event["detail"].as_str()?.strip_prefix("user picked ").map(str::to_owned)
    }
}

/// The runs of a task folder and their links.
pub struct TaskRuns<'a> {
    dir: &'a Path,
    names: &'a [String],
    /// By run: the newest run that continues it.
    continued_by: BTreeMap<String, String>,
    /// By run: the run that it continues.
    continues: BTreeMap<String, String>,
}

impl<'a> TaskRuns<'a> {
    /// Reads the `run.json` of each run once. A run with no valid record
    /// has no links.
    pub fn read(dir: &'a Path, names: &'a [String]) -> Self {
        let mut continued_by = BTreeMap::new();
        let mut continues = BTreeMap::new();
        for name in names {
            if let Ok(Some(RunRecord { continues: Some(link), .. })) = read_run(&dir.join(name)) {
                continued_by.insert(link.run.clone(), name.clone());
                continues.insert(name.clone(), link.run);
            }
        }
        Self { dir, names, continued_by, continues }
    }

    /// The run that `run` continues.
    pub fn continues(&self, run: &str) -> Option<String> {
        self.continues.get(run).cloned()
    }

    pub fn inspect(&self, run: &str) -> Resume {
        match self.load(run) {
            Ok(r) => Resume::Ready {
                label: stage_label(&r.from),
                recheck: first_check(&r.checkpoint).map(stage_label),
                from: r.from,
                repeats: r.repeats,
            },
            Err(why) => Resume::Unavailable { unavailable: why },
        }
    }

    /// The validated state that a continuation of `run` takes over, or why
    /// `run` cannot continue.
    pub fn load(&self, run: &str) -> std::result::Result<Recovered, String> {
        if let Some(next) = self.continued_by.get(run) {
            return Err(format!("run {next} continues this run; resume run {next}"));
        }
        let mut source = run.to_owned();
        for _ in 0..=self.names.len() {
            let dir = self.dir.join(&source);
            let record = read_run(&dir)?.ok_or_else(|| {
                "this run started before zwinger saved recovery records; start a new run instead".to_owned()
            })?;
            let checkpoint = match read_record(&dir, CHECKPOINT)? {
                Some(bytes) => parse::<Checkpoint>(&bytes, CHECKPOINT)?,
                // Only a run that stopped before its first event can have no
                // checkpoint: it writes one before it starts its stages.
                None if has_own_events(&dir)? => {
                    return Err(format!("{CHECKPOINT} is missing, but the run started its stages"));
                }
                None => match record.continues.clone() {
                    // A continuation that stopped before its checkpoint has
                    // no state of its own: it resolves back to its source.
                    Some(link) if self.names.contains(&link.run) && link.run != source => {
                        source = link.run;
                        continue;
                    }
                    Some(link) => return Err(format!("run {} that it continues is missing", link.run)),
                    None => Checkpoint::empty(),
                },
            };
            return recover(&dir, source, record, checkpoint);
        }
        Err("its continuation links form a loop".to_owned())
    }
}

/// True if `events.jsonl` has a line that the run did not inherit.
fn has_own_events(dir: &Path) -> std::result::Result<bool, String> {
    let bytes = read_record(dir, EVENTS)?.unwrap_or_default();
    let (complete, partial) = lines(&bytes);
    Ok(partial
        || complete.iter().any(|l| serde_json::from_slice::<Value>(l).map_or(true, |e| e["inherited"].is_null())))
}

/// The working dir that a run recorded when it started, as the task named
/// it. `None` only if the run has no `run.json`.
pub(crate) fn recorded_workdir(dir: &Path) -> std::result::Result<Option<String>, String> {
    Ok(read_run(dir)?.map(|run| run.workdir))
}

fn read_run(dir: &Path) -> std::result::Result<Option<RunRecord>, String> {
    read_record(dir, RUN)?.map(|bytes| parse::<RunRecord>(&bytes, RUN)).transpose()
}

/// A versioned record: a different version is incompatible, any other
/// difference is damage.
fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8], name: &str) -> std::result::Result<T, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| format!("{name} is damaged ({e})"))?;
    match value["version"].as_u64() {
        Some(v) if v == u64::from(VERSION) => {}
        Some(v) => return Err(format!("{name} has version {v}, and this app reads version {VERSION}")),
        None => return Err(format!("{name} is damaged (it has no version)")),
    }
    if name == CHECKPOINT && value.get("fences").is_none() {
        return Err(format!("{CHECKPOINT} uses an older incompatible recovery format"));
    }
    serde_json::from_value(value).map_err(|e| format!("{name} is damaged ({e})"))
}

/// The complete lines of a JSON Lines record, and true if a partial line follows.
fn lines(bytes: &[u8]) -> (Vec<&[u8]>, bool) {
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    let partial = !lines.pop().unwrap_or_default().is_empty();
    (lines, partial)
}

fn object(line: &[u8], name: &str, index: usize) -> std::result::Result<Value, String> {
    serde_json::from_slice::<Value>(line)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| format!("line {} of {name} is damaged", index + 1))
}

/// A message line as the run writes it: `user` from the user, `answer`
/// from the user with the digest of its `questions`, or `external` from an
/// A2A client with its contribution id in `ref`.
fn message(line: &[u8], index: usize) -> std::result::Result<Value, String> {
    let value = object(line, MESSAGES, index)?;
    let text = |key: &str| value[key].as_str().is_some();
    let complete = text("at")
        && text("stage")
        && text("body")
        && match value["kind"].as_str() {
            Some("user") => true,
            Some("answer") => text("questions"),
            Some("external") => text("from") && value["ref"].as_str().is_some_and(|r| !r.is_empty()),
            _ => false,
        };
    if !complete {
        return Err(format!("line {} of {MESSAGES} is damaged", index + 1));
    }
    Ok(value)
}

/// A line of `budget.jsonl`: the bytes of an accepted message, and for an
/// A2A contribution its `ref` and the `digest` of its text.
fn reservation(line: &[u8], index: usize) -> std::result::Result<Value, String> {
    let value = object(line, BUDGET, index)?;
    let complete = match value["kind"].as_str() {
        Some("message") => value["bytes"].as_u64().is_some(),
        Some("external") => {
            value["bytes"].as_u64().is_some()
                && value["ref"].as_str().is_some_and(|r| !r.is_empty())
                && value["digest"].as_str().is_some_and(|d| d.len() == 64)
        }
        Some("apply-start") => {
            value["ref"].as_str().is_some_and(|r| !r.is_empty())
                || value["digest"].as_str().is_some_and(|d| d.len() == 64)
        }
        Some("apply-complete" | "write-failed") => true,
        _ => false,
    };
    if !complete {
        return Err(format!("line {} of {BUDGET} is damaged", index + 1));
    }
    Ok(value)
}

fn recover(
    dir: &Path,
    source: String,
    record: RunRecord,
    checkpoint: Checkpoint,
) -> std::result::Result<Recovered, String> {
    let names = record.names();
    let mut artifacts = BTreeMap::new();
    for (name, hex) in &checkpoint.artifacts {
        if !allowed(name, &names, record.config.turns) {
            return Err(format!("{CHECKPOINT} names an unknown file {name}"));
        }
        let bytes = read_record(dir, name)?.ok_or_else(|| format!("{name} is missing"))?;
        if digest(&bytes) != *hex {
            return Err(format!("{name} changed after the checkpoint"));
        }
        artifacts.insert(name.clone(), bytes);
    }
    let context = Context { record: &record, names: &names, checkpoint: &checkpoint, artifacts: &artifacts };
    context.validate()?;

    // The budget comes first: its last line tells how to read the messages.
    // A partial last line is an append that did not complete, so the run
    // did not accept that message.
    let bytes = read_record(dir, BUDGET)?.unwrap_or_default();
    let mut ledger: Vec<Value> =
        lines(&bytes).0.iter().enumerate().map(|(i, line)| reservation(line, i)).collect::<Result<_, _>>()?;
    if ledger.iter().any(|l| l["kind"] == "write-failed") {
        return Err(WRITE_FAILED.to_owned());
    }
    let bytes = read_record(dir, MESSAGES)?.unwrap_or_default();
    let (complete, partial) = lines(&bytes);
    let messages: Vec<Value> =
        complete.iter().enumerate().map(|(i, line)| message(line, i)).collect::<Result<_, _>>()?;
    interrupted(&mut ledger, &messages, partial)?;
    agree(&messages, &ledger)?;

    let bytes = read_record(dir, EVENTS)?.unwrap_or_default();
    let (complete, _) = lines(&bytes);
    if checkpoint.events > complete.len() {
        return Err(format!("{EVENTS} is shorter than its checkpoint"));
    }
    let events: Vec<Value> = complete[..checkpoint.events]
        .iter()
        .enumerate()
        .map(|(i, l)| object(l, EVENTS, i))
        .collect::<Result<_, _>>()?;
    // The later lines are only read: a damaged one does not matter.
    let all: Vec<(usize, Value)> =
        complete.iter().enumerate().filter_map(|(i, l)| Some((i, object(l, EVENTS, i).ok()?))).collect();
    let failed_write = all.iter().any(|(_, e)| {
        e["kind"] == "run_failed" && e["detail"].as_str().is_some_and(|d| d.starts_with("cannot write "))
    });
    if failed_write {
        return Err(WRITE_FAILED.to_owned());
    }
    let decided = checkpoint.groups.iter().any(|g| g == "decision");
    let pick = (!decided).then(|| all.iter().rev().find(|(_, e)| e["kind"] == "pick").cloned()).flatten();
    let picked = pick
        .as_ref()
        .and_then(|(_, e)| e["detail"].as_str()?.strip_prefix("user picked "))
        .is_some_and(|name| checkpoint.decisions.planned.iter().any(|p| p == name));
    let next = match context.next(&checkpoint.groups, picked) {
        Some(next) => next,
        None if all.iter().any(|(_, e)| e["kind"] == "done") => return Err("the run completed all stages".to_owned()),
        None => SETTLE.to_owned(),
    };
    let unfenced = first_unfenced(&checkpoint);
    let from = unfenced.clone().unwrap_or(next);
    let repeats = unfenced.is_some()
        || all[checkpoint.events..].iter().any(|(_, e)| match e["kind"].as_str() {
            Some("stage") => e["detail"].as_str().and_then(stage_of).as_deref() == Some(from.as_str()),
            Some("waiting") => from == "decision",
            _ => false,
        });
    Ok(Recovered { record, source, checkpoint, artifacts, messages, ledger, events, pick, from, repeats })
}

/// A process kill during a message write leaves its apply-start as the last
/// budget line: the run holds the budget lock across the three appends. The
/// complete message lines decide what happened. With no new line, the
/// message was not applied: the start goes, the charge stays, and a partial
/// line is dropped. With one new line that matches the start, the message
/// was applied: only the apply-complete is missing, so it is added.
fn interrupted(ledger: &mut Vec<Value>, messages: &[Value], partial: bool) -> std::result::Result<(), String> {
    let Some(start) = ledger.last().filter(|l| l["kind"] == "apply-start").cloned() else {
        return if partial { Err(format!("{MESSAGES} ends in a partial line")) } else { Ok(()) };
    };
    let applied = ledger.iter().filter(|l| l["kind"] == "apply-complete").count();
    match (messages.len().checked_sub(applied), messages.last()) {
        (Some(0), _) => {
            ledger.pop();
            Ok(())
        }
        (Some(1), _) if partial => Err(format!("{MESSAGES} ends in a partial line")),
        (Some(1), Some(last)) => {
            let same = if last["kind"] == "external" {
                start["ref"] == last["ref"]
            } else {
                start["digest"] == digest(last["body"].as_str().unwrap_or_default().as_bytes())
            };
            if !same {
                return Err(format!(
                    "the last line of {MESSAGES} is not the message that the run was writing when it stopped"
                ));
            }
            ledger.push(serde_json::json!({ "kind": "apply-complete" }));
            Ok(())
        }
        _ => Err(format!("{MESSAGES} does not agree with {BUDGET} after an interrupted message write")),
    }
}

/// Each message has its line in the budget: the run records the budget
/// before it queues a message.
fn agree(messages: &[Value], ledger: &[Value]) -> std::result::Result<(), String> {
    let damaged = |why: &str| format!("{BUDGET} is damaged ({why})");
    let mut count = 0;
    let mut bytes = 0usize;
    let mut refs = BTreeSet::new();
    let mut pending: Option<(usize, &Value)> = None;
    let mut applied = Vec::new();
    for (index, line) in ledger.iter().enumerate() {
        match line["kind"].as_str() {
            // The budget lock is held from an apply-start to its completion.
            Some("message" | "external") if pending.is_some() => return Err(damaged("unmatched apply-start")),
            Some("message" | "external") => {
                let size = usize::try_from(line["bytes"].as_u64().unwrap_or_default()).map_err(|_| damaged("bytes"))?;
                if !(1..=MESSAGE_LIMIT).contains(&size) {
                    return Err(damaged("bytes"));
                }
                count += 1;
                bytes = bytes.checked_add(size).ok_or_else(|| damaged("total bytes"))?;
                if count > MESSAGES_COUNT || bytes > MESSAGES_BYTES {
                    return Err(damaged("message limit"));
                }
                if let Some(id) = line["ref"].as_str()
                    && !refs.insert(id)
                {
                    return Err(damaged("duplicate advisory id"));
                }
            }
            Some("apply-start") => {
                if pending.is_some() {
                    return Err(damaged("unmatched apply-start"));
                }
                pending = Some((index, line));
            }
            Some("apply-complete") => {
                applied.push(pending.take().ok_or_else(|| damaged("apply-complete without a start"))?);
            }
            _ => return Err(damaged("entry kind")),
        }
    }
    if pending.is_some() {
        return Err(damaged("unmatched apply-start"));
    }
    if applied.len() != messages.len() {
        return Err(format!("{MESSAGES} does not agree with {BUDGET}"));
    }
    // Each applied user message or answer uses its own reservation, made
    // before its apply-start and of its size.
    let mut claimed = vec![false; ledger.len()];
    for (&(at, start), message) in applied.iter().zip(messages) {
        let body = message["body"].as_str().unwrap_or_default();
        let hex = digest(body.as_bytes());
        if message["kind"] == "external" {
            let id = message["ref"].as_str().unwrap_or_default();
            if start["ref"] != id
                || !ledger.iter().any(|l| {
                    l["kind"] == "external" && l["ref"] == id && l["digest"] == hex && l["bytes"] == body.len()
                })
            {
                return Err(damaged("applied advisory bytes or digest"));
            }
        } else {
            if start["digest"] != hex {
                return Err(damaged("applied message digest"));
            }
            let reservation = (0..at)
                .find(|&i| !claimed[i] && ledger[i]["kind"] == "message" && ledger[i]["bytes"] == body.len())
                .ok_or_else(|| damaged("bytes"))?;
            claimed[reservation] = true;
        }
    }
    Ok(())
}

fn is_agent_group(group: &str) -> bool {
    AGENT_GROUPS.contains(&group) || group.starts_with("exchange-")
}

/// True for a file that a checkpoint can name.
fn allowed(name: &str, names: &[String; 2], turns: u32) -> bool {
    FIXED.contains(&name)
        || names.iter().any(|a| {
            name == format!("turns/plan-{a}.json")
                || name == format!("final-{a}.json")
                || (1..=turns).any(|n| name == format!("turns/t{n}-{a}.json"))
        })
}

struct Context<'a> {
    record: &'a RunRecord,
    names: &'a [String; 2],
    checkpoint: &'a Checkpoint,
    artifacts: &'a BTreeMap<String, Vec<u8>>,
}

impl Context<'_> {
    fn json(&self, name: &str) -> Option<Value> {
        serde_json::from_slice(self.artifacts.get(name)?).ok()
    }

    /// The group that follows `done`, with or without a user's pick. `None`
    /// once the run completed all groups. As `Actor::drive` orders them.
    fn next(&self, done: &[String], picked: bool) -> Option<String> {
        let decisions = &self.checkpoint.decisions;
        let turns = self.record.config.turns;
        let Some(last) = done.last() else {
            return Some("plan".to_owned());
        };
        let next = match last.as_str() {
            "plan" if decisions.planned.len() == 1 || picked => "decision".to_owned(),
            "plan" if turns == 0 => "tiebreak".to_owned(),
            "plan" => "exchange-1".to_owned(),
            "tiebreak" => "decision".to_owned(),
            "decision" => "implement".to_owned(),
            "implement" => "gates".to_owned(),
            "gates" => "crossreview".to_owned(),
            "crossreview" if decisions.verdict.as_deref() == Some("revise") => "revise".to_owned(),
            "crossreview" => "final".to_owned(),
            "revise" if self.record.gates.is_empty() => "final".to_owned(),
            "revise" => "revision-gates".to_owned(),
            "revision-gates" => "final".to_owned(),
            "final" => return None,
            other => {
                let n: u32 = other.strip_prefix("exchange-")?.parse().ok()?;
                let replies = self.names.clone().map(|a| self.json(&format!("turns/t{n}-{a}.json")));
                if picked || converged(self.names, &replies).is_some() {
                    "decision".to_owned()
                } else if n < turns {
                    format!("exchange-{}", n + 1)
                } else {
                    "tiebreak".to_owned()
                }
            }
        };
        Some(next)
    }

    /// The groups are in an order that the run can make, and the decisions
    /// and artifacts agree with them.
    fn validate(&self) -> std::result::Result<(), String> {
        let groups = &self.checkpoint.groups;
        let decisions = &self.checkpoint.decisions;
        let damaged = |why: String| -> std::result::Result<(), String> {
            Err(format!("{CHECKPOINT} does not agree with its stages ({why})"))
        };
        for (i, group) in groups.iter().enumerate() {
            let done = &groups[..i];
            if self.next(done, false).as_ref() != Some(group) && self.next(done, true).as_ref() != Some(group) {
                return damaged(format!("{group} cannot follow {}", done.last().map_or("the start", |s| s.as_str())));
            }
        }
        let known = |name: &String| self.names.contains(name);
        let mut planned = decisions.planned.clone();
        planned.dedup();
        if planned.len() != decisions.planned.len() || !decisions.planned.iter().all(known) {
            return damaged("planned agents".to_owned());
        }
        let has = |group: &str| groups.iter().any(|g| g == group);
        if self
            .checkpoint
            .fences
            .keys()
            .any(|g| !has(g) || !matches!(g.as_str(), "gates" | "revision-gates" | "crossreview" | "final"))
        {
            return damaged("check fences".to_owned());
        }
        let cleanup: BTreeSet<&String> = self.checkpoint.cleanup.iter().collect();
        if (!self.checkpoint.cleaning && !cleanup.is_empty())
            || cleanup.len() != self.checkpoint.cleanup.len()
            || cleanup.iter().any(|name| {
                self.checkpoint.artifacts.contains_key(name.as_str())
                    || !(matches!(name.as_str(), "gates.json" | "revision-gates.json" | "crossreview.json")
                        || self.names.iter().any(|agent| *name == &format!("final-{agent}.json")))
            })
        {
            return damaged("cleanup files".to_owned());
        }
        if decisions.planned.is_empty() == has("plan") {
            return damaged("planned agents".to_owned());
        }
        let in_planned = |name: &Option<String>| name.as_ref().is_none_or(|n| decisions.planned.contains(n));
        if decisions.implementer.is_some() != has("decision") || !in_planned(&decisions.implementer) {
            return damaged("implementer".to_owned());
        }
        if !in_planned(&decisions.picked) {
            return damaged("pick".to_owned());
        }
        let verdict_ok = match decisions.verdict.as_deref() {
            None => !has("crossreview"),
            Some("accept" | "revise" | "missing") => has("crossreview"),
            Some(_) => false,
        };
        if !verdict_ok {
            return damaged("verdict".to_owned());
        }
        let mut required: Vec<String> = Vec::new();
        for (group, file) in [
            ("decision", "plan.md"),
            ("implement", "implementation-diff.txt"),
            ("revise", "revision-diff.txt"),
            ("gates", "gates.json"),
            ("revision-gates", "revision-gates.json"),
        ] {
            if has(group) && !required.iter().any(|r| r == file) {
                required.push(file.to_owned());
            }
        }
        // Each completed agent group names the agents that reported, and
        // each of them has its result. Absence is no proof of a degraded agent.
        let outcomes = &self.checkpoint.outcomes;
        if let Some(extra) = outcomes.keys().find(|g| !has(g) || !is_agent_group(g)) {
            return damaged(format!("an outcome for {extra}"));
        }
        let mut results: Vec<String> = Vec::new();
        for group in groups.iter().filter(|g| is_agent_group(g)) {
            let Some(reported) = outcomes.get(group) else {
                return damaged(format!("{group} has no outcome"));
            };
            let unique: BTreeSet<&String> = reported.iter().collect();
            if unique.len() != reported.len() || !reported.iter().all(known) {
                return damaged(format!("the outcome of {group}"));
            }
            let implementer = decisions.implementer.as_deref();
            let reviewer = implementer.and_then(|i| self.names.iter().find(|n| *n != i)).map(String::as_str);
            let only = |agent: Option<&str>| reported.iter().all(|r| Some(r.as_str()) == agent);
            let fits = match group.as_str() {
                "plan" => *reported == decisions.planned,
                "tiebreak" => *reported == [self.names[0].clone()],
                "implement" | "revise" => only(implementer),
                "crossreview" => only(reviewer),
                _ => true,
            };
            if !fits {
                return damaged(format!("the outcome of {group}"));
            }
            results.extend(reported.iter().map(|agent| match group.as_str() {
                "plan" => format!("turns/plan-{agent}.json"),
                "tiebreak" => "tiebreak.json".to_owned(),
                "implement" => "implementation.json".to_owned(),
                "crossreview" => "crossreview.json".to_owned(),
                "revise" => "revision.json".to_owned(),
                "final" => format!("final-{agent}.json"),
                exchange => format!("turns/t{}-{agent}.json", exchange.trim_start_matches("exchange-")),
            }));
        }
        if let Some(missing) = required.iter().chain(&results).find(|r| !self.artifacts.contains_key(*r)) {
            return damaged(format!("{missing} is not in it"));
        }
        if let Some(extra) = self.artifacts.keys().find(|a| !required.contains(a) && !results.contains(a)) {
            return damaged(format!("{extra} has no outcome"));
        }
        for name in results.iter().chain(&required).filter(|n| n.ends_with(".json")) {
            let parsed = self.json(name);
            let shaped = if name.ends_with("gates.json") {
                parsed.is_some_and(|v| v.is_array())
            } else {
                parsed.is_some_and(|v| v.is_object())
            };
            if !shaped {
                return damaged(format!("{name} is not valid"));
            }
        }
        if has("tiebreak") {
            let pick = self.json("tiebreak.json").map(|t| t["implementer"].as_str().unwrap_or_default().to_owned());
            if !pick.is_some_and(|p| self.names.contains(&p)) {
                return damaged("tiebreak.json names no agent of the run".to_owned());
            }
        }
        if has("crossreview") {
            // As `Actor::crossreview` decides the verdict from the review.
            let verdict = match self.json("crossreview.json") {
                Some(review) if review["verdict"] == "revise" => "revise",
                Some(_) => "accept",
                None => "missing",
            };
            if decisions.verdict.as_deref() != Some(verdict) {
                return damaged("the verdict and crossreview.json".to_owned());
            }
        }
        Ok(())
    }
}

/// Writes a new continuation of `run` in the order that keeps a crash
/// safe: `run.json` with the link, the artifacts, the messages and the
/// budget, the events, then `checkpoint.json` last. Each inherited line keeps its
/// first origin. A crash before the last write leaves a continuation with
/// no checkpoint, which resolves back to its source.
pub fn create(writer: &RoundWriter, recovered: &Recovered, run: &str) -> std::io::Result<()> {
    let mut record = recovered.record.clone();
    record.continues = Some(Link { run: run.to_owned(), from: recovered.from.clone(), repeats: recovered.repeats });
    writer.write_durable(RUN, &serde_json::to_vec_pretty(&record)?)?;
    for (name, bytes) in &recovered.artifacts {
        writer.write_durable(name, bytes)?;
    }
    let inherited = |lines: &[&Value]| -> Vec<u8> {
        let mut out = Vec::new();
        for line in lines {
            let mut line = (*line).clone();
            if line.get("inherited").is_none() {
                line["inherited"] = Value::String(recovered.source.clone());
            }
            out.extend(line.to_string().into_bytes());
            out.push(b'\n');
        }
        out
    };
    if !recovered.messages.is_empty() {
        writer.write_durable(MESSAGES, &inherited(&recovered.messages.iter().collect::<Vec<_>>()))?;
    }
    if !recovered.ledger.is_empty() {
        writer.write_durable(BUDGET, &inherited(&recovered.ledger.iter().collect::<Vec<_>>()))?;
    }
    let mut events: Vec<&Value> = recovered.events.iter().collect();
    // A pick after the boundary is kept, so a later continuation has it too.
    if let Some((index, pick)) = &recovered.pick
        && *index >= recovered.checkpoint.events
    {
        events.push(pick);
    }
    if !events.is_empty() {
        writer.write_durable(EVENTS, &inherited(&events))?;
    }
    let mut checkpoint = recovered.checkpoint.clone();
    checkpoint.events = events.len();
    writer.write_durable(CHECKPOINT, &serde_json::to_vec_pretty(&checkpoint)?)
}
