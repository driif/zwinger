//! Task runs: two agents of a project plan, agree, build and review a task,
//! with the duo stages. One tokio task per run is the only writer of its
//! round; the user controls it through a channel.
//!
//! The agents never talk directly. The run is the broker: each reply becomes
//! a message of the round, and the next stage's prompt includes it.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};

use crate::agent::cli::{iso, now_ms};
use crate::cage::Task;
use crate::error::{Error, Result};
use crate::runtime::SandboxRuntime;
use crate::task_agents::{self, Access, Exec, Failure, Limits, Reply, SnapshotError};
use crate::task_artifacts::{RoundWriter, TURNS};

const PLAN: &str = include_str!("task_runs/prompts/plan.md");
const EXCHANGE: &str = include_str!("task_runs/prompts/exchange.md");
const TIEBREAK: &str = include_str!("task_runs/prompts/tiebreak.md");
const IMPLEMENT: &str = include_str!("task_runs/prompts/implement.md");
const CROSSREVIEW: &str = include_str!("task_runs/prompts/crossreview.md");
const REVISE: &str = include_str!("task_runs/prompts/revise.md");
const FINAL: &str = include_str!("task_runs/prompts/final.md");
/// The live records of the active stage that a late room still gets.
const RECENT: usize = 2000;
const LIVE_QUEUE: usize = 1024;
const MESSAGE_LIMIT: usize = 16 << 10;
/// The user's messages and answers of a run, by count and by bytes.
const MESSAGES_COUNT: usize = 32;
const MESSAGES_BYTES: usize = 64 << 10;
/// The transcript in a prompt keeps its newest part within this size.
const TRANSCRIPT_LIMIT: usize = 48 << 10;
const PROMPT_LIMIT: usize = 256 << 10;

/// What the user can do in the room of an in-app run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Control {
    /// A message that every following stage prompt includes.
    Message {
        text: String,
    },
    /// Pauses after the current stage.
    Pause,
    Resume,
    /// Overrides the exchange, until the implementation starts.
    PickImplementer {
        agent: String,
    },
    /// Answers the open questions of a waiting run.
    Answer {
        text: String,
    },
    Cancel {
        /// Set by the app, as for a cage stop. The user's cancel has none.
        #[serde(skip)]
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RunState {
    Running,
    Paused,
    /// Waits for the answers to open questions.
    Waiting,
    Done,
    Failed,
    Cancelled,
}

impl RunState {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Paused | Self::Waiting)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: RunState,
    /// The id of the active stage, as the room names it.
    pub stage: Option<String>,
    /// A pause that starts at the next stage boundary.
    pub pausing: bool,
    pub picked: Option<String>,
    /// The agents that the user can pick: the ones with a plan.
    pub pickable: Vec<String>,
    /// The implementer is fixed: a pick does not count any more.
    pub decided: bool,
    pub ship: Option<bool>,
    /// The commit at the start of the run, for the diff of the change.
    #[serde(skip)]
    pub base: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Exchange turns before a tiebreak.
    pub turns: u32,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self { turns: TURNS, limits: Limits { idle: Duration::from_secs(600), hard: Duration::from_secs(1800) } }
    }
}

/// A record of the active stage, from one agent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRecord {
    pub agent: String,
    pub stage: String,
    pub record: Value,
}

struct Live {
    tx: broadcast::Sender<LiveRecord>,
    recent: Mutex<VecDeque<LiveRecord>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Live {
    fn send(&self, agent: &str, stage: &str, record: Value) {
        let record = LiveRecord { agent: agent.to_owned(), stage: stage.to_owned(), record };
        let mut recent = lock(&self.recent);
        if recent.len() == RECENT {
            recent.pop_front();
        }
        recent.push_back(record.clone());
        let _ = self.tx.send(record);
    }
}

/// The cancel of a run. The handle sets it at once; each step of the run
/// watches the flag.
struct Cancel {
    flag: watch::Sender<bool>,
    /// Why the run was cancelled. The first reason wins.
    reason: Mutex<Option<String>>,
}

impl Cancel {
    fn set(&self, reason: Option<String>) {
        let mut slot = lock(&self.reason);
        if slot.is_none() {
            *slot = Some(reason.unwrap_or_else(|| "cancelled by the user".to_owned()));
            self.flag.send_replace(true);
        }
    }

    fn is_set(&self) -> bool {
        *self.flag.borrow()
    }
}

/// An in-app run, as the commands see it.
pub struct RunHandle {
    pub project_id: String,
    pub task_id: String,
    pub run: String,
    pub agents: [String; 2],
    control: mpsc::UnboundedSender<Control>,
    status: watch::Receiver<Status>,
    live: Arc<Live>,
    cancel: Arc<Cancel>,
    /// The count and the bytes of the user's messages so far.
    messages: Mutex<(usize, usize)>,
}

impl RunHandle {
    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// Refuses a control that the run cannot do in its state.
    pub fn control(&self, control: Control) -> Result<()> {
        let status = self.status();
        if !status.state.is_live() {
            return Err(Error::RunState("the run has ended"));
        }
        match &control {
            Control::Message { text } | Control::Answer { text }
                if text.trim().is_empty() || text.len() > MESSAGE_LIMIT =>
            {
                return Err(Error::RunState("the message must have 1 to 16384 bytes"));
            }
            Control::Answer { .. } if status.state != RunState::Waiting => {
                return Err(Error::RunState("the run has no open questions"));
            }
            Control::PickImplementer { .. } if status.decided => {
                return Err(Error::RunState("the implementation has started"));
            }
            Control::PickImplementer { agent } if !self.agents.contains(agent) => {
                return Err(Error::RunState("the agent is not in this run"));
            }
            Control::PickImplementer { agent } if !status.pickable.contains(agent) => {
                return Err(Error::RunState("only an agent with a plan can implement"));
            }
            Control::Message { text } | Control::Answer { text } => {
                let mut sent = lock(&self.messages);
                if sent.0 == MESSAGES_COUNT || sent.1 + text.len() > MESSAGES_BYTES {
                    return Err(Error::RunState("the run has the limit of 32 messages and 64 KiB"));
                }
                *sent = (sent.0 + 1, sent.1 + text.len());
            }
            Control::Cancel { reason } => self.cancel.set(reason.clone()),
            _ => {}
        }
        // A cancel also goes to the run, so a paused or waiting run wakes up.
        self.control.send(control).map_err(|_| Error::RunState("the run has ended"))
    }

    /// The records of the active stage so far, and the ones that follow.
    pub fn subscribe(&self) -> (Vec<LiveRecord>, broadcast::Receiver<LiveRecord>) {
        let recent = lock(&self.live.recent);
        (recent.iter().cloned().collect(), self.live.tx.subscribe())
    }

    pub async fn settled(&self) {
        let _ = self.status.clone().wait_for(|s| !s.state.is_live()).await;
    }
}

/// The in-app runs of this app session.
#[derive(Default)]
pub struct Registry {
    runs: Mutex<Vec<Arc<RunHandle>>>,
}

/// What a run needs to start.
pub struct Spec<R: SandboxRuntime> {
    pub project_id: String,
    pub task: Task,
    pub run: String,
    pub exec: Exec<R>,
    pub writer: RoundWriter,
    pub config: Config,
}

impl Registry {
    pub fn find(&self, project_id: &str, task_id: &str, run: &str) -> Option<Arc<RunHandle>> {
        lock(&self.runs).iter().find(|r| r.project_id == project_id && r.task_id == task_id && r.run == run).cloned()
    }

    pub fn is_running(&self, project_id: &str, task_id: &str) -> bool {
        lock(&self.runs)
            .iter()
            .any(|r| r.project_id == project_id && r.task_id == task_id && r.status().state.is_live())
    }

