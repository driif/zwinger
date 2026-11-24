//! Task runs: two agents of a project plan, agree, build and review a task,
//! with the duo stages. One tokio task per run is the only writer of its
//! round; the user controls it through a channel.
//!
//! The agents never talk directly. The run is the broker: each reply becomes
//! a message of the round, and the next stage's prompt includes it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::agent::cli::{iso, now_ms};
use crate::error::{Error, Result};
use crate::run_git;
use crate::runtime::SandboxRuntime;
use crate::session::Task;
use crate::session::TaskAgent;
use crate::task_agents::{self, Access, Exec, Failure, Limits, Reply, SnapshotError};
use crate::task_artifacts::{NO_GIT_DIFF, RoundWriter, append_durable, read_record, stage_key};
use crate::task_participants::{INDEPENDENT, Participant, independent, participants};
use crate::task_recovery::{self, Checkpoint, Decisions, Fence, INDEPENDENT_GROUP, Recovered};

const PLAN: &str = include_str!("task_runs/prompts/plan.md");
const EXCHANGE: &str = include_str!("task_runs/prompts/exchange.md");
const TIEBREAK: &str = include_str!("task_runs/prompts/tiebreak.md");
const IMPLEMENT: &str = include_str!("task_runs/prompts/implement.md");
const CROSSREVIEW: &str = include_str!("task_runs/prompts/crossreview.md");
const REVISE: &str = include_str!("task_runs/prompts/revise.md");
const FINAL: &str = include_str!("task_runs/prompts/final.md");
const INDEPENDENT_REVIEW: &str = include_str!("task_runs/prompts/independent.md");
/// The live records of the active stage that a late room still gets.
const RECENT: usize = 2000;
const LIVE_QUEUE: usize = 1024;
pub(crate) const MESSAGE_LIMIT: usize = 16 << 10;
/// The user's messages and answers of a run, by count and by bytes.
pub(crate) const MESSAGES_COUNT: usize = 32;
pub(crate) const MESSAGES_BYTES: usize = 64 << 10;
/// The transcript in a prompt keeps its newest part within this size.
const TRANSCRIPT_LIMIT: usize = 48 << 10;
const PROMPT_LIMIT: usize = 256 << 10;

/// What the user can do in the room of an in-app run.
#[derive(Debug, Deserialize)]
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
    /// An advisory message of an A2A client. Only [`RunHandle::contribute`]
    /// makes it: the webview cannot send it.
    #[serde(skip)]
    Contribute {
        id: String,
        text: String,
        /// Fires when the round has the message.
        applied: oneshot::Sender<()>,
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

/// What a run uses of its recorded options.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Exchange turns before a tiebreak.
    pub turns: u32,
    /// The limits of each agent call and each gate command, not of a stage.
    pub limits: Limits,
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

const DIFF_FAILED: &str = "The app could not save the diff after ";
const LAST_DIFF: &str = " This is the last saved diff.\n\n";

/// The last real diff in a saved `diff.txt`: the diff itself, or the
/// last saved diff that a failed read kept. `None` with no git or no diff.
fn last_diff(saved: &str) -> Option<String> {
    if saved == NO_GIT_DIFF {
        return None;
    }
    match saved.strip_prefix(DIFF_FAILED) {
        Some(failed) => failed.split_once(LAST_DIFF).map(|(_, last)| last.to_owned()),
        None => Some(saved.to_owned()),
    }
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

/// The messages of the user and of A2A clients that a run accepted. Each
/// one is durable in `budget.jsonl` of the round before it counts, so a
/// continuation keeps the budget that the run used.
struct Budget {
    /// The round folder.
    dir: PathBuf,
    count: usize,
    bytes: usize,
    /// The run accepts no more messages. Set under this lock when a cancel
    /// starts and at the end of the run, so a contribution is queued only
    /// while the run can apply it.
    closed: bool,
    /// By A2A message id: its digest and delivery state.
    contributions: HashMap<String, (String, Delivery)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Reserved,
    Queued,
    Applied,
}

impl Budget {
    fn new(dir: PathBuf) -> Self {
        Self { dir, count: 0, bytes: 0, closed: false, contributions: HashMap::new() }
    }

    /// Takes a message, or with `external` an A2A contribution and the
    /// digest of its text. A failed write of the budget takes nothing.
    fn take(&mut self, text: &str, external: Option<(&str, &str)>) -> Result<()> {
        if text.trim().is_empty() || text.len() > MESSAGE_LIMIT {
            return Err(Error::RunState("the message must have 1 to 16384 bytes"));
        }
        if self.count == MESSAGES_COUNT || self.bytes + text.len() > MESSAGES_BYTES {
            return Err(Error::RunState("the run has the limit of 32 messages and 64 KiB"));
        }
        let line = match external {
            None => json!({ "at": iso(now_ms()), "kind": "message", "bytes": text.len() }),
            Some((id, digest)) => {
                json!({ "at": iso(now_ms()), "kind": "external", "bytes": text.len(), "ref": id, "digest": digest })
            }
        };
        append_durable(&self.dir, task_recovery::BUDGET, &line)
            .map_err(|_| Error::RunState("the run cannot record the message"))?;
        self.count += 1;
        self.bytes += text.len();
        Ok(())
    }
}

/// What [`RunHandle::contribute`] did with an A2A message.
#[derive(Debug)]
pub enum Contribution {
    /// A new message: the receiver fires when the round has it.
    Queued(oneshot::Receiver<()>),
    /// The same id and text came before, and the round has it.
    Applied,
    /// The same id and text came before, and the run did not apply it yet.
    Pending,
}

/// Why [`RunHandle::contribute`] refused an A2A message.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The run has ended or stops: it accepts no more messages.
    Ended,
    /// The message id came before with a different text.
    Conflict,
    /// Empty, too large, or over the run's message budget.
    Invalid(&'static str),
}

/// An in-app run, as the commands see it.
pub struct RunHandle {
    pub session_id: String,
    pub task_id: String,
    pub run: String,
    pub agents: [String; 2],
    control: mpsc::UnboundedSender<Control>,
    status: watch::Receiver<Status>,
    live: Arc<Live>,
    cancel: Arc<Cancel>,
    /// The messages of the user and of A2A clients so far.
    budget: Arc<Mutex<Budget>>,
}

impl RunHandle {
    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// The status, and each change of it.
    pub fn status_rx(&self) -> watch::Receiver<Status> {
        self.status.clone()
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
            Control::Message { text } | Control::Answer { text } => lock(&self.budget).take(text, None)?,
            Control::Cancel { reason } => self.stop(reason.clone()),
            Control::Contribute { .. } => return Err(Error::RunState("only an A2A share can contribute")),
            _ => {}
        }
        // A cancel also goes to the run, so a paused or waiting run wakes up.
        self.control.send(control).map_err(|_| Error::RunState("the run has ended"))
    }

    /// Closes the budget and sets the cancel under the budget's lock, so no
    /// contribution is queued after a cancel starts.
    fn stop(&self, reason: Option<String>) {
        let mut budget = lock(&self.budget);
        budget.closed = true;
        self.cancel.set(reason);
    }

    /// Queues an advisory A2A message with the client's message `id`. A
    /// repeated id with the same text adds nothing; with a different text
    /// it fails. It shares the budget of the user's messages. A run that
    /// has ended or stops refuses each message, also a repeated one.
    pub fn contribute(&self, id: &str, text: &str) -> std::result::Result<Contribution, Refusal> {
        let digest = task_recovery::digest(text.as_bytes());
        let mut budget = lock(&self.budget);
        if budget.closed || self.cancel.is_set() || !self.status().state.is_live() {
            return Err(Refusal::Ended);
        }
        if let Some((seen, state)) = budget.contributions.get(id) {
            if *seen != digest {
                return Err(Refusal::Conflict);
            }
            match state {
                Delivery::Applied => return Ok(Contribution::Applied),
                Delivery::Queued => return Ok(Contribution::Pending),
                Delivery::Reserved => {}
            }
        } else {
            budget.take(text, Some((id, &digest))).map_err(|e| match e {
                Error::RunState(why) => Refusal::Invalid(why),
                _ => Refusal::Invalid("the message is not valid"),
            })?;
        }
        let (applied, rx) = oneshot::channel();
        let control = Control::Contribute { id: id.to_owned(), text: text.to_owned(), applied };
        // The run closes the budget before it stops reading, so this send
        // reaches a run that still applies it.
        if self.control.send(control).is_err() {
            return Err(Refusal::Ended);
        }
        budget.contributions.insert(id.to_owned(), (digest, Delivery::Queued));
        Ok(Contribution::Queued(rx))
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
    pub session_id: String,
    pub task: Task,
    pub run: String,
    pub exec: Exec<R>,
    pub writer: RoundWriter,
    pub config: Config,
    pub shared_vm: bool,
}

impl Registry {
    pub fn find(&self, session_id: &str, task_id: &str, run: &str) -> Option<Arc<RunHandle>> {
        lock(&self.runs).iter().find(|r| r.session_id == session_id && r.task_id == task_id && r.run == run).cloned()
    }

    pub fn is_running(&self, session_id: &str, task_id: &str) -> bool {
        lock(&self.runs)
            .iter()
            .any(|r| r.session_id == session_id && r.task_id == task_id && r.status().state.is_live())
    }

    /// The session and task ids of the live runs of these sessions.
    pub fn live(&self, session_ids: &[String]) -> Vec<(String, String)> {
        lock(&self.runs)
            .iter()
            .filter(|r| session_ids.contains(&r.session_id) && r.status().state.is_live())
            .map(|r| (r.session_id.clone(), r.task_id.clone()))
            .collect()
    }

    pub fn start<R: SandboxRuntime>(&self, spec: Spec<R>) -> Arc<RunHandle> {
        self.launch(spec, None)
    }

    /// Starts a continuation: a new run that takes over the state of an
    /// ended run, and runs its groups from the first one that is not complete.
    pub fn resume<R: SandboxRuntime>(&self, spec: Spec<R>, recovered: &Recovered) -> Arc<RunHandle> {
        self.launch(spec, Some(recovered))
    }

    fn launch<R: SandboxRuntime>(&self, spec: Spec<R>, recovered: Option<&Recovered>) -> Arc<RunHandle> {
        let names = participants(&spec.task.agents).map(|p| p.id);
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
            session_id: spec.session_id.clone(),
            task_id: spec.task.id.clone(),
            run: spec.run.clone(),
            agents: names.clone(),
            control,
            status,
            live: live.clone(),
            cancel: Arc::new(Cancel { flag: watch::channel(false).0, reason: Mutex::new(None) }),
            budget: Arc::new(Mutex::new(Budget::new(spec.writer.dir().to_owned()))),
        });
        let mut actor = Actor {
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
                budget: handle.budget.clone(),
                transcript: Vec::new(),
                questions: None,
            },
            control: control_rx,
            run: Default::default(),
            flow: Flow::default(),
            shared_vm: spec.shared_vm,
        };
        if let Some(recovered) = recovered {
            actor.restore(recovered);
        }
        tokio::spawn(actor.run(recovered.is_none()));
        let mut runs = lock(&self.runs);
        runs.retain(|r| !(r.session_id == handle.session_id && r.task_id == handle.task_id && r.run == handle.run));
        runs.push(handle.clone());
        handle
    }

    /// Cancels the live runs of the sessions and waits up to `limit` until each
    /// one has written its manifest.
    pub async fn cancel_sessions(&self, session_ids: &[String], reason: &str, limit: Duration) {
        let runs: Vec<Arc<RunHandle>> = lock(&self.runs)
            .iter()
            .filter(|r| session_ids.contains(&r.session_id) && r.status().state.is_live())
            .cloned()
            .collect();
        for run in &runs {
            run.stop(Some(reason.to_owned()));
            let _ = run.control.send(Control::Cancel { reason: None });
        }
        let _ = tokio::time::timeout(limit, async {
            for run in &runs {
                run.settled().await;
            }
        })
        .await;
    }

    /// Forgets the runs of a session or of one of its tasks.
    pub fn forget(&self, session_id: &str, task_id: Option<&str>) {
        lock(&self.runs).retain(|r| r.session_id != session_id || task_id.is_some_and(|t| r.task_id != t));
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
    budget: Arc<Mutex<Budget>>,
    /// The brokered messages of the round, in order.
    transcript: Vec<Entry>,
    /// While the run waits: the digest of its open questions, for the answer.
    questions: Option<String>,
}

/// The fixed sender of A2A contributions. A client cannot choose it, so it
/// cannot speak as the user or as an agent of the run.
pub const EXTERNAL: &str = "a2a";

/// One brokered message: `{from, to, kind, body}`, which the A2A bridge
/// exposes.
struct Entry {
    from: String,
    to: String,
    /// `plan`, `reply`, `tiebreak`, `user`, `answer` or `external`.
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

    fn is_external(&self) -> bool {
        self.kind == "external"
    }
}

/// What the run has decided so far, for the manifest.
#[derive(Default)]
struct Round {
    planned: Vec<usize>,
    implementer: Option<usize>,
    verdict: Option<String>,
    finals: Vec<(usize, Value)>,
    /// The reply of the independent reviewer.
    independent: Option<Value>,
    backlog: Vec<String>,
    violations: Vec<String>,
    /// `Some(false)` once a check found that it cannot see changes.
    detection: Option<bool>,
    /// `HEAD` before the first stage that can write.
    base: Option<String>,
    /// The last diff that the run saved.
    diff: Option<String>,
}

/// What the groups of the run made so far, for the next prompts. A
/// continuation restores it from the checkpoint.
#[derive(Default)]
struct Flow {
    /// The completed groups, in order.
    done: Vec<String>,
    /// By completed agent group: the agents that reported.
    outcomes: BTreeMap<String, Vec<String>>,
    /// The files that each completed check group saw.
    fences: BTreeMap<String, Fence>,
    /// The snapshot of the last read-only stage, if it was stable.
    read_fence: Option<Fence>,
    /// Dropped check files awaiting removal after a downgraded checkpoint.
    cleanup: Vec<String>,
    cleaning: bool,
    /// The digests of the question sets that an inherited answer closed.
    answered: Vec<String>,
    /// By relative name: the digest of each artifact of the checkpoint.
    artifacts: BTreeMap<String, String>,
    /// The complete lines of `events.jsonl` at the last group boundary.
    events: usize,
    plans: [Option<Value>; 2],
    turns: Vec<[Option<Value>; 2]>,
    tiebreak: Option<Value>,
    /// The plan that the implementer builds.
    plan: String,
    result: Option<Value>,
    /// The latest gate results, for a prompt.
    gates: String,
    review: Option<Value>,
    revision: Option<Value>,
    /// The change that the cross review saw.
    change: Option<String>,
}

struct Actor<R: SandboxRuntime> {
    io: Io<R>,
    book: Book,
    control: mpsc::UnboundedReceiver<Control>,
    run: Round,
    flow: Flow,
    shared_vm: bool,
}

/// The run note when the VM contains more than one session.
fn shared_vm_note(scope: &str) -> String {
    format!("The git-state check covers only {scope}. Other sessions in the same VM can see this task's files.")
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
        let mut line = json!({ "at": iso(now_ms()), "stage": stage, "kind": kind, "body": text });
        if kind == "answer" {
            line["questions"] = json!(self.questions);
        }
        let mut budget = lock(&self.budget);
        let attempt = json!({ "kind": "apply-start", "digest": task_recovery::digest(text.as_bytes()) });
        if self.writer.append_durable(task_recovery::BUDGET, &attempt).is_err()
            || self.writer.append_durable("messages.jsonl", &line).is_err()
            || self.writer.append_durable(task_recovery::BUDGET, &json!({ "kind": "apply-complete" })).is_err()
        {
            budget.closed = true;
            return;
        }
        drop(budget);
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

    /// Writes an A2A contribution to the round, then acknowledges it. It
    /// holds the budget's lock, so a repeated id sees it applied or ended.
    /// After a failed write the contribution does not apply: the budget
    /// closes, the id is dropped and its acknowledgment closes. The failure
    /// stays set, so the contributions queued before it are refused too.
    fn contribution(&mut self, id: String, text: String, applied: oneshot::Sender<()>) {
        let stage = self.stage_id();
        let line = json!({ "at": iso(now_ms()), "stage": stage, "kind": "external", "from": EXTERNAL, "ref": id, "body": text });
        let shared = self.budget.clone();
        let mut budget = lock(&shared);
        let attempt = json!({ "kind": "apply-start", "ref": id });
        if self.writer.failure().is_some()
            || self.writer.append_durable(task_recovery::BUDGET, &attempt).is_err()
            || self.writer.append_durable("messages.jsonl", &line).is_err()
            || self.writer.append_durable(task_recovery::BUDGET, &json!({ "kind": "apply-complete" })).is_err()
        {
            budget.closed = true;
            return;
        }
        self.add([Entry {
            from: EXTERNAL.to_owned(),
            to: "all".to_owned(),
            kind: "external",
            title: format!("advisory message, during {stage}"),
            body: text,
            stage,
        }]);
        if let Some(entry) = budget.contributions.get_mut(&id) {
            entry.1 = Delivery::Applied;
        }
        let _ = applied.send(());
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
            Control::Contribute { id, text, applied } => self.contribution(id, text, applied),
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
        self.call_agent(&self.task.agents[i], self.name(i), stage, access, prompt).await
    }

    /// One call of `agent`, whose live records and files use `name`.
    async fn call_agent(&self, agent: &TaskAgent, name: &str, stage: &str, access: Access, prompt: &str) -> Reply {
        let live = |record: Value| self.live.send(name, stage, record);
        let mut cancel = self.cancel.clone();
        // The id also names the agent state of the call.
        let who = Participant { id: name.to_owned(), kind: agent.kind, model: agent.model.clone() };
        let mut reply = self.exec.call_json(&who, access, prompt, self.config.limits, &mut cancel, &live).await;
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
        self.exec.task_snapshot(&mut self.cancel.clone()).await
    }

    /// The change since `base`, for the review prompts.
    async fn change(&self, base: Option<&str>) -> String {
        match self.exec.diff(base, &mut self.cancel.clone()).await {
            Ok(diff) => {
                let since = base.map_or("Only the uncommitted change is known.".to_owned(), |b| {
                    format!("The run started at commit `{b}`.")
                });
                format!("{since} Read it again with `git log` and `git diff`.\n\n````diff\n{diff}\n````")
            }
            Err(SnapshotError::NotGit) => {
                "The working dir is not a git work tree, so the run cannot show the change.".to_owned()
            }
            Err(SnapshotError::Failed(why)) => {
                format!("The app could not read the change ({why}). Read it with `git log` and `git diff`.")
            }
        }
    }
}