    pub fn start<R: SandboxRuntime>(&self, spec: Spec<R>) -> Arc<RunHandle> {
        let names = spec.task.agents.clone().map(|a| a.kind.name().to_owned());
        let (control, control_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(Status {
            state: RunState::Running,
            stage: None,
            pausing: false,
            picked: None,
            pickable: Vec::new(),
            decided: false,
            ship: None,
            base: None,
        });
        let live = Arc::new(Live { tx: broadcast::channel(LIVE_QUEUE).0, recent: Mutex::new(VecDeque::new()) });
        let handle = Arc::new(RunHandle {
            project_id: spec.project_id.clone(),
            task_id: spec.task.id.clone(),
            run: spec.run.clone(),
            agents: names.clone(),
            control,
            status,
            live: live.clone(),
            cancel: Arc::new(Cancel { flag: watch::channel(false).0, reason: Mutex::new(None) }),
            messages: Mutex::new((0, 0)),
        });
        let actor = Actor {
            io: Io {
                exec: spec.exec,
                task: spec.task,
                names,
                config: spec.config,
                cancel: handle.cancel.flag.subscribe(),
                live,
            },
            book: Book {
                writer: spec.writer,
                status: status_tx,
                cancel: handle.cancel.clone(),
                transcript: Vec::new(),
            },
            control: control_rx,
            run: Default::default(),
        };
        tokio::spawn(actor.run());
        let mut runs = lock(&self.runs);
        runs.retain(|r| !(r.project_id == handle.project_id && r.task_id == handle.task_id && r.run == handle.run));
        runs.push(handle.clone());
        handle
    }

    /// Cancels the live runs of a project and waits up to `limit` until each
    /// one has written its manifest.
    pub async fn cancel_project(&self, project_id: &str, reason: &str, limit: Duration) {
        let runs: Vec<Arc<RunHandle>> = lock(&self.runs)
            .iter()
            .filter(|r| r.project_id == project_id && r.status().state.is_live())
            .cloned()
            .collect();
        for run in &runs {
            run.cancel.set(Some(reason.to_owned()));
            let _ = run.control.send(Control::Cancel { reason: None });
        }
        let _ = tokio::time::timeout(limit, async {
            for run in &runs {
                run.settled().await;
            }
        })
        .await;
    }

    /// Forgets the runs of a project or of one of its tasks.
    pub fn forget(&self, project_id: &str, task_id: Option<&str>) {
        lock(&self.runs).retain(|r| r.project_id != project_id || task_id.is_some_and(|t| r.task_id != t));
    }
}

/// The parts of the run that the stage calls read.
struct Io<R: SandboxRuntime> {
    exec: Exec<R>,
    task: Task,
    names: [String; 2],
    config: Config,
    cancel: watch::Receiver<bool>,
    live: Arc<Live>,
}

/// The parts of the run that control messages change.
struct Book {
    writer: RoundWriter,
    status: watch::Sender<Status>,
    cancel: Arc<Cancel>,
    /// The brokered messages of the round, in order.
    transcript: Vec<Entry>,
}

/// One brokered message: `{from, to, kind, body}`, so that a bridge such as
/// A2A can expose it later.
struct Entry {
    from: String,
    to: String,
    /// `plan`, `reply`, `tiebreak`, `user` or `answer`.
    kind: &'static str,
    stage: String,
    /// The end of its heading, such as `opening plan` or `turn 2`.
    title: String,
    /// Markdown.
    body: String,
}

impl Entry {
    fn is_user(&self) -> bool {
        self.from == "user"
    }
}

/// What the run has decided so far, for the manifest.
#[derive(Default)]
struct Round {
    planned: Vec<usize>,
    implementer: Option<usize>,
    verdict: Option<String>,
    finals: Vec<(usize, Value)>,
    backlog: Vec<String>,
    violations: Vec<String>,
    /// `Some(false)` once a check found that it cannot see changes.
    detection: Option<bool>,
    /// `HEAD` before the first stage that can write.
    base: Option<String>,
}

struct Actor<R: SandboxRuntime> {
    io: Io<R>,
    book: Book,
    control: mpsc::UnboundedReceiver<Control>,
    run: Round,
}

/// The run ends early.
enum Stop {
    Cancelled,
    Failed(String),
}

fn compose(template: &str, pairs: &[(&str, &str)]) -> String {
    pairs.iter().fold(template.to_owned(), |text, (key, value)| text.replace(&format!("{{{{{key}}}}}"), value))
}

/// The newest part of `text` within `limit` bytes, with a line that says
/// what is omitted.
fn newest_within(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let kept = task_agents::tail(text, limit.saturating_sub(64));
    format!("_[The earlier {} bytes are omitted.]_\n\n{kept}", text.len() - kept.len())
}

/// The start of `text` within `limit` bytes, with a line that says what is omitted.
fn start_within(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let kept = task_agents::head(text, limit.saturating_sub(64));
    format!("{kept}\n\n_[The next {} bytes are omitted.]_", text.len() - kept.len())
}

/// A stage prompt of at most `PROMPT_LIMIT` bytes. Over the limit, the
/// largest part gives up bytes: the transcript its oldest part, the other
/// parts their end. The user's messages never give up bytes.
fn fit(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut parts: Vec<(&str, String)> = pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect();
    loop {
        let borrowed: Vec<(&str, &str)> = parts.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let prompt = compose(template, &borrowed);
        let excess = prompt.len().saturating_sub(PROMPT_LIMIT);
        let Some((key, value)) = parts.iter_mut().filter(|(k, _)| *k != "MESSAGES").max_by_key(|(_, v)| v.len()) else {
            return prompt;
        };
        // A part of a few bytes cannot give more: then the prompt stays as it is.
        if excess == 0 || value.len() <= 128 {
            return prompt;
        }
        let limit = value.len().saturating_sub(excess);
        *value = if *key == "TRANSCRIPT" { newest_within(value, limit) } else { start_within(value, limit) };
    }
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

impl Book {
    fn set(&self, change: impl FnOnce(&mut Status)) {
        self.status.send_modify(change);
    }

    fn stage_id(&self) -> String {
        self.status.borrow().stage.clone().unwrap_or_else(|| "plan".to_owned())
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_set()
    }

    /// Adds entries to the transcript and writes it.
    fn add(&mut self, entries: impl IntoIterator<Item = Entry>) {
        self.transcript.extend(entries);
        self.writer.write("transcript.md", render(self.transcript.iter()).as_bytes());
    }

    fn message(&mut self, kind: &'static str, text: String) {
        let stage = self.stage_id();
        self.writer
            .append("messages.jsonl", &json!({ "at": iso(now_ms()), "stage": stage, "kind": kind, "body": text }));
        let label = if kind == "answer" { "answer" } else { "message" };
        self.add([Entry {
            from: "user".to_owned(),
            to: "all".to_owned(),
            kind,
            title: format!("{label}, during {stage}"),
            body: text,
            stage,
        }]);
    }

    /// Applies one control. Returns true for an answer.
    fn handle(&mut self, control: Control, names: &[String; 2]) -> bool {
        match control {
            Control::Message { text } => self.message("user", text),
            Control::Pause => self.set(|s| s.pausing = true),
            Control::Resume => self.set(|s| s.pausing = false),
            Control::PickImplementer { agent } => {
                let status = self.status.borrow();
                let allowed = names.contains(&agent) && status.pickable.contains(&agent) && !status.decided;
                drop(status);
                if allowed {
                    self.writer.event("pick", &format!("user picked {agent}"));
                    self.set(|s| s.picked = Some(agent));
                }
            }
            Control::Answer { text } => {
                let waiting = self.status.borrow().state == RunState::Waiting;
                self.message(if waiting { "answer" } else { "user" }, text);
                return waiting;
            }
            Control::Cancel { reason } => self.cancel.set(reason),
        }
        false
    }
}

/// Runs `work` and applies the controls that arrive meanwhile.
async fn during<T>(
    book: &mut Book,
    control: &mut mpsc::UnboundedReceiver<Control>,
    names: &[String; 2],
    work: impl Future<Output = T>,
) -> T {
    tokio::pin!(work);
    loop {
        tokio::select! {
            out = &mut work => return out,
            Some(c) = control.recv() => {
                book.handle(c, names);
            }
        }
    }
}

impl<R: SandboxRuntime> Io<R> {
    fn name(&self, i: usize) -> &str {
        &self.names[i]
    }

    async fn call(&self, i: usize, stage: &str, access: Access, prompt: &str) -> Reply {
        let name = self.name(i).to_owned();
        let live = |record: Value| self.live.send(&name, stage, record);
        let mut cancel = self.cancel.clone();
        let mut reply =
            self.exec.call_json(&self.task.agents[i], access, prompt, self.config.limits, &mut cancel, &live).await;
        // A process that ends because of a cancel, such as with a stopped VM, is no degraded agent.
        if reply.failure.is_some() && *self.cancel.borrow() {
            reply.failure = Some(Failure::Cancelled);
        }
        reply
    }

    async fn both(&self, stage: &str, prompts: &[String; 2]) -> [Reply; 2] {
        let (a, b) = tokio::join!(
            self.call(0, stage, Access::Read, &prompts[0]),
            self.call(1, stage, Access::Read, &prompts[1]),
        );
        [a, b]
    }

    async fn snapshot(&self) -> std::result::Result<task_agents::Snapshot, SnapshotError> {
        self.exec.snapshot(&mut self.cancel.clone()).await
    }

    /// The change since `base`, for the review prompts.
    async fn change(&self, base: Option<&str>) -> String {
        match self.exec.diff(base, &mut self.cancel.clone()).await {
            Some(diff) => {
                let since = base.map_or("Only the uncommitted change is known.".to_owned(), |b| {
                    format!("The run started at commit `{b}`.")
                });
                format!("{since} Read it again with `git log` and `git diff`.\n\n````diff\n{diff}\n````")
            }
            None => "The working dir is not a git work tree, so the run cannot show the change.".to_owned(),
        }
    }
}

impl<R: SandboxRuntime> Actor<R> {
    async fn run(mut self) {
        let end = self.drive().await;
        self.settle(end);
    }

    fn name(&self, i: usize) -> String {
        self.io.names[i].clone()
    }

    fn peer(i: usize) -> usize {
        1 - i
    }

    /// The task and the working dir, for each prompt.
    fn context(&self) -> String {
        let task = &self.io.task;
        format!(
            "## Task: {}\n\n{}\n\n## Working dir\n\n`{}` — run all commands from this directory. Read `AGENTS.md` or \
             `CLAUDE.md` there first, if one exists: the project conventions have priority over your preferences.",
            task.title, task.request, task.workdir
        )
    }

    /// The user's entries of the transcript, for each prompt. They are a
    /// separate part, so neither the transcript bound nor `fit` drops them;
    /// the step-in cap bounds them.
    fn messages(&self) -> String {
        let mut user = self.book.transcript.iter().filter(|e| e.is_user()).peekable();
        if user.peek().is_none() {
            return String::new();
        }
        let mut text = "\n\n## Messages from the user\n\nThey have priority over the plan.\n".to_owned();
        for entry in user {
            let label = if entry.kind == "answer" { "answer" } else { "message" };
            text.push_str(&format!("\n- ({label}, during {}) {}", entry.stage, entry.body));
        }
        text
    }

    /// The transcript of the agents, for a prompt.
    fn transcript(&self) -> String {
        newest_within(&render(self.book.transcript.iter().filter(|e| !e.is_user())), TRANSCRIPT_LIMIT)
    }

    fn agents_choice(&self) -> String {
        format!("`{}` or `{}`", self.io.names[0], self.io.names[1])
    }

    fn gate_list(&self) -> String {
        match self.io.task.gates.as_slice() {
            [] => "none".to_owned(),
            gates => gates.iter().map(|g| format!("`{g}`")).collect::<Vec<_>>().join(", "),
        }
    }

    /// Starts a stage: its event, its status, and a clean live stream.
    fn stage(&mut self, detail: &str, id: &str) -> std::result::Result<(), Stop> {
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        self.book.writer.event("stage", detail);
        lock(&self.io.live.recent).clear();
        self.book.set(|s| s.stage = Some(id.to_owned()));
        Ok(())
    }

    /// Between stages: the mirror gets the new files, then a cancel or a
    /// pause takes effect.
    async fn boundary(&mut self) -> std::result::Result<(), Stop> {
        self.book.writer.sync_mirror();
        if let Some(why) = self.book.writer.failure() {
            return Err(Stop::Failed(why));
        }
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        if !self.book.status.borrow().pausing {
            return Ok(());
        }
        self.book.writer.event("paused", &format!("after {}", self.book.stage_id()));
        self.book.set(|s| s.state = RunState::Paused);
        self.book.writer.sync_mirror();
        while self.book.status.borrow().pausing && !self.book.cancelled() {
            let Some(control) = self.control.recv().await else {
                return Err(Stop::Cancelled);
            };
            self.book.handle(control, &self.io.names);
        }
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        self.book.writer.event("resumed", "by the user");
        self.book.set(|s| s.state = RunState::Running);
        Ok(())
    }

    /// Waits until the user answers the open questions.
    async fn wait_answers(&mut self, questions: &[String]) -> std::result::Result<(), Stop> {
        self.book.writer.write("open-questions.txt", questions.join("\n").as_bytes());
        self.book.writer.event("waiting", &format!("open questions: {}", questions.len()));
        self.book.set(|s| s.state = RunState::Waiting);
        self.book.writer.sync_mirror();
        loop {
            let Some(control) = self.control.recv().await else {
                return Err(Stop::Cancelled);
            };
            if self.book.handle(control, &self.io.names) {
                break;
            }
            if self.book.cancelled() {
                return Err(Stop::Cancelled);
            }
        }
        self.book.writer.event("answered", "by the user");
        self.book.set(|s| s.state = RunState::Running);
        Ok(())
    }

    /// Writes the raw reply and its JSON, or records the failure. Returns the JSON.
    fn record(&mut self, i: usize, stage: &str, base: &str, reply: Reply) -> Option<Value> {
        let name = self.name(i);
        if !reply.raw.is_empty() {
            self.book.writer.write(&format!("{base}.raw"), reply.raw.as_bytes());
        }
        if reply.retried {
            self.book.writer.event("retry", &format!("{name} {stage}: the reply had no JSON, asked once more"));
        }
        if let Some(failure) = &reply.failure {
            if *failure != Failure::Cancelled {
                self.book.writer.event(failure.kind(), &format!("{name} {stage} {failure}"));
            }
            return None;
        }
        let json = reply.json?;
        self.book.writer.write_json(&format!("{base}.json"), &json);
        for item in strings(&json["backlog"]) {
            if !self.run.backlog.iter().any(|b| b.eq_ignore_ascii_case(&item)) {
                self.run.backlog.push(item);
            }
        }
        Some(json)
    }

    /// Runs read-only work and reports a change of the repository that the
    /// check saw meanwhile. It does not know who made the change.
    async fn read_only<T>(&mut self, stage: &str, work: impl AsyncFnOnce(&Io<R>) -> T) -> T {
        let before = self.io.snapshot().await;
        if let Ok(snapshot) = &before
            && self.run.base.is_none()
            && !self.book.status.borrow().decided
        {
            self.run.base = Some(snapshot.head.clone());
            let base = self.run.base.clone();
            self.book.set(|s| s.base = base);
        }
        let out = during(&mut self.book, &mut self.control, &self.io.names, work(&self.io)).await;
        let check = match before {
            Ok(before) => match self.io.snapshot().await {
                Ok(after) => Ok((before, after)),
                Err(SnapshotError::NotGit) => Err(SnapshotError::Failed("the repository is gone".to_owned())),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        if self.book.cancelled() {
            return out;
        }
        match check {
            Ok((before, after)) => {
                self.run.detection = Some(true);
                if before.hash != after.hash {
                    let mut changed: Vec<&str> =
                        after.status.lines().filter(|l| !before.status.lines().any(|b| b == *l)).collect();
                    changed.extend(before.status.lines().filter(|l| !after.status.lines().any(|a| a == *l)));
                    let files = if changed.is_empty() { "file contents".to_owned() } else { changed.join(", ") };
                    self.violation(format!("the repository changed during the read-only stage {stage}: {files}"));
                }
            }
            Err(SnapshotError::Failed(why)) => {
                self.violation(format!("the read-only check of the stage {stage} failed: {why}"))
            }
            Err(SnapshotError::NotGit) if self.run.detection.is_none() => {
                self.run.detection = Some(false);
                self.book.writer.event(
                    "no_detection",
                    "the working dir is not a git work tree, so changes in read-only stages cannot be found",
                );
            }
            Err(SnapshotError::NotGit) => {}
        }
        out
    }

    fn violation(&mut self, detail: String) {
        self.book.writer.event("violation", &detail);
        self.run.violations.push(detail);
    }

    async fn drive(&mut self) -> std::result::Result<(), Stop> {
        let a = self.name(0);
        self.book.writer.write("task.md", format!("# {}\n\n{}\n", self.io.task.title, self.io.task.request).as_bytes());

        // 1. Blind plans, in parallel.
        self.stage("plan", "plan")?;
        let prompts = [0, 1].map(|i| {
            fit(
                PLAN,
                &[
                    ("ME", &self.name(i)),
                    ("PEER", &self.name(Self::peer(i))),
                    ("AGENTS", &self.agents_choice()),
                    ("CONTEXT", &self.context()),
                    ("MESSAGES", &self.messages()),
                ],
            )
        });
        for (name, prompt) in self.io.names.iter().zip(&prompts) {
            self.book.writer.write(&format!("turns/plan-{name}.prompt.md"), prompt.as_bytes());
        }
        let replies = self.read_only("plan", async |io: &Io<R>| io.both("plan", &prompts).await).await;
        let mut plans: [Option<Value>; 2] = [None, None];
        for (i, reply) in replies.into_iter().enumerate() {
            let name = self.name(i);
            plans[i] = self.record(i, "plan", &format!("turns/plan-{name}"), reply);
            if plans[i].is_some() {
                self.book.writer.event("agent_done", &format!("{name} planned"));
                self.run.planned.push(i);
            } else if !self.book.cancelled() {
                self.book.writer.event("agent_degraded", &format!("{name} plan failed"));
            }
        }
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        if self.run.planned.is_empty() {
            return Err(Stop::Failed("both agents failed to plan".to_owned()));
        }
        let pickable: Vec<String> = self.run.planned.iter().map(|&i| self.name(i)).collect();
        self.book.set(|s| s.pickable = pickable);
        self.book.add(opening(&self.io.names, &plans));
        self.boundary().await?;

        // 2. Exchange, 3. tiebreak.
        let mut turns: Vec<[Option<Value>; 2]> = Vec::new();
        let mut tiebreak = None;
        let mut implementer = None;
        if let [only] = self.run.planned[..] {
            // A walkover: nothing was exchanged, so there is no agreement.
            implementer = Some(only);
            self.book.writer.event("walkover", &format!("{} proceeds unopposed", self.name(only)));
        } else {
            for turn in 1..=self.io.config.turns {
                if self.picked().is_some() {
                    break;
                }
                self.stage(&format!("exchange turn {turn}"), &format!("exchange-{turn}"))?;
                let prompts = [0, 1].map(|i| {
                    fit(
                        EXCHANGE,
                        &[
                            ("ME", &self.name(i)),
                            ("PEER", &self.name(Self::peer(i))),
                            ("AGENTS", &self.agents_choice()),
                            ("CONTEXT", &self.context()),
                            ("MESSAGES", &self.messages()),
                            ("TRANSCRIPT", &self.transcript()),
                        ],
                    )
                });
                for (name, prompt) in self.io.names.iter().zip(&prompts) {
                    self.book.writer.write(&format!("turns/t{turn}-{name}.prompt.md"), prompt.as_bytes());
                }
                let stage = format!("exchange-{turn}");
                let replies = self.read_only(&stage, async |io: &Io<R>| io.both(&stage, &prompts).await).await;
                let mut replied: [Option<Value>; 2] = [None, None];
                for (i, reply) in replies.into_iter().enumerate() {
                    let name = self.name(i);
                    replied[i] = self.record(i, &format!("turn {turn}"), &format!("turns/t{turn}-{name}"), reply);
                    if replied[i].is_none() && !self.book.cancelled() {
                        self.book.writer.event("agent_degraded", &format!("{name} turn {turn} returned no JSON"));
                    }
                }
                if self.book.cancelled() {
                    return Err(Stop::Cancelled);
                }
                self.book.add(turn_entries(&self.io.names, turn, &replied));
                let converged = converged(&self.io.names, &replied);
                turns.push(replied);
                if let Some(pick) = converged {
                    self.book.writer.event("converged", &format!("turn {turn} on {}", self.name(pick)));
                    implementer = Some(pick);
                    break;
                }
                self.book.writer.event("no_agreement", &format!("after turn {turn}"));
                self.boundary().await?;
            }
            if implementer.is_none() && self.picked().is_none() {
                self.stage(&format!("tiebreak by {a}"), "tiebreak")?;
                let prompt = fit(
                    TIEBREAK,
                    &[
                        ("AGENTS", &self.agents_choice()),
                        ("CONTEXT", &self.context()),
                        ("MESSAGES", &self.messages()),
                        ("TRANSCRIPT", &self.transcript()),
                    ],
                );
                self.book.writer.write("tiebreak.prompt.md", prompt.as_bytes());
                let reply = self
                    .read_only("tiebreak", async |io: &Io<R>| io.call(0, "tiebreak", Access::Read, &prompt).await)
                    .await;
                let Some(json) = self.record(0, "tiebreak", "tiebreak", reply) else {
                    return Err(if self.book.cancelled() {
                        Stop::Cancelled
                    } else {
                        Stop::Failed("the tiebreak produced nothing".to_owned())
                    });
                };
                let Some(pick) = self.io.names.iter().position(|n| *n == text(&json["implementer"])) else {
                    return Err(Stop::Failed("the tiebreak named no agent of this run".to_owned()));
                };
                self.book.writer.event("decision", &format!("tiebreak chose {}", self.name(pick)));
                self.book.add([Entry {
                    from: a.clone(),
                    to: "all".to_owned(),
                    kind: "tiebreak",
                    stage: "tiebreak".to_owned(),
                    title: "tiebreak".to_owned(),
                    body: format!("**Implementer:** {}\n\n{}", self.name(pick), text(&json["why"])),
                }]);
                implementer = Some(pick);
                tiebreak = Some(json);
            }
        }
        // The user's pick overrides the exchange.
        if let Some(pick) = self.picked() {
            self.book.writer.event("decision", &format!("user chose {}", self.name(pick)));
            implementer = Some(pick);
        }
        let implementer = implementer.expect("an implementer");
        self.run.implementer = Some(implementer);
        self.book.set(|s| s.decided = true);
        let reviewer = Self::peer(implementer);
        let (me, peer) = (self.name(implementer), self.name(reviewer));

        let plan = match tiebreak.as_ref().map(|t| text(&t["plan"])).filter(|p| !p.is_empty()) {
            Some(plan) => plan,
            None => latest_plan(implementer, &plans, &turns),
        };
        self.book.writer.write("plan.md", plan.as_bytes());
        self.book.writer.event("decision", &format!("{me} implements"));
        let questions = unknowns(tiebreak.as_ref(), &plans, &turns);
        if !questions.is_empty() {
            self.wait_answers(&questions).await?;
        }
        self.boundary().await?;

        // 4. Implement.
        self.stage("implement", "implement")?;
        let prompt = fit(
            IMPLEMENT,
            &[
                ("ME", &me),
                ("CONTEXT", &self.context()),
                ("MESSAGES", &self.messages()),
                ("PLAN", &plan),
                ("GATES", &self.gate_list()),
                ("TRANSCRIPT", &self.transcript()),
            ],
        );
        self.book.writer.write("implement.prompt.md", prompt.as_bytes());
        let io = &self.io;
        let reply = during(
            &mut self.book,
            &mut self.control,
            &io.names,
            io.call(implementer, "implement", Access::Write, &prompt),
        )
        .await;
        let result = self.record(implementer, "implement", "implementation", reply);
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        if result.is_none() {
            self.book.writer.event("agent_degraded", "implementer returned no structured result");
        }
        self.book.writer.event("agent_done", &format!("implement by {me}"));
        self.boundary().await?;

        // 5. Gates.
        self.stage("gates", "gates")?;
        let gates = self.io.task.gates.clone();
        let mut results = Vec::new();
        for gate in &gates {
            let io = &self.io;
            let mut cancel = io.cancel.clone();
            let ran =
                during(&mut self.book, &mut self.control, &io.names, io.exec.sh(gate, io.config.limits, &mut cancel))
                    .await;
            let Ok(shell) = ran else {
                return Err(Stop::Cancelled);
            };
            results.push(json!({ "command": gate, "code": shell.code, "output": shell.output }));
        }
        let passed = results.iter().filter(|r| r["code"] == 0).count();
        let gates_json = Value::Array(results);
        self.book.writer.write_json("gates.json", &gates_json);
        self.book.writer.event("gates", &format!("{passed} of {} passed", gates.len()));
        let gates_text = if gates.is_empty() {
            "The task has no gates.".to_owned()
        } else {
            format!("```json\n{}\n```", serde_json::to_string_pretty(&gates_json).unwrap_or_default())
        };
        self.boundary().await?;

        // 6. Cross review.
        self.stage(&format!("crossreview by {peer}"), "crossreview")?;
        let result_text = result.as_ref().map_or("The implementer returned no report.".to_owned(), |r| {
            format!("```json\n{}\n```", serde_json::to_string_pretty(r).unwrap_or_default())
        });
        let base = self.run.base.clone();
        let change = self.io.change(base.as_deref()).await;
        let prompt = fit(
            CROSSREVIEW,
            &[
                ("ME", &peer),
                ("PEER", &me),
                ("CONTEXT", &self.context()),
                ("MESSAGES", &self.messages()),
                ("PLAN", &plan),
                ("RESULT", &result_text),
                ("GATES", &gates_text),
                ("TRANSCRIPT", &self.transcript()),
                ("CHANGE", &change),
            ],
        );
        self.book.writer.write("crossreview.prompt.md", prompt.as_bytes());
        let reply = self
            .read_only("crossreview", async |io: &Io<R>| io.call(reviewer, "crossreview", Access::Read, &prompt).await)
            .await;
        let review = self.record(reviewer, "crossreview", "crossreview", reply);
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        // A missing review is no acceptance: the verdict stays open.
        let verdict = match &review {
            Some(r) if r["verdict"] == "revise" => "revise",
            Some(_) => "accept",
            None => {
                self.book.writer.event("agent_degraded", &format!("{peer} returned no structured review"));
                "missing"
            }
        };
        self.run.verdict = Some(verdict.to_owned());
        self.book.writer.event("crossreview", verdict);
        self.boundary().await?;

        // 7. Revise, once.
        let mut revision = None;
        let mut revised = false;
        if verdict == "revise" {
            self.stage("revise", "revise")?;
            let review_text = format!("```json\n{}\n```", serde_json::to_string_pretty(&review).unwrap_or_default());
            let prompt = fit(
                REVISE,
                &[
                    ("ME", &me),
                    ("PEER", &peer),
                    ("CONTEXT", &self.context()),
                    ("MESSAGES", &self.messages()),
                    ("PLAN", &plan),
                    ("REVIEW", &review_text),
                    ("GATES", &self.gate_list()),
                    ("TRANSCRIPT", &self.transcript()),
                ],
            );
            self.book.writer.write("revise.prompt.md", prompt.as_bytes());
            let io = &self.io;
            let reply = during(
                &mut self.book,
                &mut self.control,
                &io.names,
                io.call(implementer, "revise", Access::Write, &prompt),
            )
            .await;
            revision = self.record(implementer, "revise", "revision", reply);
            revised = true;
            if self.book.cancelled() {
                return Err(Stop::Cancelled);
            }
            if revision.is_none() {
                self.book.writer.event("agent_degraded", "revision returned no structured result");
            }
            self.book.writer.event("agent_done", &format!("revised by {me}"));
            self.boundary().await?;
        }

        // 8. Blind final reviews, in parallel.
        self.stage("final review", "final")?;
        let history = history(review.as_ref(), revision.as_ref());
        self.book.writer.write("history.md", history.as_bytes());
        // A revision that returned no JSON can still have changed files.
        let change = if revised { self.io.change(base.as_deref()).await } else { change };
        let prompts = [0, 1].map(|i| {
            fit(
                FINAL,
                &[
                    ("ME", &self.name(i)),
                    ("PEER", &self.name(Self::peer(i))),
                    ("CONTEXT", &self.context()),
                    ("MESSAGES", &self.messages()),
                    ("PLAN", &plan),
                    ("HISTORY", &history),
                    ("GATES", &gates_text),
                    ("TRANSCRIPT", &self.transcript()),
                    ("CHANGE", &change),
                ],
            )
        });
        let replies = self.read_only("final", async |io: &Io<R>| io.both("final", &prompts).await).await;
        for (i, reply) in replies.into_iter().enumerate() {
            let name = self.name(i);
            match self.record(i, "final review", &format!("final-{name}"), reply) {
                Some(json) => self.run.finals.push((i, json)),
                None if !self.book.cancelled() => {
                    self.book.writer.event("agent_degraded", &format!("{name} final review returned no JSON"));
                }
                None => {}
            }
        }
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        Ok(())
    }

    /// The user's pick, if it is an agent with a plan.
    fn picked(&self) -> Option<usize> {
        let picked = self.book.status.borrow().picked.clone()?;
        self.io.names.iter().position(|n| *n == picked).filter(|i| self.run.planned.contains(i))
    }

    /// A run whose record has a failed write fails: its round is not complete.
    fn written(&self, end: std::result::Result<(), Stop>) -> std::result::Result<(), Stop> {
        match (end, self.book.writer.failure()) {
            (Ok(()), Some(why)) => Err(Stop::Failed(why)),
            (end, _) => end,
        }
    }

    /// The one end of the run: the manifest, the last event and the state.
    fn settle(mut self, end: std::result::Result<(), Stop>) {
        let end = self.written(end);
        let stopped = match &end {
            Ok(()) => None,
            Err(Stop::Cancelled) => {
                Some(lock(&self.book.cancel.reason).clone().unwrap_or_else(|| "cancelled".to_owned()))
            }
            Err(Stop::Failed(why)) => Some(why.clone()),
        };
        let manifest = self.manifest(stopped.as_deref());
        self.book.writer.write_json("manifest.json", &manifest);
        let end = self.written(end);
        let ship = end.is_ok() && manifest["ship"].as_bool().unwrap_or(false);
        let state = match &end {
            Ok(()) => {
                self.book.writer.event("done", &format!("ship={ship}"));
                RunState::Done
            }
            Err(Stop::Cancelled) => {
                self.book.writer.event("cancelled", stopped.as_deref().unwrap_or_default());
                RunState::Cancelled
            }
            Err(Stop::Failed(why)) => {
                self.book.writer.event("run_failed", why);
                RunState::Failed
            }
        };
        self.book.writer.sync_mirror();
        self.book.set(|s| {
            s.state = state;
            s.stage = None;
            s.ship = Some(ship);
        });
    }

    /// As duo's manifest: `ship` only if both final reviewers set it.
    fn manifest(&self, stopped: Option<&str>) -> Value {
        let names = &self.io.names;
        let reported: Vec<&String> = self.run.finals.iter().map(|(i, _)| &names[*i]).collect();
        let mirror_error = self.book.writer.mirror_error();
        let ship = stopped.is_none()
            && mirror_error.is_none()
            && reported.len() == 2
            && self.run.finals.iter().all(|(_, f)| f["ship"] == true);
        let blocking: Vec<Value> =
            self.run.finals.iter().flat_map(|(_, f)| f["blocking"].as_array().cloned().unwrap_or_default()).collect();
        let ship_basis = if let Some(e) = mirror_error {
            format!("withheld — the harness copy failed: {e}")
        } else if reported.len() == 2 {
            "both reviewers".to_owned()
        } else {
            let who: Vec<&str> = reported.iter().map(|s| s.as_str()).collect();
            format!("withheld — only {} reviewed", if who.is_empty() { "nobody".to_owned() } else { who.join(", ") })
        };
        let task_met: serde_json::Map<String, Value> =
            self.run.finals.iter().map(|(i, f)| (names[*i].clone(), f["task_met"].clone())).collect();
        json!({
            "task": self.io.task.slug,
            "title": self.io.task.title,
            "implementer": self.run.implementer.map(|i| names[i].clone()),
            "reviewer": self.run.implementer.map(|i| names[1 - i].clone()),
            "planned": self.run.planned.iter().map(|i| names[*i].clone()).collect::<Vec<_>>(),
            "degraded": (0..2).filter(|i| !self.run.planned.contains(i)).map(|i| names[i].clone()).collect::<Vec<_>>(),
            "crossreview_verdict": self.run.verdict,
            "final_reviewers": reported,
            "ship": ship,
            "ship_basis": ship_basis,
            "blocking": blocking,
            "task_met": task_met,
            "backlog": self.run.backlog,
            "violations": self.run.violations,
            "base": self.run.base,
            "stopped": stopped,
        })
    }
}

/// The transcript as Markdown: a section per stage, a heading per entry.
fn render<'a>(entries: impl Iterator<Item = &'a Entry>) -> String {
    let mut out = String::new();
    let mut section = "";
    for entry in entries {
        if entry.stage != section {
            section = &entry.stage;
            let heading = match section.strip_prefix("exchange-") {
                Some(turn) => format!("Exchange — turn {turn}"),
                None if section == "plan" => "Opening plans (written blind)".to_owned(),
                None => format!("Stage {section}"),
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("## {heading}\n"));
        }
        let to = if entry.to == "all" { String::new() } else { format!(" (to {})", entry.to) };
        out.push_str(&format!("\n### {} — {}{to}\n\n{}\n", entry.from, entry.title, entry.body));
    }
    out
}

/// The blind plans, as the opening of the transcript.
fn opening(names: &[String; 2], plans: &[Option<Value>; 2]) -> Vec<Entry> {
    names
        .iter()
        .zip(plans)
        .map(|(name, plan)| {
            let (title, body) = match plan {
                None => ("no plan", "_This agent did not produce a plan._".to_owned()),
                Some(d) => (
                    "opening plan",
                    format!(
                        "**Plan.** {}\n\n**Claims implementer:** {} — {}\n\n**Touches:** {}\n\n**Risks:** {}",
                        text(&d["plan"]),
                        text(&d["claim"]),
                        text(&d["claim_reason"]),
                        strings(&d["touches"]).join(", "),
                        strings(&d["risks"]).join("; "),
                    ),
                ),
            };
            Entry {
                from: name.clone(),
                to: "all".to_owned(),
                kind: "plan",
                stage: "plan".to_owned(),
                title: title.to_owned(),
                body,
            }
        })
        .collect()
}

fn turn_entries(names: &[String; 2], turn: u32, replies: &[Option<Value>; 2]) -> Vec<Entry> {
    names
        .iter()
        .zip(replies)
        .enumerate()
        .map(|(i, (name, reply))| {
            let (title, body) = match reply {
                None => ("no reply".to_owned(), "_This agent did not reply this turn._".to_owned()),
                Some(d) => {
                    let conceded = strings(&d["conceded"]);
                    (
                        format!("turn {turn}"),
                        format!(
                            "{}\n\n**Implementer:** {} · **Agreed:** {} · **Conceded:** {}",
                            text(&d["message"]),
                            text(&d["implementer"]),
                            d["agreed"] == true,
                            if conceded.is_empty() { "nothing".to_owned() } else { conceded.join("; ") },
                        ),
                    )
                }
            };
            Entry {
                from: name.clone(),
                to: names[1 - i].clone(),
                kind: "reply",
                stage: format!("exchange-{turn}"),
                title,
                body,
            }
        })
        .collect()
}

/// Both agreed, on the same agent of the run. Anything else is no agreement.
fn converged(names: &[String; 2], replies: &[Option<Value>; 2]) -> Option<usize> {
    let picks: Vec<usize> = replies
        .iter()
        .map(|r| {
            let r = r.as_ref().filter(|r| r["agreed"] == true)?;
            names.iter().position(|n| *n == text(&r["implementer"]))
        })
        .collect::<Option<_>>()?;
    (picks[0] == picks[1]).then_some(picks[0])
}

/// The agent's latest plan: its last exchange turn with a plan, else its draft.
fn latest_plan(i: usize, plans: &[Option<Value>; 2], turns: &[[Option<Value>; 2]]) -> String {
    turns
        .iter()
        .rev()
        .filter_map(|t| t[i].as_ref())
        .chain(plans[i].as_ref())
        .map(|d| text(&d["plan"]))
        .find(|p| !p.is_empty())
        .unwrap_or_default()
}

/// The questions still open: the tiebreak's, else each agent's latest.
fn unknowns(tiebreak: Option<&Value>, plans: &[Option<Value>; 2], turns: &[[Option<Value>; 2]]) -> Vec<String> {
    let items: Vec<String> = match tiebreak {
        Some(t) => strings(&t["open_questions"]),
        None => (0..2)
            .flat_map(|i| {
                let source = match turns.last() {
                    Some(last) => last[i].as_ref(),
                    None => plans[i].as_ref(),
                };
                source.map(|s| strings(&s["unknowns"])).unwrap_or_default()
            })
            .collect(),
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let item = item.trim().to_owned();
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&item)) {
            out.push(item);
        }
    }
    out
}

fn history(review: Option<&Value>, revision: Option<&Value>) -> String {
    let mut out = Vec::new();
    if let Some(cr) = review {
        let mut lines = vec![format!("**Verdict:** {}", text(&cr["verdict"])), String::new(), text(&cr["message"])];
        for f in cr["findings"].as_array().map(Vec::as_slice).unwrap_or_default() {
            lines.push(format!(
                "- [{}] {}:{} — {}",
                text(&f["severity"]),
                text(&f["file"]),
                text(&f["line"]),
                text(&f["what"])
            ));
        }
        out.push(format!("### Cross review\n\n{}\n", lines.join("\n")));
    }
    if let Some(rv) = revision {
        let mut lines = vec![text(&rv["message"]), String::new()];
        for f in rv["fixed"].as_array().map(Vec::as_slice).unwrap_or_default() {
            lines.push(format!("- fixed: {} → {}", text(&f["finding"]), text(&f["what_changed"])));
        }
        for f in rv["rebuttals"].as_array().map(Vec::as_slice).unwrap_or_default() {
            lines.push(format!("- declined: {} → {}", text(&f["finding"]), text(&f["why_not"])));
        }
        out.push(format!("### Revision\n\n{}\n", lines.join("\n")));
    }
    if out.is_empty() { "_No review or revision happened._".to_owned() } else { out.join("\n") }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;
    use crate::agent::AgentKind;
    use crate::cage::TaskAgent;
    use crate::runtime::fake::{FakePipe, FakeRuntime, FakeVm};
    use crate::runtime::{PipeEvent, PipeInput};
    use crate::task_agents::{claude_result_line, codex_result_lines};
    use crate::task_artifacts::{Mirror, read_room};

    /// What the fake agent does for one call.
    enum Fake {
        Reply(String),
        Exit(i32),
        /// Runs until it is killed.
        Hang,
        /// Replies after the gate opens.
        After(Arc<Notify>, String),
    }

    type Script = Arc<dyn Fn(&str, Stage, &str) -> Fake + Send + Sync>;
    /// What a shell process does, by its script: a `Reply` is its stdout.
    type Sh = Arc<dyn Fn(&str) -> Fake + Send + Sync>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Stage {
        Plan,
        Exchange,
        Tiebreak,
        Implement,
        Crossreview,
        Revise,
        Final,
    }

    fn stage_of(prompt: &str) -> Stage {
        let first = prompt.lines().next().unwrap_or_default();
        [
            ("# Write a plan", Stage::Plan),
            ("# Exchange", Stage::Exchange),
            ("# Tiebreak", Stage::Tiebreak),
            ("# Implement", Stage::Implement),
            ("# Review", Stage::Crossreview),
            ("# Revise", Stage::Revise),
            ("# Final", Stage::Final),
        ]
        .into_iter()
        .find(|(head, _)| first.starts_with(head))
        .map(|(_, s)| s)
        .expect("a stage prompt")
    }

    /// One agent call that the fake saw.
    #[derive(Debug, Clone)]
    struct Call {
        agent: String,
        stage: Stage,
        argv: Vec<String>,
        prompt: String,
    }

    /// Git checks find no git work tree and gates pass.
    fn no_git(script: &str) -> Fake {
        if script.contains("git rev-parse") { Fake::Exit(3) } else { Fake::Reply("ok\n".into()) }
    }

    /// The calls that the fake saw: of agents, and the argv of each shell.
    type Seen = (Arc<Mutex<Vec<Call>>>, Arc<Mutex<Vec<Vec<String>>>>);

    /// Plays every process that the run opens: shells from `sh`, agents
    /// from the script.
    fn serve(runtime: &FakeRuntime, sh: Sh, script: Script) -> Seen {
        let seen: Seen = Default::default();
        let (pipes, out) = (runtime.pipes.clone(), seen.clone());
        tokio::spawn(async move {
            loop {
                let opened: Vec<FakePipe> = std::mem::take(&mut *pipes.lock().unwrap());
                for pipe in opened {
                    tokio::spawn(play(pipe, sh.clone(), script.clone(), out.clone()));
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        seen
    }

    async fn play(mut pipe: FakePipe, sh: Sh, script: Script, (calls, shells): Seen) {
        let mut input = Vec::new();
        while let Some(msg) = pipe.stdin.recv().await {
            match msg {
                PipeInput::Data(data) => input.extend(data),
                PipeInput::Close => break,
                PipeInput::Kill => return,
            }
        }
        let out = |event| pipe.out.send(event);
        let fake = if pipe.argv[0] == "/bin/sh" {
            shells.lock().unwrap().push(pipe.argv.clone());
            sh(&pipe.argv[2])
        } else {
            let agent = match pipe.argv.iter().find(|a| a.starts_with("/opt/zwinger/")).map(String::as_str) {
                Some(bin) if bin.contains("claude") => "claude",
                Some(bin) if bin.contains("codex") => "codex",
                _ => "pi",
            };
            let prompt = String::from_utf8(input).unwrap();
            let stage = stage_of(&prompt);
            calls.lock().unwrap().push(Call {
                agent: agent.to_owned(),
                stage,
                argv: pipe.argv.clone(),
                prompt: prompt.clone(),
            });
            match script(agent, stage, &prompt) {
                Fake::Reply(text) if agent == "codex" => {
                    Fake::Reply(String::from_utf8(codex_result_lines(&text)).unwrap())
                }
                Fake::Reply(text) => Fake::Reply(String::from_utf8(claude_result_line(&text)).unwrap()),
                Fake::After(gate, text) => {
                    gate.notified().await;
                    let lines = if agent == "codex" { codex_result_lines(&text) } else { claude_result_line(&text) };
                    Fake::Reply(String::from_utf8(lines).unwrap())
                }
                other => other,
            }
        };
        let reply = match fake {
            Fake::Reply(text) => text,
            Fake::After(gate, text) => {
                gate.notified().await;
                text
            }
            Fake::Exit(code) => {
                let _ = out(PipeEvent::Stderr(b"boom\n".to_vec())).await;
                let _ = out(PipeEvent::Exit(code)).await;
                return;
            }
            Fake::Hang => {
                while let Some(msg) = pipe.stdin.recv().await {
                    if matches!(msg, PipeInput::Kill) {
                        break;
                    }
                }
                let _ = out(PipeEvent::Exit(137)).await;
                return;
            }
        };
        let _ = out(PipeEvent::Stdout(reply.into_bytes())).await;
        let _ = out(PipeEvent::Exit(0)).await;
    }

    fn fenced(value: Value) -> String {
        format!("Here it is.\n\n```json\n{value}\n```\n")
    }

    fn plan(claim: &str) -> Fake {
        Fake::Reply(fenced(
            json!({ "plan": format!("Plan that {claim} builds."), "claim": claim, "touches": ["rust"], "risks": [], "unknowns": [], "backlog": ["an idea"] }),
        ))
    }

    fn turn(implementer: &str, agreed: bool) -> Fake {
        Fake::Reply(fenced(
            json!({ "message": "My reply.", "plan": format!("Agreed plan by {implementer}."), "implementer": implementer, "agreed": agreed, "conceded": ["a point"], "unknowns": [], "backlog": [] }),
        ))
    }

    fn done() -> Fake {
        Fake::Reply(fenced(
            json!({ "summary": "Built it.", "files": ["a.rs"], "gates": {}, "deviations": [], "left_undone": [], "missing_dependencies": [], "backlog": [] }),
        ))
    }

    fn review(verdict: &str) -> Fake {
        let findings = if verdict == "revise" {
            json!([{ "severity": "blocking", "file": "a.rs", "line": 3, "what": "Bug.", "why": "Wrong.", "fix": "Fix." }])
        } else {
            json!([])
        };
        Fake::Reply(fenced(
            json!({ "verdict": verdict, "findings": findings, "plan_gaps": [], "message": "Looks right." }),
        ))
    }

    fn revised() -> Fake {
        Fake::Reply(fenced(
            json!({ "fixed": [{ "finding": "Bug.", "what_changed": "Fixed." }], "rebuttals": [], "files": ["a.rs"], "gates": {}, "missing_dependencies": [], "message": "Fixed it." }),
        ))
    }

    fn ship(ship: bool) -> Fake {
        Fake::Reply(fenced(json!({ "ship": ship, "blocking": [], "residual": [], "task_met": "Yes." })))
    }

    /// Two agents that agree at once on claude and ship.
    fn happy(_: &str, stage: Stage, _: &str) -> Fake {
        match stage {
            Stage::Plan => plan("claude"),
            Stage::Exchange => turn("claude", true),
            Stage::Tiebreak => unreachable!("no tiebreak"),
            Stage::Implement => done(),
            Stage::Crossreview => review("accept"),
            Stage::Revise => revised(),
            Stage::Final => ship(true),
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Started {
        registry: Registry,
        handle: Arc<RunHandle>,
        dir: PathBuf,
        mirror: PathBuf,
        calls: Arc<Mutex<Vec<Call>>>,
        shells: Arc<Mutex<Vec<Vec<String>>>>,
    }

    fn start(script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static) -> Started {
        start_full(Duration::from_secs(5), no_git, script)
    }

    fn start_with(idle: Duration, script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static) -> Started {
        start_full(idle, no_git, script)
    }

    fn start_full(
        idle: Duration,
        sh: impl Fn(&str) -> Fake + Send + Sync + 'static,
        script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static,
    ) -> Started {
        let runtime = FakeRuntime::default();
        let (calls, shells) = serve(&runtime, Arc::new(sh), Arc::new(script));
        let dir = temp_dir().join("01");
        let root = temp_dir();
        let parts: Vec<String> = [".zwinger", "tasks", "fix", "01"].map(str::to_owned).into();
        let mirror = Mirror { root: root.clone(), parts };
        mirror.check().unwrap();
        let task = Task {
            id: "task-1".into(),
            slug: "fix".into(),
            title: "Fix it".into(),
            request: "Fix the bug.".into(),
            agents: [
                TaskAgent { kind: AgentKind::Claude, model: Some("opus".into()) },
                TaskAgent { kind: AgentKind::Codex, model: None },
            ],
            gates: vec!["pnpm build".into()],
            workdir: "/workspace".into(),
        };
        let registry = Registry::default();
        let handle = registry.start(Spec {
            project_id: "s".into(),
            task,
            run: "01".into(),
            exec: Exec {
                runtime,
                vm: Arc::new(FakeVm { project_id: "s".into() }),
                cwd: "/workspace".into(),
                artifacts: Some("/workspace/.zwinger/tasks/fix/01".into()),
                pi_host: false,
                gemini: None,
                proxy: None,
            },
            writer: RoundWriter::create(dir.clone(), Some(mirror)).unwrap(),
            config: Config { turns: 2, limits: Limits { idle, hard: Duration::from_secs(10) } },
        });
        Started { registry, handle, dir, mirror: root.join(".zwinger/tasks/fix/01"), calls, shells }
    }

    async fn settled(run: &Started) -> Value {
        tokio::time::timeout(Duration::from_secs(20), run.handle.settled()).await.expect("the run settles");
        serde_json::from_slice(&std::fs::read(run.dir.join("manifest.json")).unwrap()).unwrap()
    }

    async fn until(run: &Started, check: impl Fn(&Status) -> bool) {
        let mut status = run.handle.status.clone();
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| check(s))).await.unwrap().unwrap();
    }

    fn events(run: &Started) -> Vec<(String, String)> {
        std::fs::read_to_string(run.dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|l| {
                let e: Value = serde_json::from_str(l).unwrap();
                (text(&e["kind"]), text(&e["detail"]))
            })
            .collect()
    }

    fn calls_of(run: &Started, stage: Stage) -> Vec<Call> {
        run.calls.lock().unwrap().iter().filter(|c| c.stage == stage).cloned().collect()
    }

    fn states(dir: &std::path::Path) -> Vec<(String, &'static str)> {
        read_room(dir).stages.into_iter().map(|s| (s.id, s.state)).collect()
    }

    #[tokio::test]
    async fn happy_path_agrees_builds_and_ships() {
        let run = start(happy);
        let manifest = settled(&run).await;
        assert_eq!(run.handle.status().state, RunState::Done);
        assert_eq!(manifest["implementer"], "claude");
        assert_eq!(manifest["reviewer"], "codex");
        assert_eq!(manifest["crossreview_verdict"], "accept");
        assert_eq!(manifest["ship"], true);
        assert_eq!(manifest["ship_basis"], "both reviewers");
        assert_eq!(manifest["degraded"], json!([]));
        assert_eq!(manifest["backlog"], json!(["an idea"]));
        for file in [
            "turns/plan-claude.json",
            "turns/plan-codex.json",
            "turns/t1-claude.json",
            "implementation.json",
            "gates.json",
            "crossreview.json",
            "final-claude.json",
            "final-codex.json",
        ] {
            assert!(run.dir.join(file).is_file(), "{file}");
            assert!(run.mirror.join(file).is_file(), "mirror {file}");
        }
        assert!(!run.dir.join("revision.json").exists());
        assert!(run.mirror.join("manifest.json").is_file());

        // Read-only stages pass the read-only flags; the implementer writes.
        let plan = calls_of(&run, Stage::Plan).into_iter().find(|c| c.agent == "claude").unwrap();
        assert!(plan.argv.contains(&"--disallowedTools".to_owned()));
        let implement = calls_of(&run, Stage::Implement);
        assert_eq!(implement.len(), 1);
        assert_eq!(implement[0].agent, "claude");
        assert!(!implement[0].argv.contains(&"--disallowedTools".to_owned()));
        // The plans are blind; the exchange sees both.
        assert!(!plan.prompt.contains("Plan that claude builds."));
        assert!(calls_of(&run, Stage::Exchange)[0].prompt.contains("### codex — opening plan"));
        // The cross review gets the gates.
        assert!(calls_of(&run, Stage::Crossreview)[0].prompt.contains("pnpm build"));
        // Each later stage gets the brokered transcript.
        for stage in [Stage::Implement, Stage::Crossreview, Stage::Final] {
            let prompt = &calls_of(&run, stage)[0].prompt;
            assert!(prompt.contains("### codex — opening plan"), "{stage:?}");
            assert!(prompt.contains("**Conceded:** a point"), "{stage:?}");
        }

        let gates: Value = serde_json::from_slice(&std::fs::read(run.dir.join("gates.json")).unwrap()).unwrap();
        assert_eq!(gates, json!([{ "command": "pnpm build", "code": 0, "output": "ok\n" }]));
        assert!(events(&run).iter().any(|(k, _)| k == "no_detection"));
        let stages = states(&run.dir);
        assert!(stages.contains(&("exchange-2".to_owned(), "skipped")));
        assert!(stages.contains(&("tiebreak".to_owned(), "skipped")));
        assert!(stages.contains(&("gates".to_owned(), "done")));
        assert!(stages.contains(&("revise".to_owned(), "skipped")));
        assert!(stages.contains(&("final".to_owned(), "done")));
    }

    #[tokio::test]
    async fn no_agreement_goes_to_the_tiebreak_of_the_first_agent() {
        let run = start(|agent, stage, prompt| match stage {
            Stage::Exchange => turn(agent, false),
            Stage::Tiebreak => Fake::Reply(fenced(
                json!({ "implementer": "codex", "why": "Codex named the files.", "plan": "The tiebreak plan.", "open_questions": [] }),
            )),
            _ => happy(agent, stage, prompt),
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["implementer"], "codex");
        assert_eq!(calls_of(&run, Stage::Exchange).len(), 4);
        let tiebreak = calls_of(&run, Stage::Tiebreak);
        assert_eq!(tiebreak.len(), 1);
        assert_eq!(tiebreak[0].agent, "claude");
        assert_eq!(std::fs::read_to_string(run.dir.join("plan.md")).unwrap(), "The tiebreak plan.");
        assert_eq!(calls_of(&run, Stage::Implement)[0].agent, "codex");
        let room = read_room(&run.dir);
        assert_eq!(room.decided_by.as_deref(), Some("tiebreak"));
        assert!(
            room.disagreements.iter().any(|d| d.about == "implementer" && d.resolved.as_deref() == Some("tiebreak"))
        );
        assert_eq!(room.messages.iter().find(|m| m.kind == "tiebreak").unwrap().from, "claude");
    }

    #[tokio::test]
    async fn a_failed_agent_gives_a_walkover_and_no_ship() {
        let run =
            start(|agent, stage, prompt| if agent == "codex" { Fake::Exit(1) } else { happy(agent, stage, prompt) });
        let manifest = settled(&run).await;
        assert_eq!(manifest["implementer"], "claude");
        assert_eq!(manifest["planned"], json!(["claude"]));
        assert_eq!(manifest["degraded"], json!(["codex"]));
        assert_eq!(manifest["crossreview_verdict"], "missing");
        assert_eq!(manifest["ship"], false);
        assert_eq!(manifest["ship_basis"], "withheld — only claude reviewed");
        assert!(calls_of(&run, Stage::Exchange).is_empty());
        let events = events(&run);
        assert!(events.contains(&("walkover".to_owned(), "claude proceeds unopposed".to_owned())));
        assert!(events.iter().any(|(k, d)| k == "agent_degraded" && d.starts_with("codex plan")));
        let room = read_room(&run.dir);
        assert_eq!(room.decided_by.as_deref(), Some("walkover"));
        assert!(room.degraded.iter().any(|d| d.agent.as_deref() == Some("codex")));
    }

    #[tokio::test]
    async fn a_revise_verdict_gets_one_revision() {
        let run = start(|agent, stage, prompt| match stage {
            Stage::Crossreview => review("revise"),
            _ => happy(agent, stage, prompt),
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["crossreview_verdict"], "revise");
        let revise = calls_of(&run, Stage::Revise);
        assert_eq!(revise.len(), 1);
        assert_eq!(revise[0].agent, "claude");
        assert!(revise[0].prompt.contains("\"verdict\": \"revise\""));
        assert!(revise[0].prompt.contains("### claude — turn 1 (to codex)"));
        assert!(run.dir.join("revision.json").is_file());
        assert!(calls_of(&run, Stage::Final)[0].prompt.contains("fixed: Bug. → Fixed."));
        assert!(states(&run.dir).contains(&("revise".to_owned(), "done")));
    }

    #[tokio::test]
    async fn an_oversize_reply_degrades_the_agent() {
        let run = start(|agent, stage, prompt| match stage {
            Stage::Implement => {
                Fake::Reply(fenced(json!({ "summary": "x".repeat(task_agents::REPLY_LIMIT), "files": [] })))
            }
            _ => happy(agent, stage, prompt),
        });
        tokio::time::timeout(Duration::from_secs(60), run.handle.settled()).await.expect("the run settles");
        assert_eq!(run.handle.status().state, RunState::Done);
        assert!(!run.dir.join("implementation.json").exists());
        assert!(run.dir.join("implementation.raw").is_file());
        let events = events(&run);
        assert!(
            events.iter().any(|(k, d)| k == "agent_degraded" && d.starts_with("claude implement replied")),
            "{events:?}"
        );
        assert!(calls_of(&run, Stage::Crossreview)[0].prompt.contains("The implementer returned no report."));
    }

    #[tokio::test]
    async fn a_reply_with_no_json_is_asked_once_more() {
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = tries.clone();
        let run = start(move |agent, stage, prompt| {
            if agent == "claude" && stage == Stage::Plan && counted.fetch_add(1, Ordering::SeqCst) == 0 {
                return Fake::Reply("I forgot the block.".into());
            }
            happy(agent, stage, prompt)
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["planned"], json!(["claude", "codex"]));
        let plans: Vec<Call> = calls_of(&run, Stage::Plan).into_iter().filter(|c| c.agent == "claude").collect();
        assert_eq!(plans.len(), 2);
        assert!(plans[1].prompt.contains("Your last reply had no valid JSON object."));
        assert!(events(&run).iter().any(|(k, d)| k == "retry" && d.starts_with("claude plan")));
        assert!(run.dir.join("turns/plan-claude.raw").is_file());
    }

    #[tokio::test]
    async fn twice_no_json_degrades_the_agent() {
        let run = start(|agent, stage, prompt| {
            if agent == "codex" && stage == Stage::Plan {
                return Fake::Reply("No block.".into());
            }
            happy(agent, stage, prompt)
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["degraded"], json!(["codex"]));
        assert_eq!(calls_of(&run, Stage::Plan).iter().filter(|c| c.agent == "codex").count(), 2);
    }

    #[tokio::test]
    async fn cancel_kills_the_stage_and_settles_once() {
        let run = start(|agent, stage, prompt| match stage {
            Stage::Implement => Fake::Hang,
            _ => happy(agent, stage, prompt),
        });
        until(&run, |s| s.stage.as_deref() == Some("implement")).await;
        // Wait for the process, so the cancel kills a running stage.
        tokio::time::timeout(Duration::from_secs(5), async {
            while calls_of(&run, Stage::Implement).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        // A cage stop that races with it does not settle again.
        let _ = run.handle.control.send(Control::Cancel { reason: Some("the cage stopped".into()) });
        let manifest = settled(&run).await;
        assert_eq!(run.handle.status().state, RunState::Cancelled);
        assert_eq!(manifest["stopped"], "cancelled by the user");
        assert_eq!(manifest["ship"], false);
        let events = events(&run);
        assert_eq!(events.iter().filter(|(k, _)| k == "cancelled").count(), 1);
        assert!(!events.iter().any(|(k, _)| k == "agent_degraded"));
        assert!(calls_of(&run, Stage::Crossreview).is_empty());
        assert_eq!(read_room(&run.dir).ended.as_deref(), Some("cancelled"));
        assert!(run.handle.control(Control::Pause).is_err());
    }

    #[tokio::test]
    async fn a_stalled_agent_is_killed_and_degraded() {
        let idle = Duration::from_millis(100);
        let run = start_with(idle, |agent, stage, prompt| {
            if agent == "codex" && stage == Stage::Plan { Fake::Hang } else { happy(agent, stage, prompt) }
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["degraded"], json!(["codex"]));
        assert!(events(&run).iter().any(|(k, d)| k == "agent_stalled" && d == "codex plan idle 0s"));
    }

    #[tokio::test]
    async fn user_messages_reach_the_next_prompt_and_a_pick_overrides_the_exchange() {
        let gate = Arc::new(Notify::new());
        let held = gate.clone();
        let run = start(move |agent, stage, prompt| match stage {
            Stage::Plan if agent == "claude" => Fake::After(
                held.clone(),
                match plan("claude") {
                    Fake::Reply(text) => text,
                    _ => unreachable!(),
                },
            ),
            _ => happy(agent, stage, prompt),
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while calls_of(&run, Stage::Plan).len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        run.handle.control(Control::Message { text: "Use SQLite, not files.".into() }).unwrap();
        // No agent has a plan yet.
        assert!(run.handle.control(Control::PickImplementer { agent: "codex".into() }).is_err());
        run.handle.control(Control::Pause).unwrap();
        assert!(run.handle.control(Control::PickImplementer { agent: "pi".into() }).is_err());
        assert!(run.handle.control(Control::Answer { text: "x".into() }).is_err());
        gate.notify_one();

        until(&run, |s| s.state == RunState::Paused).await;
        assert!(calls_of(&run, Stage::Exchange).is_empty());
        run.handle.control(Control::PickImplementer { agent: "codex".into() }).unwrap();
        run.handle.control(Control::Resume).unwrap();
        let manifest = settled(&run).await;

        assert_eq!(manifest["implementer"], "codex");
        assert!(calls_of(&run, Stage::Exchange).is_empty());
        let implement = calls_of(&run, Stage::Implement);
        assert_eq!(implement[0].agent, "codex");
        assert!(implement[0].prompt.contains("Use SQLite, not files."));
        let events = events(&run);
        assert!(events.contains(&("decision".to_owned(), "user chose codex".to_owned())));
        assert!(events.iter().any(|(k, _)| k == "paused"));
        let room = read_room(&run.dir);
        assert_eq!(room.decided_by.as_deref(), Some("user"));
        assert!(
            room.messages.iter().any(|m| m.kind == "user" && m.body == "Use SQLite, not files." && m.stage == "plan")
        );
        assert!(run.handle.status().decided);
    }

    #[tokio::test]
    async fn open_questions_wait_for_the_answer() {
        let run = start(|agent, stage, prompt| match stage {
            Stage::Exchange => Fake::Reply(fenced(
                json!({ "message": "Ok.", "plan": "P.", "implementer": "claude", "agreed": true, "conceded": [], "unknowns": ["Which database?"], "backlog": [] }),
            )),
            _ => happy(agent, stage, prompt),
        });
        until(&run, |s| s.state == RunState::Waiting).await;
        assert_eq!(read_room(&run.dir).questions, ["Which database?"]);
        run.handle.control(Control::Answer { text: "Postgres.".into() }).unwrap();
        settled(&run).await;
        let implement = calls_of(&run, Stage::Implement);
        assert!(implement[0].prompt.contains("(answer, during exchange-1) Postgres."));
        assert!(read_room(&run.dir).questions.is_empty());
    }

    const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const COMMIT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn clean(head: &str) -> Fake {
        Fake::Reply(format!("HEAD {head}\n\0"))
    }

    fn is_snapshot(script: &str) -> bool {
        script.contains("status --porcelain")
    }

    #[tokio::test]
    async fn a_change_in_a_read_only_stage_is_a_violation() {
        let snapshots = Arc::new(AtomicUsize::new(0));
        let run = start_full(
            Duration::from_secs(5),
            move |script| match script {
                s if is_snapshot(s) => match snapshots.fetch_add(1, Ordering::SeqCst) {
                    0 => clean(BASE),
                    _ => Fake::Reply(format!("HEAD {BASE}\n?? evil.txt\n\0evil")),
                },
                s => no_git(s),
            },
            happy,
        );
        let manifest = settled(&run).await;
        let violations = strings(&manifest["violations"]);
        assert_eq!(violations, ["the repository changed during the read-only stage plan: ?? evil.txt"]);
        assert_eq!(manifest["base"], BASE);
        assert!(!events(&run).iter().any(|(k, _)| k == "no_detection"));
        // The check states what it saw: it names no agent.
        assert!(read_room(&run.dir).violations.iter().all(|v| v.agent.is_none()));
        // Only the run's own artifact folder is skipped.
        let shell = run.shells.lock().unwrap()[0].clone();
        assert_eq!(shell[4], "/workspace/.zwinger/tasks/fix/01");
        assert!(!shell[2].contains("**/.zwinger/**"));
    }

    #[tokio::test]
    async fn a_failed_check_is_a_violation_and_later_checks_go_on() {
        let snapshots = Arc::new(AtomicUsize::new(0));
        let counted = snapshots.clone();
        let run = start_full(
            Duration::from_secs(5),
            move |script| match script {
                s if is_snapshot(s) => match counted.fetch_add(1, Ordering::SeqCst) {
                    1 => Fake::Exit(4),
                    _ => clean(BASE),
                },
                s => no_git(s),
            },
            happy,
        );
        let manifest = settled(&run).await;
        assert_eq!(
            strings(&manifest["violations"]),
            ["the read-only check of the stage plan failed: git ended with code 4"]
        );
        // plan, exchange 1, crossreview and final: two snapshots each.
        assert_eq!(snapshots.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn reviewers_see_the_commits_of_the_implementer() {
        let built = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let head = built.clone();
        let run = start_full(
            Duration::from_secs(5),
            move |script| match script {
                s if is_snapshot(s) => clean(if head.load(Ordering::SeqCst) { COMMIT } else { BASE }),
                s if s.contains("Commits since") => {
                    Fake::Reply(format!("Commits since the start of the run:\n{COMMIT} feat: the fix\n"))
                }
                s => no_git(s),
            },
            move |agent, stage, prompt| {
                if stage == Stage::Implement {
                    built.store(true, Ordering::SeqCst);
                }
                happy(agent, stage, prompt)
            },
        );
        let manifest = settled(&run).await;
        assert_eq!(manifest["base"], BASE);
        assert_eq!(manifest["violations"], json!([]));
        for stage in [Stage::Crossreview, Stage::Final] {
            let prompt = &calls_of(&run, stage)[0].prompt;
            assert!(prompt.contains("feat: the fix"), "{stage:?}");
            assert!(prompt.contains(&format!("The run started at commit `{BASE}`.")), "{stage:?}");
        }
        let diff = run.shells.lock().unwrap().iter().find(|a| a[2].contains("Commits since")).cloned().unwrap();
        assert_eq!(diff[5], BASE);
        assert_eq!(read_room(&run.dir).outcome.unwrap().base.as_deref(), Some(BASE));
    }

    #[tokio::test]
    async fn a_degraded_agent_cannot_be_picked() {
        let gate = Arc::new(Notify::new());
        let held = gate.clone();
        let run = start(move |agent, stage, prompt| match (agent, stage) {
            ("codex", _) => Fake::Exit(1),
            ("claude", Stage::Plan) => Fake::After(
                held.clone(),
                match plan("claude") {
                    Fake::Reply(text) => text,
                    _ => unreachable!(),
                },
            ),
            _ => happy(agent, stage, prompt),
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while calls_of(&run, Stage::Plan).len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        run.handle.control(Control::Pause).unwrap();
        gate.notify_one();
        until(&run, |s| s.state == RunState::Paused).await;
        assert_eq!(run.handle.status().pickable, ["claude"]);
        assert!(run.handle.control(Control::PickImplementer { agent: "codex".into() }).is_err());
        run.handle.control(Control::PickImplementer { agent: "claude".into() }).unwrap();
        run.handle.control(Control::Resume).unwrap();
        let manifest = settled(&run).await;
        assert_eq!(manifest["implementer"], "claude");
    }

    #[tokio::test]
    async fn cancel_stops_a_running_gate_at_once() {
        let run = start_full(
            Duration::from_secs(5),
            |script| if script.contains("pnpm build") { Fake::Hang } else { no_git(script) },
            happy,
        );
        until(&run, |s| s.stage.as_deref() == Some("gates")).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !run.shells.lock().unwrap().iter().any(|a| a[2].contains("pnpm build")) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        let manifest =
            tokio::time::timeout(Duration::from_secs(3), settled(&run)).await.expect("the cancel is at once");
        assert_eq!(manifest["stopped"], "cancelled by the user");
        assert!(!run.dir.join("gates.json").exists());
    }

    #[tokio::test]
    async fn a_cage_stop_cancels_a_snapshot_with_its_reason() {
        let run = start_full(
            Duration::from_secs(5),
            |script| if is_snapshot(script) { Fake::Hang } else { no_git(script) },
            happy,
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while run.shells.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        run.registry.cancel_project("s", "the cage stopped", Duration::from_secs(5)).await;
        assert_eq!(run.handle.status().state, RunState::Cancelled);
        let manifest = settled(&run).await;
        assert_eq!(manifest["stopped"], "the cage stopped");
        assert_eq!(manifest["violations"], json!([]));
        assert!(calls_of(&run, Stage::Plan).is_empty());
        let events = events(&run);
        assert!(!events.iter().any(|(k, _)| k == "agent_degraded" || k == "violation"), "{events:?}");
    }

    #[tokio::test]
    async fn step_in_messages_have_a_limit() {
        let gate = Arc::new(Notify::new());
        let held = gate.clone();
        let run = start(move |agent, stage, prompt| match stage {
            Stage::Plan => Fake::After(
                held.clone(),
                match plan(agent) {
                    Fake::Reply(text) => text,
                    _ => unreachable!(),
                },
            ),
            _ => happy(agent, stage, prompt),
        });
        for _ in 0..MESSAGES_COUNT {
            run.handle.control(Control::Message { text: "m".into() }).unwrap();
        }
        assert!(run.handle.control(Control::Message { text: "m".into() }).is_err());
        gate.notify_waiters();
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&run).await;
    }

    #[test]
    fn bounds_keep_whole_chars_and_say_what_is_omitted() {
        let text = "é".repeat(200);
        let newest = newest_within(&text, 101);
        assert!(newest.starts_with("_[The earlier "));
        assert!(newest.ends_with('é') && newest.len() <= 101 + 64);
        assert!(start_within(&text, 101).starts_with('é'));
        assert_eq!(newest_within("short", 10), "short");

        let prompt = fit(
            IMPLEMENT,
            &[("ME", "claude"), ("PLAN", &"p".repeat(PROMPT_LIMIT)), ("TRANSCRIPT", &"t".repeat(TRANSCRIPT_LIMIT))],
        );
        assert!(prompt.len() <= PROMPT_LIMIT);
        assert!(prompt.contains("## Output") && prompt.contains("bytes are omitted"));

        let messages = format!("\n\n## Messages from the user\n{}newest", "m".repeat(MESSAGES_BYTES - 16));
        let prompt = fit(
            IMPLEMENT,
            &[
                ("ME", "claude"),
                ("CONTEXT", &"c".repeat(PROMPT_LIMIT)),
                ("MESSAGES", &messages),
                ("PLAN", &"p".repeat(PROMPT_LIMIT / 2)),
            ],
        );
        assert!(prompt.len() <= PROMPT_LIMIT);
        assert!(prompt.contains(&messages));
    }
}