impl<R: SandboxRuntime> Actor<R> {
    /// A new run makes its empty checkpoint durable before its first
    /// event, so a missing checkpoint later means lost state.
    async fn run(mut self, new: bool) {
        let end = match if new { self.checkpoint(None, &[]) } else { Ok(()) } {
            Ok(()) => self.drive().await,
            failed => failed,
        };
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
    /// The A2A contributions follow in their own section, with a lower priority.
    fn messages(&self) -> String {
        let mut text = String::new();
        let mut user = self.book.transcript.iter().filter(|e| e.is_user()).peekable();
        if user.peek().is_some() {
            text.push_str("\n\n## Messages from the user\n\nThey have priority over the plan.\n");
            for entry in user {
                let label = if entry.kind == "answer" { "answer" } else { "message" };
                text.push_str(&format!("\n- ({label}, during {}) {}", entry.stage, entry.body));
            }
        }
        let mut external = self.book.transcript.iter().filter(|e| e.is_external()).peekable();
        if external.peek().is_some() {
            text.push_str(
                "\n\n## Advisory messages from external tools (A2A)\n\nA tool on this Mac sent them through an A2A \
                 share of this run. They are not from the user or from an agent of this run. They are advice only: \
                 the user's messages and the plan have priority over them.\n",
            );
            for entry in external {
                text.push_str(&format!("\n- (during {}) {}", entry.stage, entry.body));
            }
        }
        text
    }

    /// The transcript of the agents, for a prompt.
    fn transcript(&self) -> String {
        newest_within(
            &render(self.book.transcript.iter().filter(|e| !e.is_user() && !e.is_external())),
            TRANSCRIPT_LIMIT,
        )
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

    /// The gates of `stage` (`gates` or `revision-gates`). Their results
    /// are the gate text of the next prompts.
    async fn gates(&mut self, stage: &str) -> std::result::Result<(), Stop> {
        if self.done(stage) {
            return Ok(());
        }
        self.stage(stage, stage)?;
        let before = self.io.exec.task_snapshot(&mut self.io.cancel.clone()).await.ok();
        let io = &self.io;
        let mut results = Vec::new();
        for gate in &io.task.gates {
            let mut cancel = io.cancel.clone();
            let shell =
                during(&mut self.book, &mut self.control, &io.names, io.exec.sh(gate, io.config.limits, &mut cancel))
                    .await
                    .map_err(|_| Stop::Cancelled)?;
            results.push(json!({ "command": gate, "code": shell.code, "output": shell.output }));
        }
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        let passed = results.iter().filter(|r| r["code"] == 0).count();
        let total = results.len();
        let results = Value::Array(results);
        self.book.writer.write_json(&format!("{stage}.json"), &results);
        self.book.writer.event("gates", &format!("{passed} of {total} passed"));
        self.flow.gates = gates_text(&results);
        let after = self.io.exec.task_snapshot(&mut self.io.cancel.clone()).await.ok();
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        if let (Some(before), Some(after)) = (before, after)
            && before == after
        {
            self.flow.fences.insert(stage.to_owned(), Self::fence_of(&after));
        }
        self.checkpoint(Some(stage), &[format!("{stage}.json")])?;
        self.boundary().await
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
        // A control can wait in the queue when the stage ends; the run accepted it, so it applies here.
        while let Ok(control) = self.control.try_recv() {
            self.book.handle(control, &self.io.names);
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
    async fn wait_answers(&mut self, questions: &[String], asked: String) -> std::result::Result<(), Stop> {
        self.book.questions = Some(asked);
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
        self.book.questions = None;
        self.book.writer.event("answered", "by the user");
        self.book.set(|s| s.state = RunState::Running);
        Ok(())
    }

    /// Writes the raw reply and its JSON, or records the failure. Returns the JSON.
    fn record(&mut self, i: usize, stage: &str, base: &str, reply: Reply) -> Option<Value> {
        self.record_as(&self.name(i), stage, base, reply)
    }

    fn record_as(&mut self, name: &str, stage: &str, base: &str, reply: Reply) -> Option<Value> {
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
    /// check saw meanwhile. It does not know who made the change. The base
    /// that the first check finds is durable before the work starts.
    async fn read_only<T>(&mut self, stage: &str, work: impl AsyncFnOnce(&Io<R>) -> T) -> std::result::Result<T, Stop> {
        let before = self.io.snapshot().await;
        if let Ok(snapshot) = &before
            && self.run.base.is_none()
            && !self.book.status.borrow().decided
        {
            self.run.base = Some(snapshot.head.clone());
            let base = self.run.base.clone();
            self.book.set(|s| s.base = base);
            // The repository at the start, for a commit or a PR after the run.
            self.book.writer.write(run_git::START_STATUS, snapshot.status.as_bytes());
            if let Some(mut repo) = run_git::start_identity(&self.io.exec, &mut self.io.cancel.clone()).await {
                repo.base = snapshot.head.clone();
                let _ = self
                    .book
                    .writer
                    .write_durable(run_git::START, &serde_json::to_vec_pretty(&repo).unwrap_or_default());
            }
            self.checkpoint(None, &[])?;
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
            return Ok(out);
        }
        self.flow.read_fence = None;
        match check {
            Ok((before, after)) => {
                self.run.detection = Some(true);
                if before == after {
                    self.flow.read_fence = Some(Self::fence_of(&after));
                } else {
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
        Ok(out)
    }

    fn violation(&mut self, detail: String) {
        self.book.writer.event("violation", &detail);
        self.run.violations.push(detail);
    }

    fn done(&self, group: &str) -> bool {
        self.flow.done.iter().any(|g| g == group)
    }

    /// Marks `group` complete, or with `None` only saves the decisions, and
    /// makes the checkpoint durable with the digests of `files`. The files
    /// are made durable first. A round with a failed write cannot mark a
    /// group complete, and a failed checkpoint write fails the run.
    fn checkpoint(&mut self, group: Option<&str>, files: &[String]) -> std::result::Result<(), Stop> {
        let writer = &self.book.writer;
        let failed = || Stop::Failed(writer.failure().unwrap_or_else(|| "cannot write the checkpoint".to_owned()));
        if writer.failure().is_some() {
            return Err(failed());
        }
        for name in files {
            let bytes = match read_record(writer.dir(), name) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return Err(Stop::Failed(format!("{name} is missing at the checkpoint"))),
                Err(why) => return Err(Stop::Failed(why)),
            };
            writer.write_durable(name, &bytes).map_err(|_| failed())?;
            self.flow.artifacts.insert(name.clone(), task_recovery::digest(&bytes));
        }
        // A checkpoint inside a group keeps the events of the last boundary,
        // so a continuation does not inherit the start of a partial group.
        if let Some(group) = group {
            self.flow.events = match read_record(writer.dir(), "events.jsonl") {
                Ok(bytes) => bytes.unwrap_or_default().iter().filter(|&&b| b == b'\n').count(),
                Err(why) => return Err(Stop::Failed(why)),
            };
            self.flow.done.push(group.to_owned());
        }
        let checkpoint = Checkpoint {
            groups: self.flow.done.clone(),
            outcomes: self.flow.outcomes.clone(),
            decisions: self.decisions(),
            artifacts: self.flow.artifacts.clone(),
            events: self.flow.events,
            fences: self.flow.fences.clone(),
            cleanup: self.flow.cleanup.clone(),
            cleaning: self.flow.cleaning,
            ..Checkpoint::empty()
        };
        let bytes = serde_json::to_vec_pretty(&checkpoint).map_err(|e| Stop::Failed(e.to_string()))?;
        let writer = &self.book.writer;
        writer
            .write_durable(task_recovery::CHECKPOINT, &bytes)
            .map_err(|_| Stop::Failed(writer.failure().unwrap_or_else(|| "cannot write the checkpoint".to_owned())))
    }

    fn decisions(&self) -> Decisions {
        let names = &self.io.names;
        let status = self.book.status.borrow();
        Decisions {
            planned: self.run.planned.iter().map(|&i| names[i].clone()).collect(),
            picked: status.picked.clone().filter(|_| status.decided),
            implementer: self.run.implementer.map(|i| names[i].clone()),
            verdict: self.run.verdict.clone(),
            base: self.run.base.clone(),
            detection: self.run.detection,
            backlog: self.run.backlog.clone(),
            violations: self.run.violations.clone(),
        }
    }

    /// Takes over the state of an ended run: the completed groups, their
    /// decisions and artifacts, the transcript, the messages and the budget.
    fn restore(&mut self, recovered: &Recovered) {
        let checkpoint = &recovered.checkpoint;
        let decisions = &checkpoint.decisions;
        let names = self.io.names.clone();
        let index = |name: &String| names.iter().position(|n| n == name);
        let json = |name: &str| recovered.artifacts.get(name).and_then(|b| serde_json::from_slice::<Value>(b).ok());
        let done = |group: &str| checkpoint.groups.iter().any(|g| g == group);
        self.flow.done = checkpoint.groups.clone();
        self.flow.outcomes = checkpoint.outcomes.clone();
        self.flow.fences = checkpoint.fences.clone();
        self.flow.cleanup = checkpoint.cleanup.clone();
        self.flow.cleaning = checkpoint.cleaning;
        self.flow.artifacts = checkpoint.artifacts.clone();
        self.flow.events = checkpoint.events;
        self.run.planned = decisions.planned.iter().filter_map(index).collect();
        self.run.implementer = decisions.implementer.as_ref().and_then(index);
        self.run.verdict = decisions.verdict.clone();
        self.run.base = decisions.base.clone();
        self.run.detection = decisions.detection;
        self.run.backlog = decisions.backlog.clone();
        self.run.violations = decisions.violations.clone();

        let mut entries = Vec::new();
        if done("plan") {
            self.flow.plans = names.clone().map(|n| json(&format!("turns/plan-{n}.json")));
            entries.extend(opening(&names, &self.flow.plans));
        }
        for turn in (1..).take_while(|n| done(&format!("exchange-{n}"))) {
            let replies = names.clone().map(|n| json(&format!("turns/t{turn}-{n}.json")));
            entries.extend(turn_entries(&names, turn, &replies));
            self.flow.turns.push(replies);
        }
        if done("tiebreak") {
            self.flow.tiebreak = json("tiebreak.json");
            if let Some(t) = &self.flow.tiebreak
                && let Some(pick) = index(&text(&t["implementer"]))
            {
                entries.push(tiebreak_entry(&names, pick, t));
            }
        }
        if let Some(plan) = recovered.artifacts.get("plan.md") {
            self.flow.plan = String::from_utf8_lossy(plan).into_owned();
        }
        self.flow.result = json("implementation.json");
        let diff = if done("revise") { "revision-diff.txt" } else { "implementation-diff.txt" };
        self.run.diff = recovered.artifacts.get(diff).and_then(|b| last_diff(&String::from_utf8_lossy(b)));
        self.flow.gates = gate_result(done).and_then(json).as_ref().map(gates_text).unwrap_or_default();
        self.flow.review = json("crossreview.json");
        self.flow.revision = json("revision.json");
        let finals = checkpoint.outcomes.get("final").filter(|_| done("final")).into_iter().flatten();
        self.run.finals = finals.filter_map(|n| Some((index(n)?, json(&format!("final-{n}.json"))?))).collect();
        let reviewed = checkpoint.outcomes.get(INDEPENDENT_GROUP).is_some_and(|r| r.iter().any(|n| n == INDEPENDENT));
        self.run.independent = json("independent-review.json").filter(|_| reviewed && done(INDEPENDENT_GROUP));

        // The budget of the source is the start of this budget. An accepted
        // contribution with no message line stays reserved: a retry with the
        // same id and text applies it once, with no new charge.
        let mut budget = lock(&self.book.budget);
        for line in recovered.ledger.iter().filter(|l| l["kind"] == "message" || l["kind"] == "external") {
            budget.count += 1;
            budget.bytes += line["bytes"].as_u64().unwrap_or_default() as usize;
            if let (Some(id), Some(digest)) = (line["ref"].as_str(), line["digest"].as_str()) {
                let applied = recovered.messages.iter().any(|m| m["kind"] == "external" && m["ref"] == id);
                budget.contributions.insert(
                    id.to_owned(),
                    (digest.to_owned(), if applied { Delivery::Applied } else { Delivery::Reserved }),
                );
            }
        }
        drop(budget);
        // The messages of the source are the applied ones: each one has its line.
        for line in &recovered.messages {
            let body = text(&line["body"]);
            let stage = text(&line["stage"]);
            let (from, kind, title) = match line["kind"].as_str() {
                Some("external") => (EXTERNAL, "external", format!("advisory message, during {stage}")),
                Some("answer") => {
                    self.flow.answered.push(text(&line["questions"]));
                    ("user", "answer", format!("answer, during {stage}"))
                }
                _ => ("user", "user", format!("message, during {stage}")),
            };
            entries.push(Entry { from: from.to_owned(), to: "all".to_owned(), kind, stage, title, body });
        }
        entries.sort_by_key(|e| stage_key(&e.stage));
        self.book.add(entries);

        let pickable: Vec<String> = self.run.planned.iter().map(|&i| names[i].clone()).collect();
        let decided = done("decision");
        // A message before the first stage of the continuation names the last completed stage.
        let stage = checkpoint.groups.iter().rev().find(|g| *g != "decision").cloned();
        let picked = if decided { decisions.picked.clone() } else { recovered.picked() };
        let base = self.run.base.clone();
        self.book.set(|s| {
            s.pickable = pickable;
            s.decided = decided;
            s.picked = picked;
            s.base = base;
            s.stage = stage;
        });
    }

    /// The agents of `group` that reported, for the checkpoint.
    fn outcome(&mut self, group: &str, reported: impl IntoIterator<Item = usize>) {
        let names = reported.into_iter().map(|i| self.name(i)).collect();
        self.flow.outcomes.insert(group.to_owned(), names);
    }

    fn fence_of(snapshot: &task_agents::Snapshot) -> Fence {
        Fence { head: snapshot.head.clone(), hash: task_recovery::digest(&snapshot.hash) }
    }

    /// A continuation reuses the checks after the last write stage only if
    /// the files are the ones that they saw. Else it drops those groups
    /// and their results, so they run again. The next write stage needs
    /// no check: the checks after it run anyway.
    async fn revalidate(&mut self) -> std::result::Result<(), Stop> {
        if self.flow.cleaning {
            self.finish_cleanup()?;
        }
        if task_recovery::first_check(&self.current_checkpoint()).is_none() {
            return Ok(());
        }
        let current = self.io.exec.task_snapshot(&mut self.io.cancel.clone()).await.ok().map(|s| Self::fence_of(&s));
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        let Some(first) = task_recovery::first_invalid(&self.current_checkpoint(), current.as_ref()) else {
            return Ok(());
        };
        let first_index = self.flow.done.iter().position(|g| g == &first).unwrap_or_default();
        let (kept_events, cut) = self.event_prefix(&first)?;
        let dropped = self.flow.done.split_off(first_index);
        self.flow.events = cut;
        self.flow.cleaning = true;
        let writer = &self.book.writer;
        for group in &dropped {
            let reported = self.flow.outcomes.remove(group).unwrap_or_default();
            let files = match group.as_str() {
                "crossreview" => vec!["crossreview.json".to_owned()],
                "final" => reported.iter().map(|n| format!("final-{n}.json")).collect(),
                // `gates`, `revision-gates` and `independent-review`.
                group => vec![format!("{group}.json")],
            };
            for file in files {
                self.flow.artifacts.remove(&file);
                self.flow.cleanup.push(file);
            }
            self.flow.fences.remove(group);
        }
        if dropped.iter().any(|g| g == "crossreview") {
            self.run.verdict = None;
            self.flow.review = None;
            self.flow.change = None;
        }
        self.flow.gates = match gate_result(|g| self.flow.done.iter().any(|d| d == g)) {
            Some(name) => {
                let bytes = read_record(writer.dir(), name).map_err(Stop::Failed)?;
                let Some(bytes) = bytes.filter(|b| self.flow.artifacts.get(name) == Some(&task_recovery::digest(b)))
                else {
                    return Err(Stop::Failed(format!("{name} changed after the checkpoint")));
                };
                serde_json::from_slice(&bytes).ok().as_ref().map(gates_text).unwrap_or_default()
            }
            None => String::new(),
        };
        if dropped.iter().any(|g| g == "final") {
            self.run.finals.clear();
        }
        self.run.independent = None;
        let stage = self.flow.done.iter().rev().find(|g| *g != "decision").cloned();
        self.book.set(|s| s.stage = stage);
        let labels: Vec<String> = dropped.iter().map(|g| crate::task_artifacts::stage_label(g)).collect();
        // The reduced checkpoint is authoritative before any old event or
        // result is removed. A crash can therefore resume the cleanup.
        self.checkpoint(None, &[])?;
        self.finish_cleanup_with(&kept_events, &labels)
    }

    fn current_checkpoint(&self) -> Checkpoint {
        Checkpoint {
            groups: self.flow.done.clone(),
            outcomes: self.flow.outcomes.clone(),
            decisions: self.decisions(),
            artifacts: self.flow.artifacts.clone(),
            events: self.flow.events,
            fences: self.flow.fences.clone(),
            cleanup: self.flow.cleanup.clone(),
            cleaning: self.flow.cleaning,
            ..Checkpoint::empty()
        }
    }

    fn event_prefix(&self, group: &str) -> std::result::Result<(Vec<u8>, usize), Stop> {
        let writer = &self.book.writer;
        let bytes = read_record(writer.dir(), "events.jsonl").map_err(Stop::Failed)?.unwrap_or_default();
        let lines: Vec<&[u8]> = bytes.split_inclusive(|&b| b == b'\n').collect();
        let starts = |line: &&[u8]| {
            serde_json::from_slice::<Value>(line).is_ok_and(|e| {
                e["kind"] == "stage" && crate::task_artifacts::stage_of(&text(&e["detail"])).as_deref() == Some(group)
            })
        };
        let cut = lines.iter().position(starts).unwrap_or(self.flow.events);
        Ok((lines[..cut].concat(), cut))
    }

    fn finish_cleanup(&mut self) -> std::result::Result<(), Stop> {
        let bytes = read_record(self.book.writer.dir(), "events.jsonl").map_err(Stop::Failed)?.unwrap_or_default();
        let kept: Vec<u8> = bytes.split_inclusive(|&b| b == b'\n').take(self.flow.events).flatten().copied().collect();
        self.finish_cleanup_with(&kept, &[])
    }

    fn finish_cleanup_with(&mut self, kept: &[u8], labels: &[String]) -> std::result::Result<(), Stop> {
        let writer = &self.book.writer;
        writer
            .write_durable("events.jsonl", kept)
            .map_err(|_| Stop::Failed(writer.failure().unwrap_or_else(|| "cannot write events.jsonl".to_owned())))?;
        for file in &self.flow.cleanup {
            writer.remove(file);
        }
        if let Some(why) = writer.failure() {
            return Err(Stop::Failed(why));
        }
        if !labels.is_empty() {
            writer.event(
                "files_changed",
                &format!("the files changed after the checks, so these stages run again: {}", labels.join(", ")),
            );
        }
        self.flow.cleanup.clear();
        self.flow.cleaning = false;
        self.checkpoint(None, &[])
    }

    /// The groups of the run, in order. Each group runs, or a continuation
    /// has it from the checkpoint. A partial group runs again in full.
    async fn drive(&mut self) -> std::result::Result<(), Stop> {
        self.book.writer.write("task.md", format!("# {}\n\n{}\n", self.io.task.title, self.io.task.request).as_bytes());
        self.revalidate().await?;
        if self.done("plan") && self.shared_vm {
            let note = shared_vm_note(&self.io.exec.scope);
            self.book.writer.event("shared_vm", &note);
        }
        self.plan().await?;
        let agreed = self.exchange().await?;
        let agreed = self.tiebreak(agreed).await?;
        let implementer = self.decide(agreed).await?;
        self.implement(implementer).await?;
        self.gates("gates").await?;
        self.crossreview(implementer).await?;
        if self.run.verdict.as_deref() == Some("revise") {
            self.revise(implementer).await?;
            // The write stage can change files even if it returns no report.
            if !self.io.task.gates.is_empty() {
                self.gates("revision-gates").await?;
            }
        }
        self.finals().await?;
        self.independent_review().await
    }

    /// 1. Blind plans, in parallel.
    async fn plan(&mut self) -> std::result::Result<(), Stop> {
        if self.done("plan") {
            return Ok(());
        }
        self.stage("plan", "plan")?;
        if self.shared_vm {
            let note = shared_vm_note(&self.io.exec.scope);
            self.book.writer.event("shared_vm", &note);
        }
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
        let replies = self.read_only("plan", async |io: &Io<R>| io.both("plan", &prompts).await).await?;
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
        self.flow.plans = plans;
        self.outcome("plan", self.run.planned.clone());
        let files: Vec<String> =
            self.run.planned.iter().map(|&i| format!("turns/plan-{}.json", self.name(i))).collect();
        self.checkpoint(Some("plan"), &files)?;
        self.boundary().await
    }

    /// The exchange turns (stage 2). Returns the implementer that the
    /// agents agree on, or that a walkover gives.
    async fn exchange(&mut self) -> std::result::Result<Option<usize>, Stop> {
        if let [only] = self.run.planned[..] {
            // A walkover: nothing was exchanged, so there is no agreement.
            if !self.done("decision") {
                self.book.writer.event("walkover", &format!("{} proceeds unopposed", self.name(only)));
            }
            return Ok(Some(only));
        }
        for turn in 1..=self.io.config.turns {
            let id = format!("exchange-{turn}");
            let restored = self.done(&id);
            if !restored {
                if self.picked().is_some() {
                    break;
                }
                self.exchange_turn(turn).await?;
            }
            let replied = &self.flow.turns[turn as usize - 1];
            let converged = converged(&self.io.names, replied);
            let files: Vec<String> = (0..2)
                .filter(|&i| replied[i].is_some())
                .map(|i| format!("turns/t{turn}-{}.json", self.name(i)))
                .collect();
            if restored {
                if converged.is_some() {
                    return Ok(converged);
                }
                continue;
            }
            self.outcome(&id, (0..2).filter(|&i| replied[i].is_some()).collect::<Vec<_>>());
            if let Some(pick) = converged {
                self.book.writer.event("converged", &format!("turn {turn} on {}", self.name(pick)));
                self.checkpoint(Some(&id), &files)?;
                return Ok(Some(pick));
            }
            self.book.writer.event("no_agreement", &format!("after turn {turn}"));
            self.checkpoint(Some(&id), &files)?;
            self.boundary().await?;
        }
        Ok(None)
    }

    async fn exchange_turn(&mut self, turn: u32) -> std::result::Result<(), Stop> {
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
        let replies = self.read_only(&stage, async |io: &Io<R>| io.both(&stage, &prompts).await).await?;
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
        self.flow.turns.push(replied);
        Ok(())
    }

    /// The tiebreak by the first agent (stage 3), if the exchange found no
    /// implementer and the user picked none.
    async fn tiebreak(&mut self, agreed: Option<usize>) -> std::result::Result<Option<usize>, Stop> {
        if self.done("tiebreak") {
            let pick = self.flow.tiebreak.as_ref().map(|t| text(&t["implementer"]));
            return Ok(self.io.names.iter().position(|n| Some(n) == pick.as_ref()));
        }
        if agreed.is_some() || self.picked().is_some() {
            return Ok(agreed);
        }
        let a = self.name(0);
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
        let reply =
            self.read_only("tiebreak", async |io: &Io<R>| io.call(0, "tiebreak", Access::Read, &prompt).await).await?;
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
        self.book.add([tiebreak_entry(&self.io.names, pick, &json)]);
        self.flow.tiebreak = Some(json);
        self.outcome("tiebreak", [0]);
        self.checkpoint(Some("tiebreak"), &["tiebreak.json".to_owned()])?;
        Ok(Some(pick))
    }

    /// The decision: the user's pick overrides the exchange. Then the
    /// plan, and the answers to the open questions.
    async fn decide(&mut self, agreed: Option<usize>) -> std::result::Result<usize, Stop> {
        if self.done("decision") {
            return self.run.implementer.ok_or_else(|| Stop::Failed("the checkpoint has no implementer".to_owned()));
        }
        let mut implementer = agreed;
        if let Some(pick) = self.picked() {
            self.book.writer.event("decision", &format!("user chose {}", self.name(pick)));
            implementer = Some(pick);
        }
        let Some(implementer) = implementer else {
            return Err(Stop::Failed("the run has no implementer".to_owned()));
        };
        self.run.implementer = Some(implementer);
        self.book.set(|s| s.decided = true);
        let flow = &self.flow;
        let plan = match flow.tiebreak.as_ref().map(|t| text(&t["plan"])).filter(|p| !p.is_empty()) {
            Some(plan) => plan,
            None => latest_plan(implementer, &flow.plans, &flow.turns),
        };
        self.book.writer.write("plan.md", plan.as_bytes());
        self.book.writer.event("decision", &format!("{} implements", self.name(implementer)));
        let questions = unknowns(flow.tiebreak.as_ref(), &flow.plans, &flow.turns);
        if !questions.is_empty() {
            let asked = task_recovery::digest(questions.join("\n").as_bytes());
            // The source recorded the answer to these questions, then stopped before its checkpoint.
            if self.flow.answered.contains(&asked) {
                self.book.writer.event("answered", "by the recorded answer");
            } else {
                self.wait_answers(&questions, asked).await?;
            }
        }
        self.flow.plan = plan;
        self.checkpoint(Some("decision"), &["plan.md".to_owned()])?;
        self.boundary().await?;
        Ok(implementer)
    }

    /// 4. Implement.
    async fn implement(&mut self, implementer: usize) -> std::result::Result<(), Stop> {
        if self.done("implement") {
            return Ok(());
        }
        let me = self.name(implementer);
        self.stage("implement", "implement")?;
        let prompt = fit(
            IMPLEMENT,
            &[
                ("ME", &me),
                ("CONTEXT", &self.context()),
                ("MESSAGES", &self.messages()),
                ("PLAN", &self.flow.plan),
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
        self.save_diff("the implementation", "implementation-diff.txt").await;
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        let mut files = vec!["implementation-diff.txt".to_owned()];
        if result.is_some() {
            files.push("implementation.json".to_owned());
        }
        self.outcome("implement", result.is_some().then_some(implementer));
        self.flow.result = result;
        self.flow.fences.clear();
        self.checkpoint(Some("implement"), &files)?;
        self.boundary().await
    }

    /// 6. Cross review.
    async fn crossreview(&mut self, implementer: usize) -> std::result::Result<(), Stop> {
        if self.done("crossreview") {
            return Ok(());
        }
        let reviewer = Self::peer(implementer);
        let (me, peer) = (self.name(implementer), self.name(reviewer));
        self.stage(&format!("crossreview by {peer}"), "crossreview")?;
        let result_text = self.flow.result.as_ref().map_or("The implementer returned no report.".to_owned(), |r| {
            format!("```json\n{}\n```", serde_json::to_string_pretty(r).unwrap_or_default())
        });
        let change = self.io.change(self.run.base.as_deref()).await;
        let prompt = fit(
            CROSSREVIEW,
            &[
                ("ME", &peer),
                ("PEER", &me),
                ("CONTEXT", &self.context()),
                ("MESSAGES", &self.messages()),
                ("PLAN", &self.flow.plan),
                ("RESULT", &result_text),
                ("GATES", &self.flow.gates),
                ("TRANSCRIPT", &self.transcript()),
                ("CHANGE", &change),
            ],
        );
        self.book.writer.write("crossreview.prompt.md", prompt.as_bytes());
        let reply = self
            .read_only("crossreview", async |io: &Io<R>| io.call(reviewer, "crossreview", Access::Read, &prompt).await)
            .await?;
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
        let files = if review.is_some() { vec!["crossreview.json".to_owned()] } else { Vec::new() };
        self.outcome("crossreview", review.is_some().then_some(reviewer));
        self.flow.review = review;
        self.flow.change = Some(change);
        if let Some(fence) = self.flow.read_fence.take() {
            self.flow.fences.insert("crossreview".to_owned(), fence);
        }
        self.checkpoint(Some("crossreview"), &files)?;
        self.boundary().await
    }

    /// 7. Revise, once.
    async fn revise(&mut self, implementer: usize) -> std::result::Result<(), Stop> {
        if self.done("revise") {
            return Ok(());
        }
        let (me, peer) = (self.name(implementer), self.name(Self::peer(implementer)));
        self.stage("revise", "revise")?;
        let review_text =
            format!("```json\n{}\n```", serde_json::to_string_pretty(&self.flow.review).unwrap_or_default());
        let prompt = fit(
            REVISE,
            &[
                ("ME", &me),
                ("PEER", &peer),
                ("CONTEXT", &self.context()),
                ("MESSAGES", &self.messages()),
                ("PLAN", &self.flow.plan),
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
        let revision = self.record(implementer, "revise", "revision", reply);
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        if revision.is_none() {
            self.book.writer.event("agent_degraded", "revision returned no structured result");
        }
        self.book.writer.event("agent_done", &format!("revised by {me}"));
        self.save_diff("the revision", "revision-diff.txt").await;
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        let mut files = vec!["revision-diff.txt".to_owned()];
        if revision.is_some() {
            files.push("revision.json".to_owned());
        }
        self.outcome("revise", revision.is_some().then_some(implementer));
        self.flow.revision = revision;
        self.flow.fences.clear();
        self.checkpoint(Some("revise"), &files)?;
        self.boundary().await
    }

    /// 8. Blind final reviews, in parallel.
    async fn finals(&mut self) -> std::result::Result<(), Stop> {
        if self.done("final") {
            return Ok(());
        }
        self.stage("final review", "final")?;
        let history = history(self.flow.review.as_ref(), self.flow.revision.as_ref());
        self.book.writer.write("history.md", history.as_bytes());
        let change = match &self.flow.change {
            Some(change) if self.run.verdict.as_deref() != Some("revise") => change.clone(),
            _ => self.io.change(self.run.base.as_deref()).await,
        };
        let prompts = [0, 1].map(|i| {
            fit(
                FINAL,
                &[
                    ("ME", &self.name(i)),
                    ("PEER", &self.name(Self::peer(i))),
                    ("CONTEXT", &self.context()),
                    ("MESSAGES", &self.messages()),
                    ("PLAN", &self.flow.plan),
                    ("HISTORY", &history),
                    ("GATES", &self.flow.gates),
                    ("TRANSCRIPT", &self.transcript()),
                    ("CHANGE", &change),
                ],
            )
        });
        let replies = self.read_only("final", async |io: &Io<R>| io.both("final", &prompts).await).await?;
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
        let files: Vec<String> = self.run.finals.iter().map(|(i, _)| format!("final-{}.json", self.name(*i))).collect();
        self.outcome("final", self.run.finals.iter().map(|(i, _)| *i).collect::<Vec<_>>());
        if let Some(fence) = self.flow.read_fence.take() {
            self.flow.fences.insert("final".to_owned(), fence);
        }
        self.checkpoint(Some("final"), &files)
    }

    /// Stage 9, the independent review, if the task has a reviewer: a fresh
    /// read-only call outside the pair. It sees the change, the gates and
    /// the review history, but not the final reviews of the pair.
    async fn independent_review(&mut self) -> std::result::Result<(), Stop> {
        let Some(agent) = self.io.task.options.independent.clone() else {
            return Ok(());
        };
        if self.done(INDEPENDENT_GROUP) {
            return Ok(());
        }
        self.stage(INDEPENDENT_GROUP, INDEPENDENT_GROUP)?;
        let history = history(self.flow.review.as_ref(), self.flow.revision.as_ref());
        let change = self.io.change(self.run.base.as_deref()).await;
        let prompt = fit(
            INDEPENDENT_REVIEW,
            &[
                ("CONTEXT", &self.context()),
                ("MESSAGES", &self.messages()),
                ("PLAN", &self.flow.plan),
                ("HISTORY", &history),
                ("GATES", &self.flow.gates),
                ("CHANGE", &change),
            ],
        );
        self.book.writer.write("independent-review.prompt.md", prompt.as_bytes());
        let reply = self
            .read_only(INDEPENDENT_GROUP, async |io: &Io<R>| {
                io.call_agent(&agent, INDEPENDENT, INDEPENDENT_GROUP, Access::Read, &prompt).await
            })
            .await?;
        let review = self.record_as(INDEPENDENT, "independent review", "independent-review", reply);
        if self.book.cancelled() {
            return Err(Stop::Cancelled);
        }
        let files = match &review {
            Some(review) => {
                let verdict = match approval(review) {
                    Ok(true) => "approved".to_owned(),
                    Ok(false) => "denied".to_owned(),
                    Err(why) => format!("not valid: {why}"),
                };
                self.book.writer.event("independent_review", &verdict);
                vec!["independent-review.json".to_owned()]
            }
            None => {
                self.book.writer.event("agent_degraded", &format!("{INDEPENDENT} review returned no JSON"));
                Vec::new()
            }
        };
        self.flow
            .outcomes
            .insert(INDEPENDENT_GROUP.to_owned(), review.iter().map(|_| INDEPENDENT.to_owned()).collect());
        self.run.independent = review;
        if let Some(fence) = self.flow.read_fence.take() {
            self.flow.fences.insert(INDEPENDENT_GROUP.to_owned(), fence);
        }
        self.checkpoint(Some(INDEPENDENT_GROUP), &files)
    }

    /// Saves the change of the run after `stage`, so the room can show it
    /// with no VM. A failed read says so and keeps the last saved diff.
    async fn save_diff(&mut self, stage: &str, artifact: &str) {
        let diff = self.io.exec.diff(self.run.base.as_deref(), &mut self.io.cancel.clone()).await;
        if self.book.cancelled() {
            return;
        }
        let text = match diff {
            Ok(diff) => {
                self.run.diff = Some(diff.clone());
                diff
            }
            Err(SnapshotError::NotGit) => NO_GIT_DIFF.to_owned(),
            Err(SnapshotError::Failed(why)) => {
                self.book.writer.event("diff_failed", &format!("after {stage}: {why}"));
                let failed = format!("{DIFF_FAILED}{stage} ({why}).");
                match &self.run.diff {
                    Some(last) => format!("{failed}{LAST_DIFF}{last}"),
                    None => failed,
                }
            }
        };
        self.book.writer.write_durable(artifact, text.as_bytes()).ok();
        self.book.writer.write("diff.txt", text.as_bytes());
    }

    /// The user's pick, if it is an agent with a plan.
    fn picked(&self) -> Option<usize> {
        let picked = self.book.status.borrow().picked.clone()?;
        self.io.names.iter().position(|n| *n == picked).filter(|i| self.run.planned.contains(i))
    }

    /// A run whose record has a failed write fails: its round is not
    /// complete. The failure wins over a cancel, so the record keeps it.
    fn written(&self, end: std::result::Result<(), Stop>) -> std::result::Result<(), Stop> {
        match (end, self.book.writer.failure()) {
            (Ok(()) | Err(Stop::Cancelled), Some(why)) => Err(Stop::Failed(why)),
            (end, _) => end,
        }
    }

    /// The one end of the run: the manifest, the last event and the state.
    fn settle(mut self, end: std::result::Result<(), Stop>) {
        // No contribution is queued from here. One that waits now does not
        // apply: its acknowledgment closes, and its id cannot apply later.
        lock(&self.book.budget).closed = true;
        while self.control.try_recv().is_ok() {}
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

    /// As duo's manifest: `ship` only if both final reviewers set it. With
    /// an independent reviewer, its valid approval is necessary too.
    fn manifest(&self, stopped: Option<&str>) -> Value {
        let names = &self.io.names;
        let reported: Vec<&String> = self.run.finals.iter().map(|(i, _)| &names[*i]).collect();
        let mirror_error = self.book.writer.mirror_error();
        let reviewer = self.io.task.options.independent.as_ref();
        let verdict = reviewer.map(|_| match self.run.independent.as_ref().map(approval) {
            None => Err("the independent reviewer gave no review".to_owned()),
            Some(Ok(true)) => Ok(()),
            Some(Ok(false)) => Err("the independent reviewer did not approve".to_owned()),
            Some(Err(why)) => Err(format!("the independent review is not valid ({why})")),
        });
        let ship = stopped.is_none()
            && mirror_error.is_none()
            && reported.len() == 2
            && self.run.finals.iter().all(|(_, f)| f["ship"] == true)
            && verdict.as_ref().is_none_or(|v| v.is_ok());
        let blocking: Vec<Value> =
            self.run.finals.iter().flat_map(|(_, f)| f["blocking"].as_array().cloned().unwrap_or_default()).collect();
        let ship_basis = if let Some(e) = mirror_error {
            format!("withheld — the harness copy failed: {e}")
        } else if reported.len() != 2 {
            let who: Vec<&str> = reported.iter().map(|s| s.as_str()).collect();
            format!("withheld — only {} reviewed", if who.is_empty() { "nobody".to_owned() } else { who.join(", ") })
        } else {
            match &verdict {
                None => "both reviewers".to_owned(),
                Some(Ok(())) => "both reviewers and the independent reviewer".to_owned(),
                Some(Err(why)) => format!("withheld — {why}"),
            }
        };
        let independent_verdict = reviewer.map(|_| match self.run.independent.as_ref().map(approval) {
            None => "missing",
            Some(Ok(true)) => "approved",
            Some(Ok(false)) => "denied",
            Some(Err(_)) => "invalid",
        });
        let task_met: serde_json::Map<String, Value> =
            self.run.finals.iter().map(|(i, f)| (names[*i].clone(), f["task_met"].clone())).collect();
        json!({
            "task": self.io.task.slug,
            "title": self.io.task.title,
            "participants": participants(&self.io.task.agents),
            "implementer": self.run.implementer.map(|i| names[i].clone()),
            "reviewer": self.run.implementer.map(|i| names[1 - i].clone()),
            "planned": self.run.planned.iter().map(|i| names[*i].clone()).collect::<Vec<_>>(),
            "degraded": (0..2).filter(|i| !self.run.planned.contains(i)).map(|i| names[i].clone()).collect::<Vec<_>>(),
            "crossreview_verdict": self.run.verdict,
            "final_reviewers": reported,
            "independent": reviewer.map(independent),
            "independent_verdict": independent_verdict,
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

/// The tiebreak's choice, as a message of the first agent.
fn tiebreak_entry(names: &[String; 2], pick: usize, json: &Value) -> Entry {
    Entry {
        from: names[0].clone(),
        to: "all".to_owned(),
        kind: "tiebreak",
        stage: "tiebreak".to_owned(),
        title: "tiebreak".to_owned(),
        body: format!("**Implementer:** {}\n\n{}", names[pick], text(&json["why"])),
    }
}

/// The gate results, for a prompt.
/// The gate results that the next prompts read: the revision's if it ran.
fn gate_result(done: impl Fn(&str) -> bool) -> Option<&'static str> {
    if done("revision-gates") {
        Some("revision-gates.json")
    } else if done("gates") {
        Some("gates.json")
    } else {
        None
    }
}

fn gates_text(results: &Value) -> String {
    if results.as_array().is_none_or(Vec::is_empty) {
        "The task has no gates.".to_owned()
    } else {
        format!("```json\n{}\n```", serde_json::to_string_pretty(results).unwrap_or_default())
    }
}

/// The verdict of an independent review: `Ok(true)` approves and
/// `Ok(false)` denies. A review without the fields, or one that approves
/// with blocking findings, is not valid: it is no approval.
fn approval(review: &Value) -> std::result::Result<bool, &'static str> {
    let Some(ship) = review["ship"].as_bool() else {
        return Err("ship is not true or false");
    };
    let Some(blocking) = review["blocking"].as_array() else {
        return Err("blocking is not a list");
    };
    if !review["task_met"].is_string() {
        return Err("task_met is not text");
    }
    if ship && !blocking.is_empty() {
        return Err("it approves with blocking findings");
    }
    Ok(ship)
}

/// Both agreed, on the same agent of the run. Anything else is no agreement.
pub(crate) fn converged(names: &[String; 2], replies: &[Option<Value>; 2]) -> Option<usize> {
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
pub(crate) mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;
    use crate::agent::AgentKind;
    use crate::runtime::fake::{FakePipe, FakeRuntime, FakeVm};
    use crate::runtime::{PipeEvent, PipeInput};
    use crate::session::RunOptions;
    use crate::task_agents::{claude_result_line, codex_result_lines, pi_result_line};
    use crate::task_artifacts::{Mirror, read_diff, read_room};
    use crate::task_recovery::{RunRecord, TaskRuns};

    /// What the fake agent does for one call.
    pub(crate) enum Fake {
        Reply(String),
        Exit(i32),
        /// Runs until it is killed.
        Hang,
        /// Replies after the gate opens.
        After(Arc<Notify>, String),
    }

    pub(crate) type Script = Arc<dyn Fn(&str, Stage, &str) -> Fake + Send + Sync>;
    /// What a shell process does, by its script: a `Reply` is its stdout.
    pub(crate) type Sh = Arc<dyn Fn(&str) -> Fake + Send + Sync>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Stage {
        Plan,
        Exchange,
        Tiebreak,
        Implement,
        Crossreview,
        Revise,
        Final,
        Independent,
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
            ("# Independent review", Stage::Independent),
        ]
        .into_iter()
        .find(|(head, _)| first.starts_with(head))
        .map(|(_, s)| s)
        .expect("a stage prompt")
    }

    /// One agent call that the fake saw.
    #[derive(Debug, Clone)]
    pub(crate) struct Call {
        agent: String,
        stage: Stage,
        argv: Vec<String>,
        pub(crate) prompt: String,
    }

    /// Git checks find no git work tree and gates pass.
    pub(crate) fn no_git(script: &str) -> Fake {
        if script.contains("git rev-parse") { Fake::Exit(3) } else { Fake::Reply("ok\n".into()) }
    }

    /// The calls that the fake saw: of agents, and the argv of each shell.
    pub(crate) type Seen = (Arc<Mutex<Vec<Call>>>, Arc<Mutex<Vec<Vec<String>>>>);

    /// Plays every process that the run opens: shells from `sh`, agents
    /// from the script.
    pub(crate) fn serve(runtime: &FakeRuntime, sh: Sh, script: Script) -> Seen {
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
            let lines = |text: &str| {
                let bytes = match agent {
                    "codex" => codex_result_lines(text),
                    "pi" => pi_result_line(text),
                    _ => claude_result_line(text),
                };
                String::from_utf8(bytes).unwrap()
            };
            match script(agent, stage, &prompt) {
                Fake::Reply(text) => Fake::Reply(lines(&text)),
                Fake::After(gate, text) => {
                    gate.notified().await;
                    Fake::Reply(lines(&text))
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

    pub(crate) fn plan(claim: &str) -> Fake {
        Fake::Reply(fenced(
            json!({ "plan": format!("Plan that {claim} builds."), "claim": claim, "touches": ["rust"], "risks": [], "unknowns": [], "backlog": ["an idea"] }),
        ))
    }

    pub(crate) fn turn(implementer: &str, agreed: bool) -> Fake {
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
    pub(crate) fn happy(_: &str, stage: Stage, _: &str) -> Fake {
        match stage {
            Stage::Plan => plan("claude"),
            Stage::Exchange => turn("claude", true),
            Stage::Tiebreak => unreachable!("no tiebreak"),
            Stage::Implement => done(),
            Stage::Crossreview => review("accept"),
            Stage::Revise => revised(),
            Stage::Final | Stage::Independent => ship(true),
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = crate::test_dir::root().join(format!("zwinger-run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) struct Started {
        registry: Registry,
        pub(crate) handle: Arc<RunHandle>,
        pub(crate) dir: PathBuf,
        mirror: PathBuf,
        calls: Arc<Mutex<Vec<Call>>>,
        shells: Arc<Mutex<Vec<Vec<String>>>>,
    }

    pub(crate) fn start(script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static) -> Started {
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
        start_full_mode(idle, sh, script, false)
    }

    fn start_full_mode(
        idle: Duration,
        sh: impl Fn(&str) -> Fake + Send + Sync + 'static,
        script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static,
        shared_vm: bool,
    ) -> Started {
        let agents = [
            TaskAgent { kind: AgentKind::Claude, model: Some("opus".into()) },
            TaskAgent { kind: AgentKind::Codex, model: None },
        ];
        start_agents(agents, RunOptions::default(), idle, sh, script, shared_vm)
    }

    /// The default pair with the run options `options`.
    pub(crate) fn start_options(
        options: RunOptions,
        script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static,
    ) -> Started {
        let agents = [
            TaskAgent { kind: AgentKind::Claude, model: Some("opus".into()) },
            TaskAgent { kind: AgentKind::Codex, model: None },
        ];
        start_agents(agents, options, Duration::from_secs(5), no_git, script, false)
    }

    fn start_agents(
        agents: [TaskAgent; 2],
        options: RunOptions,
        idle: Duration,
        sh: impl Fn(&str) -> Fake + Send + Sync + 'static,
        script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static,
        shared_vm: bool,
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
            agents,
            gates: vec!["pnpm build".into()],
            workdir: "/workspace".into(),
            options,
        };
        let registry = Registry::default();
        // The tests use limits below one second, which the task cannot set.
        let config = Config { turns: task.options.turns, limits: Limits { idle, hard: Duration::from_secs(10) } };
        let mut record = RunRecord::new(&task);
        record.config.idle_ms = idle.as_millis() as u64;
        record.config.hard_ms = 10_000;
        let writer = RoundWriter::create(dir.clone(), Some(mirror)).unwrap();
        writer.write_durable(task_recovery::RUN, &serde_json::to_vec(&record).unwrap()).unwrap();
        let handle = registry.start(Spec {
            session_id: "s".into(),
            task,
            run: "01".into(),
            exec: Exec {
                runtime,
                vm: Arc::new(FakeVm { key: "s".into() }),
                cwd: "/workspace".into(),
                code: "/workspace".into(),
                scope: "/workspace".into(),
                artifacts: Some("/workspace/.zwinger/tasks/fix/01".into()),
                pi_host: false,
                gemini: None,
                proxy: None,
                runtimes: crate::runtimes::fake_installed().to_vec(),
                placeholders: Vec::new(),
                state: None,
            },
            writer,
            config,
            shared_vm,
        });
        Started { registry, handle, dir, mirror: root.join(".zwinger/tasks/fix/01"), calls, shells }
    }

    pub(crate) async fn settled(run: &Started) -> Value {
        tokio::time::timeout(Duration::from_secs(20), run.handle.settled()).await.expect("the run settles");
        serde_json::from_slice(&std::fs::read(run.dir.join("manifest.json")).unwrap()).unwrap()
    }

    pub(crate) async fn until(run: &Started, check: impl Fn(&Status) -> bool) {
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

    pub(crate) fn calls_of(run: &Started, stage: Stage) -> Vec<Call> {
        run.calls.lock().unwrap().iter().filter(|c| c.stage == stage).cloned().collect()
    }

    fn states(dir: &std::path::Path) -> Vec<(String, &'static str)> {
        read_room(dir).stages.into_iter().map(|s| (s.id, s.state)).collect()
    }

    #[tokio::test]
    async fn shared_vm_note_appears_only_for_multiple_members() {
        let single = start(happy);
        settled(&single).await;
        assert!(!events(&single).iter().any(|(kind, _)| kind == "shared_vm"));

        let shared = start_full_mode(Duration::from_secs(5), no_git, happy, true);
        settled(&shared).await;
        let note =
            "The git-state check covers only /workspace. Other sessions in the same VM can see this task's files.";
        assert!(events(&shared).iter().any(|(kind, detail)| kind == "shared_vm" && detail == note));
        assert!(read_room(&shared.dir).messages.iter().any(|message| message.body.ends_with(note)));
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
        // Accepted work uses one gate run for both reviews.
        assert_eq!(run.shells.lock().unwrap().iter().filter(|argv| argv[2].contains("pnpm build")).count(), 1);
        assert!(!run.dir.join("revision-gates.json").exists());
        assert!(calls_of(&run, Stage::Crossreview)[0].prompt.contains("pnpm build"));
        assert!(calls_of(&run, Stage::Final).iter().all(|c| c.prompt.contains("pnpm build")));
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

    /// A pair of one kind with the models `a` and `b`. `script` gets the
    /// participant id from the prompt instead of the kind.
    pub(crate) fn start_pair(
        kind: AgentKind,
        script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static,
    ) -> Started {
        let agents = ["a", "b"].map(|m| TaskAgent { kind, model: Some(format!("provider/model.{m}")) });
        let me = |prompt: &str| {
            let id = prompt.split("You are `").nth(1).and_then(|rest| rest.split('`').next());
            id.unwrap_or("tiebreak").to_owned()
        };
        start_agents(
            agents,
            RunOptions::default(),
            Duration::from_secs(5),
            no_git,
            move |_, stage, prompt| script(&me(prompt), stage, prompt),
            false,
        )
    }

    fn model_of(call: &Call) -> &str {
        call.argv.iter().find(|a| a.starts_with("provider/model.")).map(String::as_str).unwrap_or_default()
    }

    #[tokio::test]
    async fn a_same_kind_pair_keeps_two_participants_through_the_run() {
        let run = start_pair(AgentKind::Codex, |me, stage, prompt| match stage {
            Stage::Plan => plan("codex-2"),
            Stage::Exchange => turn("codex-2", true),
            Stage::Final => Fake::Reply(fenced(
                json!({ "ship": true, "blocking": [], "residual": [], "task_met": format!("Yes, says {me}.") }),
            )),
            _ => happy(me, stage, prompt),
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["implementer"], "codex-2");
        assert_eq!(manifest["reviewer"], "codex-1");
        assert_eq!(manifest["planned"], json!(["codex-1", "codex-2"]));
        assert_eq!(manifest["final_reviewers"].as_array().unwrap().len(), 2);
        assert_eq!(manifest["task_met"], json!({ "codex-1": "Yes, says codex-1.", "codex-2": "Yes, says codex-2." }));
        assert_eq!(manifest["ship"], true);
        assert_eq!(
            manifest["participants"],
            json!([
                { "id": "codex-1", "kind": "codex", "model": "provider/model.a" },
                { "id": "codex-2", "kind": "codex", "model": "provider/model.b" },
            ])
        );
        for id in ["codex-1", "codex-2"] {
            for file in [format!("turns/plan-{id}.json"), format!("turns/t1-{id}.json"), format!("final-{id}.json")] {
                assert!(run.dir.join(&file).is_file(), "{file}");
            }
        }
        // Each slot gets its own model and prompt, in parallel stages too.
        for stage in [Stage::Plan, Stage::Exchange, Stage::Final] {
            let mut seen: Vec<(String, bool)> = calls_of(&run, stage)
                .iter()
                .map(|c| (model_of(c).to_owned(), c.prompt.contains("You are `codex-1`")))
                .collect();
            seen.sort();
            assert_eq!(seen, [("provider/model.a".to_owned(), true), ("provider/model.b".to_owned(), false)]);
        }
        let implement = calls_of(&run, Stage::Implement);
        assert_eq!((implement.len(), model_of(&implement[0])), (1, "provider/model.b"));
        let review = calls_of(&run, Stage::Crossreview);
        assert_eq!((review.len(), model_of(&review[0])), (1, "provider/model.a"));
        assert!(review[0].prompt.contains("`codex-2` is the implementer"));

        let room = read_room(&run.dir);
        assert_eq!(room.agents, ["codex-1", "codex-2"]);
        assert_eq!(room.participants[1].model.as_deref(), Some("provider/model.b"));
        assert_eq!(room.finals.len(), 2);
        let replies: Vec<(&str, &str)> =
            room.messages.iter().filter(|m| m.kind == "reply").map(|m| (m.from.as_str(), m.to.as_str())).collect();
        assert_eq!(replies, [("codex-1", "codex-2"), ("codex-2", "codex-1")]);
    }

    #[tokio::test]
    async fn a_same_kind_slot_retries_on_its_own_while_the_peer_runs() {
        // claude-1 holds its plan and final until claude-2 retries, so the retries overlap its calls.
        let (plan_gate, final_gate) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (plan_held, final_held) = (plan_gate.clone(), final_gate.clone());
        let tries = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let run = start_pair(AgentKind::Claude, move |me, stage, prompt| {
            let (gate, index) = match stage {
                Stage::Plan => (&plan_held, 0),
                Stage::Final => (&final_held, 1),
                Stage::Exchange => return turn("claude-1", true),
                _ => return happy(me, stage, prompt),
            };
            let reply = match stage {
                Stage::Plan => fenced(
                    json!({ "plan": format!("Plan of {me}."), "claim": "claude-1", "touches": [], "risks": [], "unknowns": [], "backlog": [] }),
                ),
                _ => fenced(
                    json!({ "ship": true, "blocking": [], "residual": [], "task_met": format!("Yes, says {me}.") }),
                ),
            };
            if me == "claude-1" {
                return Fake::After(gate.clone(), reply);
            }
            if tries[index].fetch_add(1, Ordering::SeqCst) == 0 {
                return Fake::Reply(format!("{me} forgot the block."));
            }
            gate.notify_one();
            Fake::Reply(reply)
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["planned"], json!(["claude-1", "claude-2"]));
        assert_eq!(manifest["implementer"], "claude-1");

        for (stage, label) in [(Stage::Plan, "plan"), (Stage::Final, "final")] {
            let calls = calls_of(&run, stage);
            let of = |model: &str| calls.iter().filter(|c| model_of(c) == model).cloned().collect::<Vec<_>>();
            let (first, second) = (of("provider/model.a"), of("provider/model.b"));
            assert_eq!((first.len(), second.len()), (1, 2), "{stage:?}");
            assert!(first[0].prompt.contains("You are `claude-1`"), "{stage:?}");
            assert!(second.iter().all(|c| c.prompt.contains("You are `claude-2`")), "{stage:?}");
            assert!(!first[0].prompt.contains("Your last reply had no valid JSON object."), "{stage:?}");
            assert!(!second[0].prompt.contains("Your last reply had no valid JSON object."), "{stage:?}");
            assert!(second[1].prompt.contains("Your last reply had no valid JSON object."), "{stage:?}");
            let retries: Vec<String> = events(&run).into_iter().filter(|(k, _)| k == "retry").map(|(_, d)| d).collect();
            assert!(retries.iter().any(|d| d.starts_with(&format!("claude-2 {label}"))), "{retries:?}");
            assert!(!retries.iter().any(|d| d.starts_with(&format!("claude-1 {label}"))), "{retries:?}");
        }

        let read = |file: &str| std::fs::read_to_string(run.dir.join(file)).unwrap();
        for (me, peer) in [("claude-1", "claude-2"), ("claude-2", "claude-1")] {
            for (base, own, other) in [
                (format!("turns/plan-{me}"), format!("Plan of {me}."), format!("Plan of {peer}.")),
                (format!("final-{me}"), format!("Yes, says {me}."), format!("Yes, says {peer}.")),
            ] {
                for file in [format!("{base}.raw"), format!("{base}.json")] {
                    let text = read(&file);
                    assert!(text.contains(&own), "{file}");
                    assert!(!text.contains(&other), "{file}");
                    assert!(!text.contains("forgot the block"), "{file}");
                }
            }
        }
        assert_eq!(
            manifest["task_met"],
            json!({ "claude-1": "Yes, says claude-1.", "claude-2": "Yes, says claude-2." })
        );
    }

    #[tokio::test]
    async fn a_pi_pair_runs_with_the_second_slot_as_implementer() {
        let run = start_pair(AgentKind::Pi, |me, stage, prompt| match stage {
            Stage::Plan => plan("pi-2"),
            Stage::Exchange => turn("pi-2", true),
            _ => happy(me, stage, prompt),
        });
        let manifest = settled(&run).await;
        assert_eq!(run.handle.status().state, RunState::Done);
        assert_eq!(manifest["implementer"], "pi-2");
        assert_eq!(manifest["reviewer"], "pi-1");
        assert_eq!(manifest["planned"], json!(["pi-1", "pi-2"]));
        assert_eq!(manifest["ship"], true);
        assert_eq!(
            manifest["participants"],
            json!([
                { "id": "pi-1", "kind": "pi", "model": "provider/model.a" },
                { "id": "pi-2", "kind": "pi", "model": "provider/model.b" },
            ])
        );
        assert!(run.calls.lock().unwrap().iter().all(|c| c.agent == "pi"));
        for stage in [Stage::Plan, Stage::Final] {
            let mut models: Vec<String> = calls_of(&run, stage).iter().map(|c| model_of(c).to_owned()).collect();
            models.sort();
            assert_eq!(models, ["provider/model.a", "provider/model.b"], "{stage:?}");
        }
        let implement = calls_of(&run, Stage::Implement);
        assert_eq!((implement.len(), model_of(&implement[0])), (1, "provider/model.b"));
        assert!(!implement[0].argv.contains(&"--tools".to_owned()));
        let review = calls_of(&run, Stage::Crossreview);
        assert_eq!((review.len(), model_of(&review[0])), (1, "provider/model.a"));
        assert!(review[0].argv.contains(&"--tools".to_owned()));

        let room = read_room(&run.dir);
        assert_eq!(room.agents, ["pi-1", "pi-2"]);
        let participants: Vec<_> =
            room.participants.iter().map(|p| (p.kind, p.model.as_deref(), p.model_recorded)).collect();
        assert_eq!(
            participants,
            [
                (Some(AgentKind::Pi), Some("provider/model.a"), true),
                (Some(AgentKind::Pi), Some("provider/model.b"), true),
            ]
        );
        assert_eq!(room.implementer.as_deref(), Some("pi-2"));
        assert_eq!(room.finals.len(), 2);
    }

    #[tokio::test]
    async fn a_same_kind_tiebreak_is_the_first_slot_and_can_pick_the_second() {
        let run = start_pair(AgentKind::Claude, |me, stage, prompt| match stage {
            Stage::Plan => plan(me),
            Stage::Exchange => turn(me, false),
            Stage::Tiebreak => Fake::Reply(fenced(
                json!({ "implementer": "claude-2", "why": "It named the files.", "plan": "The tiebreak plan.", "open_questions": [] }),
            )),
            _ => happy(me, stage, prompt),
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["implementer"], "claude-2");
        let tiebreak = calls_of(&run, Stage::Tiebreak);
        assert_eq!((tiebreak.len(), model_of(&tiebreak[0])), (1, "provider/model.a"));
        let implement = calls_of(&run, Stage::Implement);
        assert_eq!(model_of(&implement[0]), "provider/model.b");
        assert!(!implement[0].argv.contains(&"--disallowedTools".to_owned()));
        assert!(calls_of(&run, Stage::Crossreview)[0].argv.contains(&"--disallowedTools".to_owned()));
        assert_eq!(model_of(&calls_of(&run, Stage::Crossreview)[0]), "provider/model.a");
        let room = read_room(&run.dir);
        assert_eq!(room.decided_by.as_deref(), Some("tiebreak"));
        assert_eq!(room.messages.iter().find(|m| m.kind == "tiebreak").unwrap().from, "claude-1");
    }

    #[tokio::test]
    async fn a_user_pick_selects_the_second_slot_of_one_kind() {
        let gate = Arc::new(Notify::new());
        let held = gate.clone();
        let run = start_pair(AgentKind::Claude, move |me, stage, prompt| match stage {
            Stage::Plan if me == "claude-1" => match plan("claude-1") {
                Fake::Reply(text) => Fake::After(held.clone(), text),
                _ => unreachable!(),
            },
            Stage::Plan => plan("claude-1"),
            _ => happy(me, stage, prompt),
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while calls_of(&run, Stage::Plan).len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        run.handle.control(Control::Pause).unwrap();
        // The kind name is no participant.
        assert!(run.handle.control(Control::PickImplementer { agent: "claude".into() }).is_err());
        gate.notify_one();
        until(&run, |s| s.state == RunState::Paused).await;
        assert_eq!(run.handle.status().pickable, ["claude-1", "claude-2"]);
        run.handle.control(Control::PickImplementer { agent: "claude-2".into() }).unwrap();
        run.handle.control(Control::Resume).unwrap();
        let manifest = settled(&run).await;
        assert_eq!(manifest["implementer"], "claude-2");
        assert_eq!(model_of(&calls_of(&run, Stage::Implement)[0]), "provider/model.b");
        assert!(events(&run).contains(&("decision".to_owned(), "user chose claude-2".to_owned())));
    }

    #[tokio::test]
    async fn a_same_kind_pair_degrades_the_first_slot_and_keeps_the_second() {
        let run =
            start_pair(
                AgentKind::Claude,
                |me, stage, prompt| {
                    if me == "claude-1" { Fake::Exit(1) } else { happy(me, stage, prompt) }
                },
            );
        let manifest = settled(&run).await;
        assert_eq!(manifest["planned"], json!(["claude-2"]));
        assert_eq!(manifest["degraded"], json!(["claude-1"]));
        assert_eq!(manifest["implementer"], "claude-2");
        assert_eq!(manifest["ship"], false);
        assert_eq!(manifest["final_reviewers"], json!(["claude-2"]));
        assert!(run.dir.join("turns/plan-claude-2.json").is_file());
        assert!(!run.dir.join("turns/plan-claude-1.json").exists());
        let room = read_room(&run.dir);
        assert_eq!(room.implementer.as_deref(), Some("claude-2"));
        assert_eq!(room.decided_by.as_deref(), Some("walkover"));
        assert_eq!(room.finals.len(), 1);
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
    async fn final_reviewers_get_the_revision_gate_results_even_without_a_revision_report() {
        for report in [true, false] {
            let calls = AtomicUsize::new(0);
            let run = start_full(
                Duration::from_secs(5),
                move |script| {
                    if !script.contains("pnpm build") {
                        return no_git(script);
                    }
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 => Fake::Exit(37),
                        1 => Fake::Reply("revision build passed".into()),
                        _ => panic!("extra gate run"),
                    }
                },
                move |agent, stage, prompt| match stage {
                    Stage::Crossreview => review("revise"),
                    Stage::Revise if !report => Fake::Reply("No structured revision report.".into()),
                    _ => happy(agent, stage, prompt),
                },
            );
            settled(&run).await;
            let cross = &calls_of(&run, Stage::Crossreview)[0].prompt;
            assert!(cross.contains("\"code\": 37"));
            assert!(!cross.contains("revision build passed"));
            let finals = calls_of(&run, Stage::Final);
            assert_eq!(finals.len(), 2);
            for call in finals {
                assert!(call.prompt.contains("revision build passed"), "{}", call.agent);
                assert!(!call.prompt.contains("\"code\": 37"), "{}", call.agent);
            }
            assert_eq!(run.dir.join("revision.json").exists(), report);
            let room = read_room(&run.mirror);
            assert_eq!(room.gates[0].output, "revision build passed");
            assert_eq!(room.gates[0].code, Some(0));
            let history: Vec<_> = room
                .messages
                .iter()
                .filter(|m| m.kind == "gates")
                .map(|m| (m.stage.as_str(), m.summary.as_str()))
                .collect();
            assert_eq!(history, [("gates", "0 of 1 gates passed"), ("revision-gates", "1 of 1 gates passed")]);
            assert!(states(&run.dir).contains(&("revision-gates".to_owned(), "done")));
        }
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
        // Only the session folder is checked; its run copy is skipped.
        let shell = run.shells.lock().unwrap()[0].clone();
        assert_eq!((shell[4].as_str(), shell[5].as_str()), ("/workspace", "/workspace/.zwinger/tasks/fix"));
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
        // plan, exchange 1, crossreview and final: two snapshots each;
        // gates take their own before and after snapshots.
        assert_eq!(snapshots.load(Ordering::SeqCst), 10);
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
        assert_eq!(diff[6], BASE);
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
    async fn cancelling_the_revision_gate_leaves_only_the_initial_record() {
        let calls = AtomicUsize::new(0);
        let running = Arc::new(Notify::new());
        let started = running.clone();
        let run = start_full(
            Duration::from_secs(5),
            move |script| {
                if script.contains("pnpm build") && calls.fetch_add(1, Ordering::SeqCst) == 1 {
                    started.notify_one();
                    Fake::Hang
                } else {
                    no_git(script)
                }
            },
            |agent, stage, prompt| {
                if stage == Stage::Crossreview { review("revise") } else { happy(agent, stage, prompt) }
            },
        );
        tokio::time::timeout(Duration::from_secs(5), running.notified()).await.unwrap();
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        let manifest = tokio::time::timeout(Duration::from_secs(3), settled(&run)).await.unwrap();
        assert_eq!(manifest["stopped"], "cancelled by the user");
        assert!(run.dir.join("gates.json").is_file());
        assert!(!run.dir.join("revision-gates.json").exists());
        let room = read_room(&run.dir);
        assert!(room.gates.is_empty());
        assert_eq!(room.messages.iter().filter(|m| m.kind == "gates").count(), 1);
        assert!(states(&run.dir).contains(&("revision-gates".to_owned(), "stopped")));
        assert!(calls_of(&run, Stage::Final).is_empty());
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
        run.registry.cancel_sessions(&["s".to_owned()], "the cage stopped", Duration::from_secs(5)).await;
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

    /// A run whose first plan waits for the gate.
    fn held_plan(gate: Arc<Notify>) -> Started {
        start(move |agent, stage, prompt| match stage {
            Stage::Plan if agent == "claude" => Fake::After(
                gate.clone(),
                match plan(agent) {
                    Fake::Reply(text) => text,
                    _ => unreachable!(),
                },
            ),
            _ => happy(agent, stage, prompt),
        })
    }

    #[tokio::test]
    async fn a2a_contributions_reach_the_next_prompt_once_as_advice() {
        let gate = Arc::new(Notify::new());
        let run = held_plan(gate.clone());
        let text = "Check the cache too.";
        let Ok(Contribution::Queued(applied)) = run.handle.contribute("m-1", text) else {
            panic!("a new contribution is queued");
        };
        tokio::time::timeout(Duration::from_secs(5), applied).await.unwrap().unwrap();
        assert!(matches!(run.handle.contribute("m-1", text), Ok(Contribution::Applied)));
        assert_eq!(run.handle.contribute("m-1", "Something else.").unwrap_err(), Refusal::Conflict);
        gate.notify_one();
        settled(&run).await;

        let exchange = calls_of(&run, Stage::Exchange);
        let prompt = &exchange[0].prompt;
        assert!(prompt.contains("## Advisory messages from external tools (A2A)"));
        assert!(prompt.contains("- (during plan) Check the cache too."));
        assert!(!prompt.contains("## Messages from the user"));
        // Only the advisory section has it, not the agents' transcript.
        assert_eq!(prompt.matches(text).count(), 1);
        let room = read_room(&run.dir);
        let external: Vec<_> = room.messages.iter().filter(|m| m.kind == "external").collect();
        assert_eq!(external.len(), 1);
        assert_eq!((external[0].from.as_str(), external[0].id.as_str()), ("a2a", "messages.jsonl#1"));
        assert_eq!(external[0].body, text);

        assert_eq!(run.handle.contribute("m-2", "Late.").unwrap_err(), Refusal::Ended);
        assert_eq!(run.handle.contribute("m-1", text).unwrap_err(), Refusal::Ended);
    }

    #[tokio::test]
    async fn a_contribution_that_the_round_cannot_record_gets_no_acknowledgment() {
        let gate = Arc::new(Notify::new());
        let run = held_plan(gate.clone());
        // A folder in place of the file makes each append fail.
        std::fs::create_dir(run.dir.join("messages.jsonl")).unwrap();
        let Ok(Contribution::Queued(applied)) = run.handle.contribute("m-1", "Check the cache.") else {
            panic!("a new contribution is queued");
        };
        assert!(tokio::time::timeout(Duration::from_secs(5), applied).await.unwrap().is_err());
        // The run can no longer record a contribution: a retry and a new id end.
        assert_eq!(run.handle.contribute("m-1", "Check the cache.").unwrap_err(), Refusal::Ended);
        assert_eq!(run.handle.contribute("m-2", "Check the logs.").unwrap_err(), Refusal::Ended);
        gate.notify_one();
        let manifest = settled(&run).await;
        assert_eq!(run.handle.status().state, RunState::Failed);
        assert!(manifest["stopped"].as_str().unwrap_or_default().contains("messages.jsonl"), "{manifest}");
        assert!(calls_of(&run, Stage::Exchange).is_empty());
        assert!(read_room(&run.dir).messages.iter().all(|m| m.kind != "external"));
    }

    #[tokio::test]
    async fn a_session_cancel_closes_the_budget_at_once() {
        let gate = Arc::new(Notify::new());
        let run = held_plan(gate.clone());
        run.registry.cancel_sessions(&["s".to_owned()], "the cage stopped", Duration::ZERO).await;
        assert_eq!(run.handle.contribute("m-1", "a").unwrap_err(), Refusal::Ended);
        assert!(lock(&run.handle.budget).closed);
        gate.notify_one();
        settled(&run).await;
    }

    #[tokio::test]
    async fn a2a_contributions_share_the_message_budget_and_stop_at_a_cancel() {
        let gate = Arc::new(Notify::new());
        let run = held_plan(gate.clone());
        assert!(matches!(run.handle.contribute("empty", " "), Err(Refusal::Invalid(_))));
        let large = "x".repeat(MESSAGE_LIMIT + 1);
        assert!(matches!(run.handle.contribute("large", &large), Err(Refusal::Invalid(_))));
        for _ in 0..MESSAGES_COUNT - 1 {
            run.handle.control(Control::Message { text: "m".into() }).unwrap();
        }
        assert!(matches!(run.handle.contribute("last", "a"), Ok(Contribution::Queued(_))));
        assert!(matches!(run.handle.contribute("over", "b"), Err(Refusal::Invalid(_))));
        assert!(run.handle.control(Control::Message { text: "m".into() }).is_err());
        // A refused id is not kept, so its next refusal is for the cancel.
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        assert_eq!(run.handle.contribute("over", "b").unwrap_err(), Refusal::Ended);
        gate.notify_one();
        settled(&run).await;
    }

    fn is_diff(script: &str) -> bool {
        script.contains("Commits since")
    }

    /// A run whose diff tells how many write stages ran.
    fn counted_diff(verdict: &'static str) -> Started {
        let writes = Arc::new(AtomicUsize::new(0));
        let seen = writes.clone();
        start_full(
            Duration::from_secs(5),
            move |script| match script {
                s if is_diff(s) => Fake::Reply(format!("diff after {}", seen.load(Ordering::SeqCst))),
                s => no_git(s),
            },
            move |agent, stage, prompt| match stage {
                Stage::Implement | Stage::Revise => {
                    writes.fetch_add(1, Ordering::SeqCst);
                    happy(agent, stage, prompt)
                }
                Stage::Crossreview => review(verdict),
                _ => happy(agent, stage, prompt),
            },
        )
    }

    #[tokio::test]
    async fn the_implementation_saves_its_diff() {
        let run = counted_diff("accept");
        settled(&run).await;
        assert_eq!(std::fs::read_to_string(run.dir.join("diff.txt")).unwrap(), "diff after 1");
        assert_eq!(read_diff(&run.dir).as_deref(), Some("diff after 1"));
    }

    #[tokio::test]
    async fn the_revision_replaces_the_saved_diff() {
        let run = counted_diff("revise");
        settled(&run).await;
        assert_eq!(std::fs::read_to_string(run.dir.join("diff.txt")).unwrap(), "diff after 2");
    }

    #[tokio::test]
    async fn a_failed_read_after_the_revision_keeps_the_last_diff() {
        let writes = Arc::new(AtomicUsize::new(0));
        let seen = writes.clone();
        let run = start_full(
            Duration::from_secs(5),
            move |script| match script {
                s if is_diff(s) && seen.load(Ordering::SeqCst) > 1 => Fake::Exit(5),
                s if is_diff(s) => Fake::Reply("the first diff".into()),
                s => no_git(s),
            },
            move |agent, stage, prompt| match stage {
                Stage::Implement | Stage::Revise => {
                    writes.fetch_add(1, Ordering::SeqCst);
                    happy(agent, stage, prompt)
                }
                Stage::Crossreview => review("revise"),
                _ => happy(agent, stage, prompt),
            },
        );
        settled(&run).await;
        let saved = std::fs::read_to_string(run.dir.join("diff.txt")).unwrap();
        assert!(saved.starts_with("The app could not save the diff after the revision (git ended with code 5)."));
        assert!(saved.ends_with("This is the last saved diff.\n\nthe first diff"));
        assert!(!saved.contains(NO_GIT_DIFF));
    }

    #[tokio::test]
    async fn an_empty_diff_saves_an_empty_file() {
        let run = start_full(
            Duration::from_secs(5),
            |script| if is_diff(script) { Fake::Reply(String::new()) } else { no_git(script) },
            happy,
        );
        settled(&run).await;
        assert_eq!(std::fs::read_to_string(run.dir.join("diff.txt")).unwrap(), "");
    }

    #[tokio::test]
    async fn outside_git_the_saved_diff_says_so() {
        let run = start(happy);
        settled(&run).await;
        assert_eq!(std::fs::read_to_string(run.dir.join("diff.txt")).unwrap(), NO_GIT_DIFF);
    }

    #[tokio::test]
    async fn a_cancel_during_the_diff_saves_no_diff() {
        let run = start_full(
            Duration::from_secs(5),
            |script| if is_diff(script) { Fake::Hang } else { no_git(script) },
            happy,
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !run.shells.lock().unwrap().iter().any(|argv| is_diff(&argv[2])) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        let manifest = settled(&run).await;
        assert_eq!(manifest["stopped"], "cancelled by the user");
        assert!(!run.dir.join("diff.txt").exists());
        assert!(!run.mirror.join("diff.txt").exists());
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

    // --- continuation ------------------------------------------------------

    /// A full round: no agreement in turn 1, agreement on claude in turn 2,
    /// a revise verdict, then a ship.
    pub(crate) fn full(agent: &str, stage: Stage, prompt: &str) -> Fake {
        match stage {
            Stage::Exchange if prompt.contains("Exchange — turn 1") => turn("claude", true),
            Stage::Exchange => turn("claude", false),
            Stage::Crossreview => review("revise"),
            _ => happy(agent, stage, prompt),
        }
    }

    /// A git work tree that is clean at `head`. The gates pass.
    pub(crate) fn git_at(head: &'static str, script: &str) -> Fake {
        match script {
            s if is_snapshot(s) => clean(head),
            s if is_diff(s) => Fake::Reply("the diff".into()),
            s => no_git(s),
        }
    }

    pub(crate) async fn eventually(check: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the condition comes true");
    }

    /// Continues the ended run 01 of `src` as run 02, with a new fake.
    pub(crate) fn resume_full(
        src: &Started,
        sh: impl Fn(&str) -> Fake + Send + Sync + 'static,
        script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static,
    ) -> Started {
        let task_dir = src.dir.parent().unwrap().to_owned();
        let names = vec!["01".to_owned()];
        let recovered = TaskRuns::read(&task_dir, &names).load("01").unwrap();
        let runtime = FakeRuntime::default();
        let (calls, shells) = serve(&runtime, Arc::new(sh), Arc::new(script));
        let dir = task_dir.join("02");
        let writer = RoundWriter::create(dir.clone(), None).unwrap();
        task_recovery::create(&writer, &recovered, "01").unwrap();
        let registry = Registry::default();
        let handle = registry.resume(
            Spec {
                session_id: "s".into(),
                task: recovered.record.task("task-1", "fix", "/workspace".into()),
                run: "02".into(),
                exec: Exec {
                    runtime,
                    vm: Arc::new(FakeVm { key: "s".into() }),
                    cwd: "/workspace".into(),
                    code: "/workspace".into(),
                    scope: "/workspace".into(),
                    artifacts: Some("/workspace/.zwinger/tasks/fix/02".into()),
                    pi_host: false,
                    gemini: None,
                    proxy: None,
                    runtimes: crate::runtimes::fake_installed().to_vec(),
                    placeholders: Vec::new(),
                    state: None,
                },
                writer,
                config: recovered.record.config(),
                shared_vm: false,
            },
            &recovered,
        );
        Started { registry, handle, dir, mirror: PathBuf::new(), calls, shells }
    }

    /// The groups in order, with the agent stage of each.
    const GROUPS: [(&str, Option<Stage>); 10] = [
        ("plan", Some(Stage::Plan)),
        ("exchange-1", Some(Stage::Exchange)),
        ("exchange-2", Some(Stage::Exchange)),
        ("decision", None),
        ("implement", Some(Stage::Implement)),
        ("gates", None),
        ("crossreview", Some(Stage::Crossreview)),
        ("revise", Some(Stage::Revise)),
        ("revision-gates", None),
        ("final", Some(Stage::Final)),
    ];

    fn gate_runs(run: &Started) -> usize {
        run.shells.lock().unwrap().iter().filter(|a| a[2].contains("pnpm build")).count()
    }

    /// Cancels a full round while the group `at` runs, then continues it.
    /// The implementation commits: HEAD is `BASE` before it and `COMMIT`
    /// after it, also in the continuation.
    async fn interrupted(at: &'static str) -> Started {
        let hung = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (hang_sh, hang_agent) = (hung.clone(), hung.clone());
        let built = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let head = built.clone();
        let gates = Arc::new(AtomicUsize::new(0));
        let sh = move |s: &str| {
            if s.contains("pnpm build") {
                let n = gates.fetch_add(1, Ordering::SeqCst) + 1;
                if (at == "gates" && n == 1) || (at == "revision-gates" && n == 2) {
                    hang_sh.store(true, Ordering::SeqCst);
                    return Fake::Hang;
                }
            }
            git_at(if head.load(Ordering::SeqCst) { COMMIT } else { BASE }, s)
        };
        let script = move |agent: &str, stage: Stage, prompt: &str| {
            if stage == Stage::Implement {
                built.store(true, Ordering::SeqCst);
            }
            let second = prompt.contains("Exchange — turn 1");
            let hang = match (at, stage) {
                ("plan", Stage::Plan) | ("final", Stage::Final) => agent == "claude",
                ("exchange-1", Stage::Exchange) => !second,
                ("exchange-2", Stage::Exchange) => second,
                ("implement", Stage::Implement) | ("crossreview", Stage::Crossreview) | ("revise", Stage::Revise) => {
                    true
                }
                _ => false,
            };
            if hang {
                hang_agent.store(true, Ordering::SeqCst);
                return Fake::Hang;
            }
            match stage {
                Stage::Exchange if at == "decision" && second => Fake::Reply(fenced(
                    json!({ "message": "Ok.", "plan": "P.", "implementer": "claude", "agreed": true, "conceded": [], "unknowns": ["Which database?"], "backlog": [] }),
                )),
                _ => full(agent, stage, prompt),
            }
        };
        let src = start_full(Duration::from_secs(5), sh, script);
        if at == "decision" {
            until(&src, |s| s.state == RunState::Waiting).await;
            src.handle.control(Control::Message { text: "Use the cache.".into() }).unwrap();
            let messages = src.dir.join("messages.jsonl");
            eventually(|| std::fs::read_to_string(&messages).is_ok_and(|m| m.contains("Use the cache."))).await;
        } else {
            eventually(|| hung.load(Ordering::SeqCst)).await;
        }
        src.handle.control(Control::Cancel { reason: None }).unwrap();
        assert_eq!(settled(&src).await["stopped"], "cancelled by the user");

        let cont = resume_full(&src, |s| git_at(COMMIT, s), full);
        if at == "decision" {
            until(&cont, |s| s.state == RunState::Waiting).await;
            assert_eq!(read_room(&cont.dir).questions, ["Which database?"], "the question is asked again");
            cont.handle.control(Control::Answer { text: "Postgres.".into() }).unwrap();
        }
        cont
    }

    #[tokio::test]
    async fn an_interrupted_group_repeats_and_the_completed_groups_are_skipped() {
        for (k, &(at, stage)) in GROUPS.iter().enumerate() {
            let cont = interrupted(at).await;
            let manifest = settled(&cont).await;
            assert_eq!(manifest["stopped"], Value::Null, "{at}");
            assert_eq!(manifest["ship"], true, "{at}");
            assert_eq!(manifest["base"], BASE, "{at}: the base comes from the checkpoint");

            let record: Value = serde_json::from_slice(&std::fs::read(cont.dir.join("run.json")).unwrap()).unwrap();
            assert_eq!(record["continues"], json!({ "run": "01", "from": at, "repeats": true }), "{at}");
            // The completed groups do not run again.
            for &(_, before) in &GROUPS[..k] {
                if let Some(before) = before
                    && !(at == "exchange-2" && before == Stage::Exchange)
                {
                    assert!(calls_of(&cont, before).is_empty(), "{at}: {before:?} ran again");
                }
            }
            // The interrupted group runs again in full.
            if let Some(stage) = stage {
                let calls = match at {
                    // Turn 1 again, then turn 2.
                    "exchange-1" => 4,
                    _ if matches!(stage, Stage::Plan | Stage::Exchange | Stage::Final) => 2,
                    _ => 1,
                };
                assert_eq!(calls_of(&cont, stage).len(), calls, "{at}");
            }
            let gates = match k {
                0..=5 => 2,
                6..=8 => 1,
                _ => 0,
            };
            assert_eq!(gate_runs(&cont), gates, "{at}");
            // After the last write stage, the continuation has the saved diff of its source.
            assert_eq!(read_diff(&cont.dir).as_deref(), Some("the diff"), "{at}");
            if at == "exchange-2" {
                let prompt = &calls_of(&cont, Stage::Exchange)[0].prompt;
                assert!(prompt.contains("Exchange — turn 1"), "the transcript has turn 1");
            }
            if at == "decision" {
                let prompt = &calls_of(&cont, Stage::Implement)[0].prompt;
                assert_eq!(prompt.matches("Use the cache.").count(), 1, "inherited advice appears once");
                assert!(prompt.contains("(answer, during exchange-2) Postgres."), "{prompt}");
            }
            // The worktree stays as it is: no reset, checkout, clean or apply.
            for argv in cont.shells.lock().unwrap().iter() {
                for word in ["git reset", "git checkout", "git clean", "git apply", "git stash"] {
                    assert!(!argv[2].contains(word), "{at}: {word}");
                }
                if is_diff(&argv[2]) {
                    assert_eq!(argv[6], BASE, "{at}: the diff starts at the original base");
                }
            }
            // The room shows the inherited and the repeated stages.
            let stages = read_room(&cont.dir).stages;
            let label = |id: &str| stages.iter().find(|s| s.id == id).map(|s| s.label.clone()).unwrap_or_default();
            if k > 0 {
                assert_eq!(label("plan"), "Plan (from run 01)", "{at}");
            }
            if at != "decision" {
                assert!(label(at).ends_with("(repeated)"), "{at}: {}", label(at));
            }
            assert!(stages.iter().all(|s| s.state == "done" || s.state == "skipped"), "{at}: {stages:?}");
        }
    }

    #[tokio::test]
    async fn a_pick_before_the_decision_carries_over() {
        let hung = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mark = hung.clone();
        let src = start_full(
            Duration::from_secs(5),
            |s| git_at(BASE, s),
            move |agent, stage, prompt| match stage {
                Stage::Exchange => {
                    mark.store(true, Ordering::SeqCst);
                    Fake::Hang
                }
                _ => full(agent, stage, prompt),
            },
        );
        eventually(|| hung.load(Ordering::SeqCst)).await;
        src.handle.control(Control::PickImplementer { agent: "codex".into() }).unwrap();
        eventually(|| events(&src).iter().any(|(k, _)| k == "pick")).await;
        src.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&src).await;

        let cont = resume_full(&src, |s| git_at(COMMIT, s), full);
        let manifest = settled(&cont).await;
        assert_eq!(manifest["implementer"], "codex");
        assert!(calls_of(&cont, Stage::Exchange).is_empty());
        assert_eq!(calls_of(&cont, Stage::Implement)[0].agent, "codex");
        assert_eq!(read_room(&cont.dir).decided_by.as_deref(), Some("user"));
    }

    #[tokio::test]
    async fn messages_and_contributions_carry_over_once_with_their_budget() {
        let src = start(|agent, stage, prompt| match stage {
            Stage::Implement => Fake::Hang,
            _ => happy(agent, stage, prompt),
        });
        until(&src, |s| s.stage.as_deref() == Some("implement")).await;
        src.handle.control(Control::Message { text: "Keep it small.".into() }).unwrap();
        let Ok(Contribution::Queued(applied)) = src.handle.contribute("note-1", "Mind the cache.") else {
            panic!("the contribution is queued")
        };
        applied.await.unwrap();
        src.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&src).await;

        let gate = Arc::new(Notify::new());
        let held = gate.clone();
        let cont = resume_full(&src, no_git, move |agent, stage, prompt| match (stage, happy(agent, stage, prompt)) {
            (Stage::Implement, Fake::Reply(text)) => Fake::After(held.clone(), text),
            (_, fake) => fake,
        });
        assert_eq!(lock(&cont.handle.budget).count, 2);
        assert!(matches!(cont.handle.contribute("note-1", "Mind the cache."), Ok(Contribution::Applied)));
        assert_eq!(cont.handle.contribute("note-1", "Other text.").unwrap_err(), Refusal::Conflict);
        eventually(|| !calls_of(&cont, Stage::Implement).is_empty()).await;
        gate.notify_one();
        settled(&cont).await;
        let prompt = &calls_of(&cont, Stage::Implement)[0].prompt;
        assert_eq!(prompt.matches("Keep it small.").count(), 1);
        assert_eq!(prompt.matches("Mind the cache.").count(), 1);
        let room = read_room(&cont.dir);
        let kinds: Vec<(&str, &str)> = room
            .messages
            .iter()
            .filter(|m| m.id.starts_with("messages.jsonl"))
            .map(|m| (m.from.as_str(), m.kind.as_str()))
            .collect();
        assert_eq!(kinds, [("user", "user"), ("a2a", "external")]);
        let lines = std::fs::read_to_string(cont.dir.join("messages.jsonl")).unwrap();
        assert!(lines.lines().all(|l| serde_json::from_str::<Value>(l).unwrap()["inherited"] == "01"));
    }

    #[tokio::test]
    async fn a_damaged_or_unsafe_record_gives_no_resume() {
        let src = start(|agent, stage, prompt| match stage {
            Stage::Implement => Fake::Hang,
            _ => happy(agent, stage, prompt),
        });
        until(&src, |s| s.stage.as_deref() == Some("implement")).await;
        src.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&src).await;
        let task_dir = src.dir.parent().unwrap().to_owned();
        let names = vec!["01".to_owned()];
        let load = || TaskRuns::read(&task_dir, &names).load("01").map(|r| (r.from, r.repeats));
        assert_eq!(load().unwrap(), ("implement".to_owned(), true));

        let file = |name: &str| src.dir.join(name);
        std::fs::write(file("messages.jsonl"), "").unwrap();
        type Damage = dyn Fn(String) -> String;
        let cases: [(&str, &Damage, &str); 8] = [
            ("messages.jsonl", &|m| m + "{\"partial", "partial line"),
            (
                "messages.jsonl",
                &|m| m + r#"{"at":"t","stage":"plan","kind":"external","from":"a2a","body":"No ref."}"# + "\n",
                "line 1 of messages.jsonl is damaged",
            ),
            (
                "messages.jsonl",
                &|m| m + r#"{"at":"t","stage":"plan","kind":"pick","body":"x"}"# + "\n",
                "line 1 of messages.jsonl is damaged",
            ),
            ("messages.jsonl", &|m| m + r#"{"at":"t","kind":"user","body":"x"}"# + "\n", "is damaged"),
            ("turns/plan-claude.json", &|p| p.replace("Plan", "Other plan"), "changed after the checkpoint"),
            ("checkpoint.json", &|c| c.replace("\"version\": 2", "\"version\": 3"), "version 3"),
            ("checkpoint.json", &|c| c.replace("\"decision\"", "\"implement\""), "does not agree"),
            (
                "events.jsonl",
                &|e| e + "{\"at\":\"t\",\"kind\":\"run_failed\",\"detail\":\"cannot write x\"}\n",
                "could not write",
            ),
        ];
        for (name, change, why) in cases {
            let before = std::fs::read_to_string(file(name)).unwrap();
            std::fs::write(file(name), change(before.clone())).unwrap();
            let error = load().unwrap_err();
            assert!(error.contains(why), "{name}: {error}");
            std::fs::write(file(name), before).unwrap();
        }
        assert!(load().is_ok());

        // A link is never followed.
        let plan = file("turns/plan-claude.json");
        std::fs::rename(&plan, file("plan-copy.json")).unwrap();
        std::os::unix::fs::symlink(file("plan-copy.json"), &plan).unwrap();
        assert!(load().unwrap_err().contains("not a regular file"));
        std::fs::remove_file(&plan).unwrap();
        std::fs::rename(file("plan-copy.json"), &plan).unwrap();

        std::fs::write(file("run.json"), vec![b' '; (8 << 20) + 1]).unwrap();
        assert!(load().unwrap_err().contains("larger than 8 MiB"));
        std::fs::remove_file(file("run.json")).unwrap();
        assert!(load().unwrap_err().contains("before zwinger saved recovery records"));
    }

    #[tokio::test]
    async fn a_checkpoint_after_a_write_stage_needs_the_saved_diff() {
        let src = start(|agent, stage, prompt| match stage {
            Stage::Crossreview => Fake::Hang,
            _ => happy(agent, stage, prompt),
        });
        until(&src, |s| s.stage.as_deref() == Some("crossreview")).await;
        src.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&src).await;
        let task_dir = src.dir.parent().unwrap().to_owned();
        let names = vec!["01".to_owned()];
        assert!(TaskRuns::read(&task_dir, &names).load("01").is_ok());

        let path = src.dir.join("checkpoint.json");
        let mut checkpoint: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        checkpoint["artifacts"].as_object_mut().unwrap().remove("implementation-diff.txt").unwrap();
        std::fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let error = TaskRuns::read(&task_dir, &names).load("01").unwrap_err();
        assert!(error.contains("implementation-diff.txt is not in it"), "{error}");
    }

    #[test]
    fn the_last_diff_comes_back_from_the_saved_diff() {
        assert_eq!(last_diff("diff --git a b").as_deref(), Some("diff --git a b"));
        assert_eq!(last_diff(""), Some(String::new()));
        assert_eq!(last_diff(NO_GIT_DIFF), None);
        let failed = format!("{DIFF_FAILED}the revision (git ended with code 5).");
        assert_eq!(last_diff(&failed), None);
        assert_eq!(last_diff(&format!("{failed}{LAST_DIFF}the first diff")).as_deref(), Some("the first diff"));
    }

    #[tokio::test]
    async fn a_repeated_revision_keeps_the_last_diff_of_its_source() {
        let src = start_full(
            Duration::from_secs(5),
            |s| if is_diff(s) { Fake::Reply("the first diff".into()) } else { no_git(s) },
            |agent, stage, prompt| match stage {
                Stage::Crossreview => review("revise"),
                Stage::Revise => Fake::Hang,
                _ => happy(agent, stage, prompt),
            },
        );
        until(&src, |s| s.stage.as_deref() == Some("revise")).await;
        src.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&src).await;

        let cont = resume_full(
            &src,
            |s| if is_diff(s) { Fake::Exit(5) } else { no_git(s) },
            |agent, stage, prompt| match stage {
                Stage::Crossreview => review("revise"),
                _ => happy(agent, stage, prompt),
            },
        );
        settled(&cont).await;
        assert_eq!(calls_of(&cont, Stage::Revise).len(), 1);
        let saved = std::fs::read_to_string(cont.dir.join("diff.txt")).unwrap();
        assert!(saved.ends_with("This is the last saved diff.\n\nthe first diff"), "{saved}");
    }

    #[tokio::test]
    async fn a_failed_checkpoint_write_fails_the_run() {
        let run = start(happy);
        // A folder at the temp name makes the durable write fail.
        std::fs::create_dir(run.dir.join("checkpoint.json.tmp~")).unwrap();
        std::fs::write(run.dir.join("checkpoint.json.tmp~/x"), "").unwrap();
        let manifest = settled(&run).await;
        assert!(manifest["stopped"].as_str().unwrap().starts_with("cannot write"), "{manifest}");
        assert_eq!(run.handle.status().state, RunState::Failed);
        assert!(calls_of(&run, Stage::Exchange).is_empty(), "no group after the failed checkpoint");
    }

    fn load(src: &Started) -> std::result::Result<Recovered, String> {
        let task_dir = src.dir.parent().unwrap().to_owned();
        TaskRuns::read(&task_dir, &["01".to_owned()]).load("01")
    }

    fn edit_json(path: &std::path::Path, change: impl FnOnce(&mut Value)) {
        let mut value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        change(&mut value);
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn append(path: &std::path::Path, line: Value) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(file, "{line}").unwrap();
    }

    /// A run that a cancel stopped in `stage`.
    async fn stopped_in(
        stage: Stage,
        sh: impl Fn(&str) -> Fake + Send + Sync + 'static,
        script: impl Fn(&str, Stage, &str) -> Fake + Send + Sync + 'static,
    ) -> Started {
        let src =
            start_full(
                Duration::from_secs(5),
                sh,
                move |agent, s, prompt| {
                    if s == stage { Fake::Hang } else { script(agent, s, prompt) }
                },
            );
        eventually(|| !calls_of(&src, stage).is_empty()).await;
        src.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&src).await;
        src
    }

    #[tokio::test]
    async fn a_new_run_has_its_checkpoint_before_its_first_event() {
        let run = start(|_, _, _| Fake::Hang);
        eventually(|| !calls_of(&run, Stage::Plan).is_empty()).await;
        let checkpoint: Value =
            serde_json::from_slice(&std::fs::read(run.dir.join("checkpoint.json")).unwrap()).unwrap();
        assert_eq!(checkpoint["groups"], json!([]));
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&run).await;
    }

    #[tokio::test]
    async fn a_lost_checkpoint_of_a_started_run_gives_no_resume() {
        let src = stopped_in(Stage::Crossreview, no_git, happy).await;
        assert_eq!(load(&src).unwrap().from, "gates");
        std::fs::remove_file(src.dir.join("checkpoint.json")).unwrap();
        let error = load(&src).unwrap_err();
        assert!(error.contains("checkpoint.json is missing"), "{error}");
    }

    #[tokio::test]
    async fn changed_files_repeat_the_checks_after_the_last_write_stage() {
        type MakeFake = fn(&str) -> Fake;
        let cases: [(&str, MakeFake); 4] = [
            ("same files", |s| git_at(COMMIT, s)),
            ("other HEAD", |s| git_at(BASE, s)),
            ("other worktree or index", |s| {
                if is_snapshot(s) {
                    Fake::Reply(format!("HEAD {COMMIT}\n M a.rs\n\0a change"))
                } else {
                    git_at(COMMIT, s)
                }
            }),
            ("failed snapshot", |s| if is_snapshot(s) { Fake::Exit(4) } else { git_at(COMMIT, s) }),
        ];
        for (case, sh) in cases {
            let src = stopped_in(Stage::Crossreview, |s| git_at(COMMIT, s), happy).await;
            let cont = resume_full(&src, sh, happy);
            settled(&cont).await;
            let same = case == "same files";
            assert_eq!(gate_runs(&cont), usize::from(!same), "{case}");
            assert_eq!(calls_of(&cont, Stage::Crossreview).len(), 1, "{case}");
            assert_eq!(events(&cont).iter().any(|(k, _)| k == "files_changed"), !same, "{case}");
            let gates = read_room(&cont.dir).stages.into_iter().find(|s| s.id == "gates").unwrap();
            let label = if same { "Gates (from run 01)" } else { "Gates" };
            assert_eq!(gates.label, label, "{case}");
        }
    }

    #[tokio::test]
    async fn a_continuation_outside_git_repeats_the_checks() {
        let src = stopped_in(Stage::Final, no_git, happy).await;
        let names = vec!["01".to_owned()];
        assert_eq!(
            TaskRuns::read(src.dir.parent().unwrap(), &names).inspect("01"),
            task_recovery::Resume::Ready {
                from: "gates".to_owned(),
                label: "Gates".to_owned(),
                repeats: true,
                recheck: Some("Gates".to_owned()),
            }
        );
        let cont = resume_full(&src, no_git, happy);
        let manifest = settled(&cont).await;
        assert_eq!(gate_runs(&cont), 1);
        assert_eq!(calls_of(&cont, Stage::Crossreview).len(), 1);
        assert_eq!(manifest["ship"], true, "{manifest}");
    }

    /// The source completed its cross review and stopped in the final
    /// review. The files changed, so the checks run again.
    async fn repeated_checks(crossreview: fn() -> Fake) -> (Started, Started) {
        let src = stopped_in(Stage::Final, |s| git_at(COMMIT, s), happy).await;
        let cont = resume_full(
            &src,
            |s| git_at(BASE, s),
            move |agent, stage, prompt| match stage {
                Stage::Crossreview => crossreview(),
                _ => happy(agent, stage, prompt),
            },
        );
        (src, cont)
    }

    #[tokio::test]
    async fn a_stopped_repeated_check_shows_no_old_result() {
        let (src, cont) = repeated_checks(|| Fake::Hang).await;
        eventually(|| !calls_of(&cont, Stage::Crossreview).is_empty()).await;
        cont.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&cont).await;
        assert!(src.dir.join("crossreview.json").exists(), "the source keeps its result");
        assert!(!cont.dir.join("crossreview.json").exists());
        let room = read_room(&cont.dir);
        assert!(room.crossreview.is_none(), "{:?}", room.crossreview);
        let state = |id: &str| room.stages.iter().find(|s| s.id == id).unwrap().state;
        assert_ne!(state("crossreview"), "done");
        assert_eq!(state("gates"), "done");
        let checkpoint: Value =
            serde_json::from_slice(&std::fs::read(cont.dir.join("checkpoint.json")).unwrap()).unwrap();
        assert!(checkpoint["artifacts"].get("crossreview.json").is_none(), "{checkpoint}");
        let runs = ["01".to_owned(), "02".to_owned()];
        let recovered = TaskRuns::read(cont.dir.parent().unwrap(), &runs).load("02").unwrap();
        assert_eq!(recovered.from, "crossreview");
    }

    #[tokio::test]
    async fn a_degraded_repeated_check_shows_no_old_result() {
        let (_, cont) = repeated_checks(|| Fake::Exit(1)).await;
        let manifest = settled(&cont).await;
        assert_eq!(manifest["crossreview_verdict"], "missing", "{manifest}");
        let room = read_room(&cont.dir);
        let verdict = room.crossreview.as_ref().map(|r| r.verdict.clone());
        assert_ne!(verdict.as_deref(), Some("accept"), "{:?}", room.crossreview);
    }

    /// The crash window after the final checkpoint: the source has no
    /// manifest and no `done` event.
    async fn finals_without_result(ships: [bool; 2], sh: fn(&str) -> Fake) -> Started {
        let src = start_full(Duration::from_secs(5), sh, move |agent, stage, prompt| match stage {
            Stage::Final => ship(ships[usize::from(agent == "codex")]),
            _ => happy(agent, stage, prompt),
        });
        settled(&src).await;
        std::fs::remove_file(src.dir.join("manifest.json")).unwrap();
        let events = std::fs::read_to_string(src.dir.join("events.jsonl")).unwrap();
        let kept: Vec<&str> = events.lines().filter(|l| !l.contains("\"kind\":\"done\"")).collect();
        std::fs::write(src.dir.join("events.jsonl"), kept.join("\n") + "\n").unwrap();
        src
    }

    #[tokio::test]
    async fn a_stop_after_the_final_checkpoint_only_records_the_result() {
        for ships in [[true, true], [true, false]] {
            let src = finals_without_result(ships, |s| git_at(COMMIT, s)).await;
            let recovered = load(&src).unwrap();
            assert_eq!((recovered.from.as_str(), recovered.repeats), (task_recovery::SETTLE, false));
            let cont = resume_full(&src, |s| git_at(COMMIT, s), |_, _, _| Fake::Exit(1));
            let manifest = settled(&cont).await;
            assert!(cont.calls.lock().unwrap().is_empty(), "no agent runs again");
            assert_eq!(gate_runs(&cont), 0);
            assert_eq!(manifest["ship"], ships == [true, true], "{manifest}");
            assert_eq!(manifest["final_reviewers"], json!(["claude", "codex"]));
            let stages = read_room(&cont.dir).stages;
            let ran = stages.iter().filter(|s| s.state != "skipped");
            assert!(ran.clone().all(|s| s.label.ends_with("(from run 01)") && s.state == "done"), "{stages:?}");
        }
        // A run that recorded its result completed all stages.
        let done = start(happy);
        settled(&done).await;
        assert!(load(&done).unwrap_err().contains("completed all stages"));
    }

    #[tokio::test]
    async fn a_settle_only_continuation_outside_git_repeats_the_checks() {
        let src = finals_without_result([true, false], no_git).await;
        let cont = resume_full(&src, no_git, happy);
        let manifest = settled(&cont).await;
        assert_eq!(gate_runs(&cont), 1);
        assert_eq!(calls_of(&cont, Stage::Crossreview).len(), 1);
        assert_eq!(calls_of(&cont, Stage::Final).len(), 2);
        assert_eq!(manifest["ship"], true, "the new final reviews decide");
    }

    #[tokio::test]
    async fn changed_files_after_the_final_checkpoint_repeat_the_checks() {
        let src = finals_without_result([true, false], |s| git_at(COMMIT, s)).await;
        let cont = resume_full(&src, |s| git_at(BASE, s), happy);
        let manifest = settled(&cont).await;
        assert_eq!(gate_runs(&cont), 1);
        assert_eq!(calls_of(&cont, Stage::Crossreview).len(), 1);
        assert_eq!(calls_of(&cont, Stage::Final).len(), 2);
        assert_eq!(manifest["ship"], true, "the new final reviews decide");
    }

    #[tokio::test]
    async fn a_completed_group_needs_the_result_of_each_agent_that_reported() {
        let src = stopped_in(Stage::Revise, no_git, |agent, stage, prompt| match stage {
            Stage::Exchange => turn("claude", true),
            Stage::Crossreview => review("revise"),
            _ => happy(agent, stage, prompt),
        })
        .await;
        assert_eq!(load(&src).unwrap().from, "revise");
        let path = src.dir.join("checkpoint.json");
        let original = std::fs::read(&path).unwrap();
        type Corrupt = dyn Fn(&mut Value);
        let cases: [(&Corrupt, &str); 7] = [
            (
                &|c| drop(c["artifacts"].as_object_mut().unwrap().remove("crossreview.json")),
                "crossreview.json is not in it",
            ),
            (&|c| drop(c["outcomes"].as_object_mut().unwrap().remove("crossreview")), "crossreview has no outcome"),
            (&|c| c["decisions"]["verdict"] = json!("accept"), "the verdict and crossreview.json"),
            (&|c| c["outcomes"]["exchange-1"] = json!(["claude"]), "turns/t1-codex.json has no outcome"),
            (
                &|c| drop(c["artifacts"].as_object_mut().unwrap().remove("turns/t1-codex.json")),
                "turns/t1-codex.json is not in it",
            ),
            (&|c| c["cleanup"] = json!(["../run.json"]), "cleanup files"),
            (&|c| drop(c.as_object_mut().unwrap().remove("fences")), "older incompatible recovery format"),
        ];
        for (change, why) in cases {
            edit_json(&path, change);
            let error = load(&src).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
            std::fs::write(&path, &original).unwrap();
        }
        assert!(load(&src).is_ok());
    }

    #[tokio::test]
    async fn the_budget_is_durable_before_a_message_is_queued() {
        let src = stopped_in(Stage::Implement, no_git, happy).await;
        // Three messages reached the round. A fourth was accepted, then the app stopped before it applied.
        let full = "m".repeat(MESSAGE_LIMIT);
        for n in 0..3 {
            append(
                &src.dir.join("messages.jsonl"),
                json!({ "at": "t", "stage": "plan", "kind": "user", "body": format!("{n}{}", &full[1..]) }),
            );
            append(&src.dir.join("budget.jsonl"), json!({ "at": "t", "kind": "message", "bytes": MESSAGE_LIMIT }));
            append(
                &src.dir.join("budget.jsonl"),
                json!({ "kind": "apply-start", "digest": task_recovery::digest(format!("{n}{}", &full[1..]).as_bytes()) }),
            );
            append(&src.dir.join("budget.jsonl"), json!({ "kind": "apply-complete" }));
        }
        let late = "l".repeat(MESSAGE_LIMIT);
        let digest = task_recovery::digest(late.as_bytes());
        append(
            &src.dir.join("budget.jsonl"),
            json!({ "at": "t", "kind": "external", "bytes": MESSAGE_LIMIT, "ref": "late", "digest": digest }),
        );

        let gate = Arc::new(Notify::new());
        let held = gate.clone();
        let cont = resume_full(&src, no_git, move |agent, stage, prompt| match (stage, happy(agent, stage, prompt)) {
            (Stage::Implement, Fake::Reply(text)) => Fake::After(held.clone(), text),
            (_, fake) => fake,
        });
        let used = {
            let budget = lock(&cont.handle.budget);
            (budget.count, budget.bytes)
        };
        assert_eq!(used, (4, MESSAGES_BYTES));
        let Ok(Contribution::Queued(applied)) = cont.handle.contribute("late", &late) else {
            panic!("the reserved id is queued without a second charge");
        };
        applied.await.unwrap();
        assert_eq!(cont.handle.contribute("late", "Other text.").unwrap_err(), Refusal::Conflict);
        let refused = cont.handle.control(Control::Message { text: "x".into() }).unwrap_err();
        assert!(refused.to_string().contains("limit of 32 messages and 64 KiB"), "{refused}");
        eventually(|| !calls_of(&cont, Stage::Implement).is_empty()).await;
        gate.notify_one();
        settled(&cont).await;
        let prompt = &calls_of(&cont, Stage::Implement)[0].prompt;
        assert!(!prompt.contains(&late), "the implementation prompt was made before the retry");
        assert!(calls_of(&cont, Stage::Crossreview)[0].prompt.contains(&late));
        let ledger = std::fs::read_to_string(cont.dir.join("budget.jsonl")).unwrap();
        assert_eq!(ledger.lines().count(), 12);

        // A budget line that is missing for a message is damage.
        let lines = std::fs::read_to_string(src.dir.join("budget.jsonl")).unwrap();
        let kept: Vec<&str> = lines.lines().skip(1).collect();
        std::fs::write(src.dir.join("budget.jsonl"), kept.join("\n") + "\n").unwrap();
        assert!(load(&src).unwrap_err().contains("budget.jsonl is damaged (bytes)"));
    }

    #[tokio::test]
    async fn a_failed_budget_write_refuses_the_message_and_takes_nothing() {
        let gate = Arc::new(Notify::new());
        let run = held_plan(gate.clone());
        std::fs::create_dir(run.dir.join("budget.jsonl")).unwrap();
        let refused = run.handle.control(Control::Message { text: "Keep it small.".into() }).unwrap_err();
        assert!(refused.to_string().contains("cannot record the message"), "{refused}");
        assert!(matches!(run.handle.contribute("m-1", "Check the cache."), Err(Refusal::Invalid(_))));
        assert_eq!(lock(&run.handle.budget).count, 0);
        assert!(lock(&run.handle.budget).contributions.is_empty());
        gate.notify_one();
        settled(&run).await;
        assert_eq!(run.handle.status().state, RunState::Done, "a refused message does not fail the run");
    }

    #[tokio::test]
    async fn a_failed_record_stays_after_a_cancel() {
        let gate = Arc::new(Notify::new());
        let run = held_plan(gate.clone());
        std::fs::create_dir(run.dir.join("messages.jsonl")).unwrap();
        let Ok(Contribution::Queued(applied)) = run.handle.contribute("m-1", "Check the cache.") else {
            panic!("a new contribution is queued");
        };
        assert!(tokio::time::timeout(Duration::from_secs(5), applied).await.unwrap().is_err());
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        gate.notify_one();
        settled(&run).await;
        assert_eq!(run.handle.status().state, RunState::Failed);
        std::fs::remove_dir(run.dir.join("messages.jsonl")).unwrap();
        assert!(load(&run).unwrap_err().contains("could not write its record"));
    }

    #[tokio::test]
    async fn a_recorded_answer_closes_the_same_questions_after_a_stop() {
        let asked = |agent: &str, stage: Stage, prompt: &str| match stage {
            Stage::Exchange => Fake::Reply(fenced(
                json!({ "message": "Ok.", "plan": "P.", "implementer": "claude", "agreed": true, "conceded": [], "unknowns": ["Which database?"], "backlog": [] }),
            )),
            _ => happy(agent, stage, prompt),
        };
        for (questions, closed) in [("Which database?", true), ("Which cache?", false)] {
            let src = start(asked);
            until(&src, |s| s.state == RunState::Waiting).await;
            src.handle.control(Control::Cancel { reason: None }).unwrap();
            settled(&src).await;
            // The answer and all of the budget were recorded, then the app stopped before the decision checkpoint.
            append(
                &src.dir.join("messages.jsonl"),
                json!({ "at": "t", "stage": "exchange-1", "kind": "answer", "body": "Postgres.", "questions": task_recovery::digest(questions.as_bytes()) }),
            );
            for _ in 0..MESSAGES_COUNT {
                append(&src.dir.join("budget.jsonl"), json!({ "at": "t", "kind": "message", "bytes": 9 }));
            }
            append(
                &src.dir.join("budget.jsonl"),
                json!({ "kind": "apply-start", "digest": task_recovery::digest(b"Postgres.") }),
            );
            append(&src.dir.join("budget.jsonl"), json!({ "kind": "apply-complete" }));
            assert_eq!(load(&src).unwrap().from, "decision");

            let cont = resume_full(&src, no_git, asked);
            if closed {
                settled(&cont).await;
                assert!(!events(&cont).iter().any(|(k, _)| k == "waiting"), "the same questions are not asked again");
                let prompt = &calls_of(&cont, Stage::Implement)[0].prompt;
                assert_eq!(prompt.matches("Postgres.").count(), 1);
                assert_eq!(lock(&cont.handle.budget).count, MESSAGES_COUNT, "no new debit");
            } else {
                until(&cont, |s| s.state == RunState::Waiting).await;
                cont.handle.control(Control::Cancel { reason: None }).unwrap();
                settled(&cont).await;
            }
        }
    }

    #[tokio::test]
    async fn immutable_diff_is_selected_by_the_checkpoint() {
        let serial = Arc::new(AtomicUsize::new(0));
        let next = serial.clone();
        let src = stopped_in(
            Stage::Final,
            move |script| {
                if is_diff(script) {
                    Fake::Reply(format!("diff {}", next.fetch_add(1, Ordering::SeqCst)))
                } else {
                    no_git(script)
                }
            },
            full,
        )
        .await;
        let saved = std::fs::read_to_string(src.dir.join("revision-diff.txt")).unwrap();
        assert_ne!(saved, std::fs::read_to_string(src.dir.join("implementation-diff.txt")).unwrap());
        std::fs::write(src.dir.join("diff.txt"), "uncommitted mirror").unwrap();
        assert_eq!(read_diff(&src.dir).as_deref(), Some(saved.as_str()));
        assert!(load(&src).is_ok());
    }

    #[tokio::test]
    async fn the_downgraded_checkpoint_hides_every_cleanup_prefix() {
        let src = stopped_in(Stage::Final, no_git, happy).await;
        let path = src.dir.join("checkpoint.json");
        let mut checkpoint: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let events = std::fs::read_to_string(src.dir.join("events.jsonl")).unwrap();
        let lines: Vec<&str> = events.lines().collect();
        let cut = lines
            .iter()
            .position(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|e| e["kind"] == "stage" && e["detail"] == "gates")
            })
            .unwrap();
        checkpoint["groups"].as_array_mut().unwrap().retain(|g| g != "gates" && g != "crossreview");
        checkpoint["outcomes"].as_object_mut().unwrap().remove("crossreview");
        checkpoint["decisions"]["verdict"] = Value::Null;
        for name in ["gates.json", "crossreview.json"] {
            checkpoint["artifacts"].as_object_mut().unwrap().remove(name);
        }
        checkpoint["fences"] = json!({});
        checkpoint["events"] = json!(cut);
        checkpoint["cleanup"] = json!(["gates.json", "crossreview.json"]);
        checkpoint["cleaning"] = json!(true);
        std::fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let check = || {
            let room = read_room(&src.dir);
            assert!(room.gates.is_empty());
            assert!(room.crossreview.is_none());
            assert!(room.stages.iter().find(|s| s.id == "gates").is_some_and(|s| s.state == "pending"));
            assert_eq!(load(&src).unwrap().from, "gates");
        };
        check(); // Checkpoint published; old events and files remain.
        std::fs::write(src.dir.join("events.jsonl"), lines[..cut].join("\n") + "\n").unwrap();
        check();
        std::fs::remove_file(src.dir.join("gates.json")).unwrap();
        check();
        std::fs::remove_file(src.dir.join("crossreview.json")).unwrap();
        check();
        append(&src.dir.join("events.jsonl"), json!({ "at": "t", "kind": "files_changed", "detail": "repeat" }));
        check();
        let cont = resume_full(&src, no_git, happy);
        settled(&cont).await;
        assert_eq!(gate_runs(&cont), 1);
        assert_eq!(calls_of(&cont, Stage::Crossreview).len(), 1);
    }

    #[tokio::test]
    async fn each_check_uses_its_own_fence() {
        let crossreview = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let in_review = crossreview.clone();
        let mark = crossreview.clone();
        let src = stopped_in(
            Stage::Final,
            move |script| if in_review.load(Ordering::SeqCst) { git_at(BASE, script) } else { git_at(COMMIT, script) },
            move |agent, stage, prompt| {
                if stage == Stage::Crossreview {
                    mark.store(true, Ordering::SeqCst);
                }
                happy(agent, stage, prompt)
            },
        )
        .await;
        let checkpoint: Value =
            serde_json::from_slice(&std::fs::read(src.dir.join("checkpoint.json")).unwrap()).unwrap();
        assert_ne!(checkpoint["fences"]["gates"], checkpoint["fences"]["crossreview"]);
        let cont = resume_full(&src, |s| git_at(COMMIT, s), happy);
        settled(&cont).await;
        assert_eq!(gate_runs(&cont), 0);
        assert_eq!(calls_of(&cont, Stage::Crossreview).len(), 1);
    }

    #[tokio::test]
    async fn a_repeated_final_review_gets_the_revision_gate_results() {
        let builds = AtomicUsize::new(0);
        let src = start_full(
            Duration::from_secs(5),
            move |s| {
                if !s.contains("pnpm build") {
                    return git_at(COMMIT, s);
                }
                match builds.fetch_add(1, Ordering::SeqCst) {
                    0 => Fake::Exit(37),
                    _ => Fake::Reply("revision build passed".into()),
                }
            },
            full,
        );
        settled(&src).await;
        // The crash window after the final checkpoint, with no fence for the final review.
        std::fs::remove_file(src.dir.join("manifest.json")).unwrap();
        let events = std::fs::read_to_string(src.dir.join("events.jsonl")).unwrap();
        let kept: Vec<&str> = events.lines().filter(|l| !l.contains("\"kind\":\"done\"")).collect();
        std::fs::write(src.dir.join("events.jsonl"), kept.join("\n") + "\n").unwrap();
        edit_json(&src.dir.join("checkpoint.json"), |c| {
            assert!(c["fences"]["revision-gates"].is_object(), "{c}");
            c["fences"].as_object_mut().unwrap().remove("final");
        });
        assert_eq!(load(&src).unwrap().from, "final");

        let cont = resume_full(&src, |s| git_at(COMMIT, s), full);
        let manifest = settled(&cont).await;
        assert_eq!(manifest["stopped"], Value::Null, "{manifest}");
        assert_eq!(gate_runs(&cont), 0);
        let finals = calls_of(&cont, Stage::Final);
        assert_eq!(finals.len(), 2);
        for call in finals {
            assert!(call.prompt.contains("revision build passed"), "{}", call.agent);
            assert!(!call.prompt.contains("\"code\": 37"), "{}", call.agent);
        }
    }

    #[tokio::test]
    async fn a_pending_revision_runs_with_the_original_findings_after_changed_files() {
        let src = stopped_in(Stage::Revise, |s| git_at(COMMIT, s), full).await;
        let names = vec!["01".to_owned()];
        assert_eq!(
            TaskRuns::read(src.dir.parent().unwrap(), &names).inspect("01"),
            task_recovery::Resume::Ready {
                from: "revise".to_owned(),
                label: "Revise".to_owned(),
                repeats: true,
                recheck: None,
            }
        );
        // A second cross review would accept: it must not replace the findings.
        let cont = resume_full(
            &src,
            |s| git_at(BASE, s),
            |agent, stage, prompt| match stage {
                Stage::Crossreview => review("accept"),
                _ => full(agent, stage, prompt),
            },
        );
        let manifest = settled(&cont).await;
        assert_eq!(manifest["stopped"], Value::Null, "{manifest}");
        assert!(calls_of(&cont, Stage::Crossreview).is_empty());
        let revise = calls_of(&cont, Stage::Revise);
        assert_eq!(revise.len(), 1);
        assert!(revise[0].prompt.contains("Bug."), "{}", revise[0].prompt);
        assert_eq!(gate_runs(&cont), 1);
        assert_eq!(calls_of(&cont, Stage::Final).len(), 2);
        assert!(!events(&cont).iter().any(|(k, _)| k == "files_changed"));
        let labels: Vec<(String, String)> = read_room(&cont.dir).stages.into_iter().map(|s| (s.id, s.label)).collect();
        for (id, label) in [
            ("gates", "Gates (from run 01)"),
            ("crossreview", "Cross review (from run 01)"),
            ("revise", "Revise (repeated)"),
            ("revision-gates", "Revision gates"),
            ("final", "Final review"),
        ] {
            assert!(labels.contains(&(id.to_owned(), label.to_owned())), "{id}: {labels:?}");
        }
    }

    #[tokio::test]
    async fn impossible_budget_lines_block_recovery() {
        let src = stopped_in(Stage::Plan, no_git, happy).await;
        let reservation = |bytes: usize| json!({ "kind": "message", "bytes": bytes });
        let external = |id: &str, bytes: usize, body: &str| json!({ "kind": "external", "ref": id, "bytes": bytes, "digest": task_recovery::digest(body.as_bytes()) });
        let applied =
            json!({ "at": "t", "stage": "plan", "kind": "external", "from": "a2a", "ref": "id", "body": "wrong" });
        let cases = [
            (vec![reservation(0)], None, "bytes"),
            (vec![reservation(MESSAGE_LIMIT + 1)], None, "bytes"),
            (vec![reservation(1); MESSAGES_COUNT + 1], None, "message limit"),
            (vec![reservation(MESSAGE_LIMIT); 5], None, "message limit"),
            (vec![external("id", 1, "x"), external("id", 1, "x")], None, "duplicate advisory id"),
            (
                vec![
                    external("id", 5, "right"),
                    json!({ "kind": "apply-start", "ref": "id" }),
                    json!({ "kind": "apply-complete" }),
                ],
                Some(applied.clone()),
                "digest",
            ),
            (
                vec![
                    external("id", 6, "wrong"),
                    json!({ "kind": "apply-start", "ref": "id" }),
                    json!({ "kind": "apply-complete" }),
                ],
                Some(applied),
                "bytes",
            ),
            // Only the last line can be an interrupted write: the budget lock is held across it.
            (
                vec![external("id", 1, "x"), json!({ "kind": "apply-start", "ref": "id" }), external("id2", 1, "y")],
                None,
                "unmatched apply-start",
            ),
            (
                vec![
                    external("id", 1, "x"),
                    json!({ "kind": "apply-start", "ref": "id" }),
                    json!({ "kind": "apply-start", "ref": "id" }),
                ],
                None,
                "unmatched apply-start",
            ),
        ];
        for (ledger, message, why) in cases {
            let content = ledger.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
            std::fs::write(src.dir.join("budget.jsonl"), content).unwrap();
            if let Some(message) = message {
                std::fs::write(src.dir.join("messages.jsonl"), message.to_string() + "\n").unwrap();
            } else {
                let _ = std::fs::remove_file(src.dir.join("messages.jsonl"));
            }
            let error = load(&src).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }

        // After a killed message write, only no new line or one matching new line is possible.
        let user = |body: &str| json!({ "at": "t", "stage": "plan", "kind": "user", "body": body }).to_string() + "\n";
        let ledger =
            [reservation(1), reservation(1), json!({ "kind": "apply-start", "digest": task_recovery::digest(b"x") })];
        let content = ledger.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
        std::fs::write(src.dir.join("budget.jsonl"), content).unwrap();
        for (messages, why) in [
            (user("y"), "not the message that the run was writing"),
            (user("x") + &user("x"), "after an interrupted message write"),
            (user("x") + "{\"partial", "partial line"),
        ] {
            std::fs::write(src.dir.join("messages.jsonl"), messages).unwrap();
            let error = load(&src).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
        for (messages, ledger) in [("", 2), ("{\"partial", 2), (user("x").as_str(), 4)] {
            std::fs::write(src.dir.join("messages.jsonl"), messages).unwrap();
            assert_eq!(load(&src).unwrap().ledger.len(), ledger, "{messages}");
        }

        // Each applied user message uses its own reservation of its size, made before its apply-start.
        let start = |body: &str| json!({ "kind": "apply-start", "digest": task_recovery::digest(body.as_bytes()) });
        let complete = json!({ "kind": "apply-complete" });
        let long = "l".repeat(100);
        for (ledger, messages, why) in [
            (vec![reservation(1), start(&long), complete.clone()], user(&long), Some("bytes")),
            (
                vec![reservation(1), start("a"), complete.clone(), start("b"), complete.clone()],
                user("a") + &user("b"),
                Some("bytes"),
            ),
            (
                vec![reservation(1), reservation(2), start("bb"), complete.clone(), start("a"), complete.clone()],
                user("bb") + &user("a"),
                None,
            ),
            (vec![reservation(7), reservation(1), start("x"), complete.clone()], user("x"), None),
        ] {
            let content = ledger.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
            std::fs::write(src.dir.join("budget.jsonl"), content).unwrap();
            std::fs::write(src.dir.join("messages.jsonl"), &messages).unwrap();
            match why {
                Some(why) => {
                    let error = load(&src).unwrap_err();
                    assert!(error.contains(&format!("budget.jsonl is damaged ({why})")), "{messages}: {error}");
                }
                None => assert!(load(&src).is_ok(), "{messages}"),
            }
        }
    }

    // --- run options -------------------------------------------------------

    fn turns(n: u32) -> RunOptions {
        RunOptions { turns: n, ..RunOptions::default() }
    }

    /// A claude reviewer with another model than the claude participant.
    fn reviewed() -> RunOptions {
        RunOptions {
            independent: Some(TaskAgent { kind: AgentKind::Claude, model: Some("sonnet".into()) }),
            ..RunOptions::default()
        }
    }

    fn exchange_ids(run: &Started) -> Vec<(String, &'static str)> {
        states(&run.dir).into_iter().filter(|(id, _)| id.starts_with("exchange-")).collect()
    }

    fn tiebreak_to(implementer: &str) -> Fake {
        Fake::Reply(fenced(
            json!({ "implementer": implementer, "why": "It knows the files.", "plan": "The tiebreak plan.", "open_questions": [] }),
        ))
    }

    #[tokio::test]
    async fn zero_turns_go_from_the_plans_to_the_tiebreak() {
        let run = start_options(turns(0), |agent, stage, prompt| match stage {
            Stage::Tiebreak => tiebreak_to("codex"),
            _ => happy(agent, stage, prompt),
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["ship"], true);
        assert!(calls_of(&run, Stage::Exchange).is_empty());
        assert_eq!(calls_of(&run, Stage::Tiebreak).len(), 1);
        assert_eq!(calls_of(&run, Stage::Implement)[0].agent, "codex");
        assert!(exchange_ids(&run).is_empty());
        assert!(states(&run.dir).contains(&("tiebreak".to_owned(), "done")));
        let record: Value = serde_json::from_slice(&std::fs::read(run.dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(record["config"]["turns"], 0);
    }

    #[tokio::test]
    async fn one_turn_and_three_turns_run_the_configured_count() {
        for count in [1, 3] {
            let run = start_options(turns(count), |agent, stage, prompt| match stage {
                Stage::Exchange => turn(agent, false),
                Stage::Tiebreak => tiebreak_to("claude"),
                _ => happy(agent, stage, prompt),
            });
            settled(&run).await;
            assert_eq!(calls_of(&run, Stage::Exchange).len(), 2 * count as usize);
            assert_eq!(calls_of(&run, Stage::Tiebreak).len(), 1);
            let expected: Vec<(String, &str)> = (1..=count).map(|n| (format!("exchange-{n}"), "done")).collect();
            assert_eq!(exchange_ids(&run), expected);
            assert_eq!(read_room(&run.dir).turns.len(), count as usize);
        }
    }

    #[tokio::test]
    async fn an_early_agreement_or_a_walkover_skips_the_other_configured_turns() {
        let run = start_options(turns(3), happy);
        settled(&run).await;
        assert_eq!(calls_of(&run, Stage::Exchange).len(), 2);
        assert_eq!(
            exchange_ids(&run),
            [
                ("exchange-1".to_owned(), "done"),
                ("exchange-2".to_owned(), "skipped"),
                ("exchange-3".to_owned(), "skipped")
            ]
        );
        assert!(states(&run.dir).contains(&("tiebreak".to_owned(), "skipped")));

        let options = RunOptions { turns: 3, ..reviewed() };
        let run = start_options(
            options,
            |agent, stage, prompt| {
                if agent == "codex" { Fake::Exit(1) } else { happy(agent, stage, prompt) }
            },
        );
        let manifest = settled(&run).await;
        assert!(calls_of(&run, Stage::Exchange).is_empty());
        assert!(exchange_ids(&run).iter().all(|(_, state)| *state == "skipped"));
        assert_eq!(exchange_ids(&run).len(), 3);
        // The independent approval does not replace a missing final review.
        assert_eq!(read_room(&run.dir).independent.unwrap().verdict.as_deref(), Some("approved"));
        assert_eq!(
            (manifest["ship"].clone(), manifest["ship_basis"].clone()),
            (json!(false), json!("withheld — only claude reviewed"))
        );
    }

    #[tokio::test]
    async fn the_independent_reviewer_approves_in_its_own_role() {
        let run = start_options(reviewed(), |agent, stage, prompt| match stage {
            Stage::Final => {
                Fake::Reply(fenced(json!({ "ship": true, "blocking": [], "residual": [], "task_met": "PAIR-VOTE" })))
            }
            Stage::Independent => Fake::Reply(fenced(
                json!({ "ship": true, "blocking": [], "residual": ["A note."], "task_met": "Done." }),
            )),
            _ => happy(agent, stage, prompt),
        });
        let manifest = settled(&run).await;
        assert_eq!(manifest["ship"], true);
        assert_eq!(manifest["ship_basis"], "both reviewers and the independent reviewer");
        assert_eq!(manifest["independent_verdict"], "approved");
        assert_eq!(manifest["independent"], json!({ "id": "independent", "kind": "claude", "model": "sonnet" }));
        assert_eq!(manifest["participants"].as_array().unwrap().len(), 2);

        // One fresh read-only call with its own model. It gets the change and
        // the gates, but not the votes of the pair.
        let calls = calls_of(&run, Stage::Independent);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].agent, "claude");
        assert!(calls[0].argv.contains(&"sonnet".to_owned()));
        assert!(calls[0].argv.contains(&"--disallowedTools".to_owned()));
        assert!(calls[0].prompt.contains("pnpm build") && calls[0].prompt.contains("## The change"));
        assert!(!calls[0].prompt.contains("PAIR-VOTE"));
        assert!(!calls[0].prompt.contains("### codex — opening plan"));

        // The same kind as a participant writes its own file.
        let file =
            |name: &str| -> Value { serde_json::from_slice(&std::fs::read(run.dir.join(name)).unwrap()).unwrap() };
        assert_eq!(file("final-claude.json")["task_met"], "PAIR-VOTE");
        assert_eq!(file("independent-review.json")["task_met"], "Done.");
        assert!(run.mirror.join("independent-review.json").is_file());
        let checkpoint = file("checkpoint.json");
        assert_eq!(checkpoint["groups"].as_array().unwrap().last().unwrap(), "independent-review");
        assert_eq!(checkpoint["outcomes"]["independent-review"], json!(["independent"]));

        let room = read_room(&run.dir);
        assert_eq!(room.agents, ["claude", "codex"]);
        assert_eq!(room.finals.iter().map(|f| f.agent.as_str()).collect::<Vec<_>>(), ["claude", "codex"]);
        let independent = room.independent.unwrap();
        assert_eq!(independent.reviewer.id, "independent");
        assert_eq!(independent.reviewer.model.as_deref(), Some("sonnet"));
        assert_eq!((independent.state, independent.verdict.as_deref()), ("done", Some("approved")));
        assert_eq!(independent.review.unwrap().residual, ["A note."]);
        assert!(states(&run.dir).ends_with(&[("final".to_owned(), "done"), ("independent-review".to_owned(), "done")]));
        assert!(room.messages.iter().any(|m| m.from == "independent" && m.stage == "independent-review"));
        assert_eq!(room.outcome.unwrap().ship_basis, "both reviewers and the independent reviewer");
    }

    #[tokio::test]
    async fn a_denied_malformed_or_contradictory_review_withholds_ship() {
        let blocking = json!([{ "what": "Bug.", "why": "Wrong.", "where": "a.rs" }]);
        let cases = [
            (
                json!({ "ship": false, "blocking": blocking, "residual": [], "task_met": "No." }),
                "denied",
                "withheld — the independent reviewer did not approve",
            ),
            (
                json!({ "ship": true, "blocking": blocking, "residual": [], "task_met": "Yes." }),
                "invalid",
                "withheld — the independent review is not valid (it approves with blocking findings)",
            ),
            (
                json!({ "ship": true }),
                "invalid",
                "withheld — the independent review is not valid (blocking is not a list)",
            ),
            (
                json!({ "ship": "yes", "blocking": [], "task_met": "Yes." }),
                "invalid",
                "withheld — the independent review is not valid (ship is not true or false)",
            ),
            (
                json!({ "ship": true, "blocking": [] }),
                "invalid",
                "withheld — the independent review is not valid (task_met is not text)",
            ),
            (
                json!({ "verdict": "accept" }),
                "invalid",
                "withheld — the independent review is not valid (ship is not true or false)",
            ),
        ];
        for (reply, verdict, basis) in cases {
            let text = fenced(reply.clone());
            let run = start_options(reviewed(), move |agent, stage, prompt| match stage {
                Stage::Independent => Fake::Reply(text.clone()),
                _ => happy(agent, stage, prompt),
            });
            let manifest = settled(&run).await;
            assert_eq!(run.handle.status().state, RunState::Done, "{reply}");
            assert_eq!(manifest["ship"], false, "{reply}");
            assert_eq!(manifest["independent_verdict"], verdict, "{reply}");
            assert_eq!(manifest["ship_basis"], basis, "{reply}");
            // No automatic repair loop: the pair does not run again.
            assert_eq!(calls_of(&run, Stage::Revise).len(), 0);
            assert_eq!(calls_of(&run, Stage::Independent).len(), 1);
        }
    }

    #[tokio::test]
    async fn a_timed_out_reviewer_withholds_ship() {
        let agents = [
            TaskAgent { kind: AgentKind::Claude, model: Some("opus".into()) },
            TaskAgent { kind: AgentKind::Codex, model: None },
        ];
        let run = start_agents(
            agents,
            reviewed(),
            Duration::from_millis(300),
            no_git,
            |agent, stage, prompt| {
                if stage == Stage::Independent { Fake::Hang } else { happy(agent, stage, prompt) }
            },
            false,
        );
        let manifest = settled(&run).await;
        assert_eq!(run.handle.status().state, RunState::Done);
        assert_eq!(manifest["ship"], false);
        assert_eq!(manifest["independent_verdict"], "missing");
        assert_eq!(manifest["ship_basis"], "withheld — the independent reviewer gave no review");
        assert!(!run.dir.join("independent-review.json").exists());
        let room = read_room(&run.dir);
        assert!(room.degraded.iter().any(|d| d.stage == "independent-review"));
        assert_eq!(room.independent.unwrap().verdict.as_deref(), Some("missing"));
    }

    #[tokio::test]
    async fn without_a_reviewer_the_pair_rule_is_unchanged() {
        let run = start(happy);
        let manifest = settled(&run).await;
        assert_eq!((manifest["ship"].clone(), manifest["ship_basis"].clone()), (json!(true), json!("both reviewers")));
        assert_eq!(
            (manifest["independent"].clone(), manifest["independent_verdict"].clone()),
            (Value::Null, Value::Null)
        );
        assert!(calls_of(&run, Stage::Independent).is_empty());
        let room = read_room(&run.dir);
        assert!(room.independent.is_none());
        assert!(!room.stages.iter().any(|s| s.id == "independent-review"));
        let record: Value = serde_json::from_slice(&std::fs::read(run.dir.join("run.json")).unwrap()).unwrap();
        assert!(record.get("independent").is_none());
    }

    #[tokio::test]
    async fn an_interrupted_independent_review_repeats_alone() {
        let agents = [
            TaskAgent { kind: AgentKind::Claude, model: Some("opus".into()) },
            TaskAgent { kind: AgentKind::Codex, model: None },
        ];
        let sh = |script: &str| git_at(COMMIT, script);
        let src = start_agents(
            agents,
            reviewed(),
            Duration::from_secs(5),
            sh,
            |agent, stage, prompt| if stage == Stage::Independent { Fake::Hang } else { happy(agent, stage, prompt) },
            false,
        );
        eventually(|| !calls_of(&src, Stage::Independent).is_empty()).await;
        src.handle.control(Control::Cancel { reason: None }).unwrap();
        let stopped = settled(&src).await;
        assert_eq!((src.handle.status().state, stopped["ship"].clone()), (RunState::Cancelled, json!(false)));
        let recovered = load(&src).unwrap();
        assert_eq!((recovered.from.as_str(), recovered.repeats), ("independent-review", true));

        let next = resume_full(&src, sh, happy);
        let manifest = settled(&next).await;
        assert_eq!(manifest["ship"], true);
        assert_eq!(manifest["ship_basis"], "both reviewers and the independent reviewer");
        assert_eq!(gate_runs(&next), 0);
        assert!(calls_of(&next, Stage::Final).is_empty());
        assert_eq!(calls_of(&next, Stage::Independent).len(), 1);
        let record: Value = serde_json::from_slice(&std::fs::read(next.dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(record["independent"], json!({ "kind": "claude", "model": "sonnet" }));
        // A completed review is not repeated: the run only settles.
        let task_dir = next.dir.parent().unwrap().to_owned();
        let names = ["01".to_owned(), "02".to_owned()];
        let done = TaskRuns::read(&task_dir, &names).load("02");
        assert_eq!(done.unwrap_err(), "the run completed all stages");
    }
}
