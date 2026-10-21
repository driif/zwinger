//! Claude Code and Codex. They have no RPC like pi, so this module does the
//! chat commands itself: it starts a process per chat (Claude Code) or per
//! turn (Codex), answers `get_state` and `get_messages`, and keeps each chat
//! as a transcript in a host-only file (`<app_data>/chats/<shield>/<agent>`).
//! The guest cannot write that dir, so the files are trusted.

use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;

use super::{AgentCommand, AgentKind, Chat, Framer, Line, Sink, TITLE_CHARS, claude, codex, cut, keep, lock, send_now};
use crate::error::{Error, Result};
use crate::runtime::{Pipe, PipeEvent, PipeInput};

/// Opens a process in the shield's VM, in the cwd if it is set, else in the
/// harness. Returns the cwd that it used.
pub type Spawn =
    Box<dyn Fn(Vec<String>, Option<String>) -> Pin<Box<dyn Future<Output = Result<(Pipe, String)>> + Send>> + Send + Sync>;

/// How long an interrupted Claude Code turn has to end before it is killed.
const ABORT_LIMIT: Duration = Duration::from_secs(5);
/// How long a process that lost its chat has to exit before it is killed.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);
/// How long the check of a cwd may take.
const PROBE_LIMIT: Duration = Duration::from_secs(10);
/// The start of a metadata line, so a listing skips the message lines.
const META: &str = "{\"type\":\"meta\"";

/// A transcript line. A metadata line changes the fields that it has.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Entry {
    Meta(Meta),
    Message { message: Value },
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    /// The guest cwd of the Claude Code session: Claude Code keeps its
    /// sessions per cwd, so a resume must start there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

impl Meta {
    fn merge(&mut self, change: Meta) {
        self.agent = change.agent.or(self.agent);
        self.started = change.started.or(self.started.take());
        // An empty session id clears it: `None` keeps the old one.
        self.session_id = change.session_id.or(self.session_id.take()).filter(|s| !s.is_empty());
        self.cwd = change.cwd.or(self.cwd.take());
        self.model = change.model.or(self.model.take());
        self.title = change.title.or(self.title.take());
    }
}

/// The path of a chat id, if the id is one that [`Transcript::create`] makes.
fn chat_path(dir: &Path, id: &str) -> Option<PathBuf> {
    let uuid = uuid::Uuid::parse_str(id.strip_suffix(".jsonl")?).ok()?;
    (format!("{uuid}.jsonl") == id).then(|| dir.join(id))
}

/// Reads the metadata and, if `messages` is set, the messages of a transcript.
fn read(path: &Path, messages: bool) -> Option<(Meta, Vec<Value>)> {
    let file = std::fs::File::open(path).ok()?;
    let (mut meta, mut list) = (Meta::default(), Vec::new());
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        if !messages && !line.starts_with(META) {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(Entry::Meta(change)) => meta.merge(change),
            Ok(Entry::Message { message }) => list.push(message),
            Err(_) => {}
        }
    }
    Some((meta, list))
}

/// One chat of Claude Code or Codex.
struct Transcript {
    id: String,
    path: PathBuf,
    meta: Meta,
    messages: Vec<Value>,
}

impl Transcript {
    fn create(dir: &Path, kind: AgentKind, model: Option<String>) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let id = format!("{}.jsonl", uuid::Uuid::new_v4());
        let path = dir.join(&id);
        let meta = Meta {
            agent: Some(kind),
            started: Some(iso(now_ms())),
            model,
            ..Meta::default()
        };
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        file.write_all(&entry_line(&Entry::Meta(meta.clone()))?)?;
        Ok(Self {
            id,
            path,
            meta,
            messages: Vec::new(),
        })
    }

    fn open(dir: &Path, kind: AgentKind, id: &str) -> Result<Self> {
        let path = chat_path(dir, id).ok_or(Error::ChatNotFound)?;
        match read(&path, true) {
            Some((meta, messages)) if meta.agent == Some(kind) => Ok(Self {
                id: id.to_owned(),
                path,
                meta,
                messages,
            }),
            _ => Err(Error::ChatNotFound),
        }
    }

    /// Never creates the file, so a deleted chat stays deleted. A failed
    /// write loses the line, not the chat that is shown.
    fn append(&self, entry: &Entry) {
        let written = entry_line(entry).and_then(|line| {
            let mut file = OpenOptions::new().append(true).open(&self.path)?;
            Ok(file.write_all(&line)?)
        });
        if let Err(e) = written {
            eprintln!("cannot write chat {}: {e}", self.path.display());
        }
    }

    fn set(&mut self, change: Meta) {
        self.append(&Entry::Meta(change.clone()));
        self.meta.merge(change);
    }

    fn push(&mut self, message: Value) {
        self.append(&Entry::Message { message: message.clone() });
        self.messages.push(message);
    }
}

fn entry_line(entry: &Entry) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    Ok(line)
}

/// Lists the chats of one agent. Only the files that this module made count.
pub fn list_chats(dir: &Path, kind: AgentKind) -> Vec<Chat> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().into_string().ok()?;
            let (meta, _) = read(&chat_path(dir, &id)?, false)?;
            (meta.agent == Some(kind)).then_some(())?;
            Some(Chat {
                agent: kind,
                id,
                started: meta.started?,
                title: meta.title.unwrap_or_else(|| "New chat".to_owned()),
            })
        })
        .collect()
}

pub fn delete_chat(dir: &Path, id: &str) -> Result<()> {
    let path = chat_path(dir, id).ok_or(Error::ChatNotFound)?;
    std::fs::remove_file(path).map_err(|_| Error::ChatNotFound)
}

pub fn rename_chat(dir: &Path, kind: AgentKind, id: &str, title: &str) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        return Err(Error::InvalidName(title.to_owned()));
    }
    let mut chat = Transcript::open(dir, kind, id)?;
    chat.set(Meta {
        title: Some(title.chars().take(TITLE_CHARS).collect()),
        ..Meta::default()
    });
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The ISO 8601 form of a time in ms since the epoch, as pi writes it.
fn iso(ms: i64) -> String {
    let (secs, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // The civil date of a day number (Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms").
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The title of a chat: the start of its first message.
fn title_of(message: &str) -> String {
    let title = message.split_whitespace().collect::<Vec<_>>().join(" ");
    title.chars().take(TITLE_CHARS).collect()
}

#[derive(Default)]
struct Inner {
    /// Set by close or kill. No process starts after that.
    closed: bool,
    finished: bool,
    /// `None` uses the default model of the CLI.
    model: Option<String>,
    /// `None` until the first prompt of a new chat.
    chat: Option<Transcript>,
    running: bool,
    aborting: bool,
    /// The process of the chat (Claude Code) or of the turn (Codex), with
    /// its number. Output of an older process is dropped.
    process: Option<(u64, Sender<PipeInput>)>,
    starting: bool,
    processes: u64,
    /// The last message timestamp. Each message gets a new one: the chat
    /// takes two messages with the same role and timestamp as one.
    stamp: i64,
}

impl Inner {
    fn is_current(&self, number: u64) -> bool {
        self.process.as_ref().is_some_and(|(n, _)| *n == number)
    }

    fn stamp(&mut self) -> i64 {
        self.stamp = now_ms().max(self.stamp + 1);
        self.stamp
    }
}

/// The event mapping of one process.
enum Mapper {
    Claude(claude::Mapper),
    Codex(codex::Mapper),
}

/// The Claude Code or Codex agent of a shield.
pub struct CliAgent {
    kind: AgentKind,
    /// The host dir of the agent's transcripts.
    dir: PathBuf,
    spawn: Spawn,
    sink: Mutex<Sink>,
    inner: Mutex<Inner>,
    /// Runs once, before `agent_exit`, so a reopen in reaction to the event
    /// starts a new agent.
    on_finish: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    done: watch::Sender<bool>,
    /// The processes that lost their chat, by number, until they exit. A new
    /// process and the finish wait for them. Changed with `inner` locked.
    closing: watch::Sender<HashMap<u64, Sender<PipeInput>>>,
}

impl CliAgent {
    pub fn new(kind: AgentKind, dir: PathBuf, spawn: Spawn, sink: Sink, on_finish: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            kind,
            dir,
            spawn,
            sink: Mutex::new(sink),
            inner: Mutex::new(Inner::default()),
            on_finish: Mutex::new(Some(on_finish)),
            done: watch::Sender::new(false),
            closing: watch::Sender::new(HashMap::new()),
        }
    }

    pub fn set_sink(&self, sink: Sink) {
        *lock(&self.sink) = sink;
    }

    fn emit(&self, record: Value) {
        (lock(&self.sink))(record);
    }

    /// Ends the idle process of the chat, so the next prompt starts one with
    /// the new chat or model. The process is killed if it does not exit
    /// within `CLOSE_LIMIT`.
    fn end_process(self: &Arc<Self>, inner: &mut Inner) {
        let Some((number, input)) = inner.process.take() else {
            return;
        };
        send_now(&input, PipeInput::Close);
        self.closing.send_modify(|closing| {
            closing.insert(number, input);
        });
        let agent = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CLOSE_LIMIT).await;
            if let Some(input) = agent.closing.borrow().get(&number) {
                send_now(input, PipeInput::Kill);
            }
        });
    }

    /// Forgets the current chat if it is `chat`, and tells the UI with a
    /// `chat_reset` record. Fails while it runs.
    pub fn forget(self: &Arc<Self>, chat: &str) -> Result<()> {
        let mut inner = lock(&self.inner);
        if inner.chat.as_ref().is_some_and(|c| c.id == chat) {
            if inner.running {
                return Err(Error::AgentBusy);
            }
            inner.chat = None;
            self.end_process(&mut inner);
            self.emit(json!({ "type": "chat_reset" }));
        }
        Ok(())
    }

    pub async fn call(self: &Arc<Self>, command: AgentCommand) -> Result<Value> {
        match command {
            AgentCommand::Prompt { message } => self.prompt(message).await.map(|()| Value::Null),
            AgentCommand::Abort => {
                self.abort();
                Ok(Value::Null)
            }
            AgentCommand::GetState => Ok(self.state()),
            AgentCommand::GetMessages => {
                let inner = lock(&self.inner);
                let messages = inner.chat.as_ref().map(|c| c.messages.clone()).unwrap_or_default();
                Ok(json!({ "messages": messages }))
            }
            AgentCommand::SetModel { model_id, .. } => {
                let model = Some(model_id.trim().to_owned()).filter(|m| !m.is_empty());
                self.idle(|inner| {
                    if let Some(chat) = &mut inner.chat {
                        // An empty model is the default model: `None` keeps the old one.
                        chat.set(Meta {
                            model: Some(model.clone().unwrap_or_default()),
                            ..Meta::default()
                        });
                    }
                    inner.model = model;
                    self.end_process(inner);
                })
            }
            AgentCommand::NewSession => self.idle(|inner| {
                inner.chat = None;
                self.end_process(inner);
            }),
            AgentCommand::SwitchSession { chat } => {
                let (dir, kind) = (self.dir.clone(), self.kind);
                let chat = tokio::task::spawn_blocking(move || Transcript::open(&dir, kind, &chat))
                    .await
                    .map_err(|e| Error::Io(std::io::Error::other(e)))??;
                self.idle(|inner| {
                    inner.model = chat.meta.model.clone().filter(|m| !m.is_empty());
                    inner.chat = Some(chat);
                    self.end_process(inner);
                })
            }
            AgentCommand::SetSessionName { name } => {
                if let Some(chat) = &mut lock(&self.inner).chat {
                    chat.set(Meta {
                        title: Some(name),
                        ..Meta::default()
                    });
                }
                Ok(Value::Null)
            }
            AgentCommand::Steer { .. } | AgentCommand::FollowUp { .. } => Err(Error::Unsupported("messages while it works")),
            AgentCommand::GetAvailableModels => Err(Error::Unsupported("a model list")),
            AgentCommand::SetThinkingLevel { .. } => Err(Error::Unsupported("thinking levels")),
            AgentCommand::GetSessionStats => Err(Error::Unsupported("chat stats")),
        }
    }

    /// Does a chat change. Fails while a turn runs.
    fn idle(&self, change: impl FnOnce(&mut Inner)) -> Result<Value> {
        let mut inner = lock(&self.inner);
        if inner.closed {
            return Err(Error::AgentClosed);
        }
        if inner.running {
            return Err(Error::AgentBusy);
        }
        change(&mut inner);
        Ok(Value::Null)
    }

    fn state(&self) -> Value {
        let inner = lock(&self.inner);
        let model = inner.model.as_ref().map(|m| {
            json!({ "id": m, "name": m, "provider": self.kind.name(), "reasoning": false })
        });
        json!({
            "isStreaming": inner.running,
            "model": model,
            "thinkingLevel": "off",
            "sessionFile": inner.chat.as_ref().map(|c| c.id.clone()),
        })
    }

    /// Saves the message of a record and gives it a timestamp.
    fn record(&self, inner: &mut Inner, mut record: Value) {
        if record["type"] == "message_end" {
            record["message"]["timestamp"] = inner.stamp().into();
            if inner.aborting && record["message"]["stopReason"] == "error" {
                record["message"]["stopReason"] = "aborted".into();
            }
            if let Some(chat) = &mut inner.chat {
                chat.push(record["message"].clone());
            }
        }
        if record["type"] == "agent_settled" {
            inner.running = false;
            inner.aborting = false;
        }
        self.emit(record);
    }

    /// Ends a turn that its process did not end.
    fn settle(&self, inner: &mut Inner, error: String) {
        if !inner.running {
            return;
        }
        let message = if inner.aborting {
            json!({ "role": "assistant", "content": [], "stopReason": "aborted" })
        } else {
            json!({ "role": "assistant", "content": [], "stopReason": "error", "errorMessage": error })
        };
        self.record(inner, json!({ "type": "message_end", "message": message }));
        self.record(inner, json!({ "type": "agent_settled" }));
    }

    async fn prompt(self: &Arc<Self>, message: String) -> Result<()> {
        let (number, session, cwd) = {
            let mut inner = lock(&self.inner);
            if inner.closed {
                return Err(Error::AgentClosed);
            }
            if inner.running {
                return Err(Error::AgentBusy);
            }
            if inner.chat.is_none() {
                inner.chat = Some(Transcript::create(&self.dir, self.kind, inner.model.clone())?);
            }
            let timestamp = inner.stamp();
            let chat = inner.chat.as_mut().expect("chat");
            if chat.meta.title.is_none() {
                chat.set(Meta {
                    title: Some(title_of(&message)),
                    ..Meta::default()
                });
            }
            let (session, cwd) = (chat.meta.session_id.clone(), chat.meta.cwd.clone());
            let user = json!({ "role": "user", "content": [{ "type": "text", "text": message }], "timestamp": timestamp });
            chat.push(user.clone());
            inner.running = true;
            inner.aborting = false;
            self.emit(json!({ "type": "message_end", "message": user }));
            self.emit(json!({ "type": "agent_start" }));
            if self.kind == AgentKind::Claude
                && let Some((_, input)) = &inner.process
            {
                send_now(input, PipeInput::Data(claude::user_line(&message)));
                return Ok(());
            }
            inner.processes += 1;
            inner.starting = true;
            (inner.processes, session, cwd)
        };

        // A process of the chat before may still write the session files.
        let _ = self.closing.subscribe().wait_for(HashMap::is_empty).await;
        let (session, cwd) = match (self.kind, session) {
            (AgentKind::Claude, Some(session)) => match self.resume_cwd(cwd).await {
                Some(cwd) => (Some(session), Some(cwd)),
                None => (None, None),
            },
            (_, session) => (session, None),
        };
        let argv = {
            let model = lock(&self.inner).model.clone();
            match self.kind {
                AgentKind::Claude => claude::argv(session.as_deref(), model.as_deref()),
                _ => codex::argv(session.as_deref(), model.as_deref()),
            }
        };

        let pipe = (self.spawn)(argv, cwd).await;
        let finish = {
            let mut inner = lock(&self.inner);
            inner.starting = false;
            match &pipe {
                Ok((pipe, used)) if !inner.closed => {
                    match self.kind {
                        AgentKind::Claude => {
                            send_now(&pipe.input, PipeInput::Data(claude::user_line(&message)));
                            if let Some(chat) = &mut inner.chat
                                && chat.meta.cwd.as_deref() != Some(used)
                            {
                                chat.set(Meta {
                                    cwd: Some(used.clone()),
                                    ..Meta::default()
                                });
                            }
                        }
                        // Codex reads the prompt from stdin until it closes.
                        _ => {
                            send_now(&pipe.input, PipeInput::Data(message.clone().into_bytes()));
                            send_now(&pipe.input, PipeInput::Close);
                        }
                    }
                    if inner.aborting {
                        send_now(&pipe.input, PipeInput::Kill);
                    }
                    inner.process = Some((number, pipe.input.clone()));
                    false
                }
                Ok((pipe, _)) => {
                    send_now(&pipe.input, PipeInput::Kill);
                    !std::mem::replace(&mut inner.finished, true)
                }
                Err(e) => {
                    self.settle(&mut inner, e.to_string());
                    inner.closed && !std::mem::replace(&mut inner.finished, true)
                }
            }
        };
        if finish {
            self.finish(0, VecDeque::new());
        }
        let (pipe, _) = pipe?;
        if lock(&self.inner).is_current(number) {
            tokio::spawn(self.clone().run(number, pipe.events));
            Ok(())
        } else {
            Err(Error::AgentClosed)
        }
    }

    /// The saved cwd of the Claude Code session, if the session can resume
    /// there. Else the session id is cleared for good, the chat gets a
    /// notice, and a new session starts in the harness.
    async fn resume_cwd(&self, cwd: Option<String>) -> Option<String> {
        let notice = match &cwd {
            None => "This chat has no saved folder, and Claude Code keeps its sessions per folder. \
                     So a new Claude Code session starts: it does not remember the earlier messages."
                .to_owned(),
            Some(dir) if self.is_gone(dir).await => format!(
                "The folder of this chat ({dir}) is gone. So a new Claude Code session starts: \
                 it does not remember the earlier messages."
            ),
            Some(_) => return cwd,
        };
        let mut inner = lock(&self.inner);
        if let Some(chat) = &mut inner.chat {
            chat.set(Meta {
                session_id: Some(String::new()),
                ..Meta::default()
            });
        }
        let notice = json!({ "role": "notice", "content": [{ "type": "text", "text": notice }] });
        self.record(&mut inner, json!({ "type": "message_end", "message": notice }));
        None
    }

    /// Checks with `test -d`. Only its exit code 1 means gone: a probe that
    /// fails keeps the session.
    async fn is_gone(&self, dir: &str) -> bool {
        let argv = ["test", "-d", dir].map(str::to_owned).to_vec();
        let Ok((mut pipe, _)) = (self.spawn)(argv, Some("/".to_owned())).await else {
            return false;
        };
        send_now(&pipe.input, PipeInput::Close);
        let exit = async {
            while let Some(event) = pipe.events.recv().await {
                if let PipeEvent::Exit(code) = event {
                    return code;
                }
            }
            -1
        };
        match tokio::time::timeout(PROBE_LIMIT, exit).await {
            Ok(code) => code == 1,
            Err(_) => {
                send_now(&pipe.input, PipeInput::Kill);
                false
            }
        }
    }

    fn abort(self: &Arc<Self>) {
        let mut inner = lock(&self.inner);
        if !inner.running || inner.aborting {
            return;
        }
        inner.aborting = true;
        // A process that is still starting is killed when it is up.
        let Some((number, input)) = inner.process.clone() else {
            return;
        };
        if self.kind == AgentKind::Codex {
            return send_now(&input, PipeInput::Kill);
        }
        send_now(&input, PipeInput::Data(claude::interrupt_line(number)));
        let agent = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(ABORT_LIMIT).await;
            let inner = lock(&agent.inner);
            if inner.aborting && inner.is_current(number) {
                send_now(&input, PipeInput::Kill);
            }
        });
    }

    /// Reads the output of one process until it ends.
    async fn run(self: Arc<Self>, number: u64, mut events: Receiver<PipeEvent>) {
        let (mut stdout, mut stderr_framer) = (Framer::default(), Framer::default());
        let mut stderr = VecDeque::new();
        let mut mapper = match self.kind {
            AgentKind::Claude => Mapper::Claude(claude::Mapper::default()),
            _ => Mapper::Codex(codex::Mapper::default()),
        };
        let mut code = -1;
        while let Some(event) = events.recv().await {
            match event {
                PipeEvent::Stdout(bytes) => {
                    for line in stdout.push(&bytes) {
                        match line {
                            Line::Record(line) => match serde_json::from_slice::<Value>(&line) {
                                Ok(event) => self.handle(number, &event, &mut mapper),
                                Err(_) => keep(&mut stderr, cut(&line)),
                            },
                            Line::TooLong => keep(&mut stderr, "output line over 16 MB".to_owned()),
                        }
                    }
                }
                PipeEvent::Stderr(bytes) => {
                    for line in stderr_framer.push(&bytes) {
                        match line {
                            Line::Record(line) => keep(&mut stderr, cut(&line)),
                            Line::TooLong => keep(&mut stderr, "stderr line over 16 MB".to_owned()),
                        }
                    }
                }
                PipeEvent::Exit(c) => {
                    code = c;
                    break;
                }
            }
        }
        let finish = {
            let mut inner = lock(&self.inner);
            if inner.is_current(number) {
                inner.process = None;
                let mut error = format!("The process ended with code {code}.");
                for line in &stderr {
                    error.push('\n');
                    error.push_str(line);
                }
                self.settle(&mut inner, error);
            }
            self.closing.send_modify(|closing| {
                closing.remove(&number);
            });
            inner.closed
                && inner.process.is_none()
                && !inner.starting
                && self.closing.borrow().is_empty()
                && !std::mem::replace(&mut inner.finished, true)
        };
        if finish {
            self.finish(code, stderr);
        }
    }

    fn handle(&self, number: u64, event: &Value, mapper: &mut Mapper) {
        let (records, session) = match mapper {
            Mapper::Claude(mapper) => (mapper.map(event), claude::session_id(event)),
            Mapper::Codex(mapper) => (mapper.map(event), codex::session_id(event)),
        };
        let mut inner = lock(&self.inner);
        // An ended process, such as the one of the chat before.
        if !inner.is_current(number) {
            return;
        }
        if let Some(session) = session
            && let Some(chat) = &mut inner.chat
            && chat.meta.session_id.as_deref() != Some(session)
        {
            chat.set(Meta {
                session_id: Some(session.to_owned()),
                ..Meta::default()
            });
        }
        for record in records {
            self.record(&mut inner, record);
        }
    }

    /// Asks the agent to exit: Claude Code ends when its stdin closes; a
    /// Codex turn is killed.
    pub fn close(self: &Arc<Self>) {
        let input = match self.kind {
            AgentKind::Claude => PipeInput::Close,
            _ => PipeInput::Kill,
        };
        self.end(input);
    }

    pub fn kill(self: &Arc<Self>) {
        self.end(PipeInput::Kill);
    }

    /// The caller can hold the shield state, which `on_finish` locks, so an
    /// agent with no process finishes in a task. A kill also ends the
    /// processes that close.
    fn end(self: &Arc<Self>, input: PipeInput) {
        let mut inner = lock(&self.inner);
        inner.closed = true;
        if matches!(input, PipeInput::Kill) {
            for process in self.closing.borrow().values() {
                send_now(process, PipeInput::Kill);
            }
        }
        if let Some((_, process)) = &inner.process {
            return send_now(process, input);
        }
        if !inner.starting && self.closing.borrow().is_empty() && !std::mem::replace(&mut inner.finished, true) {
            let agent = self.clone();
            tokio::spawn(async move { agent.finish(0, VecDeque::new()) });
        }
    }

    fn finish(&self, code: i32, stderr: VecDeque<String>) {
        if let Some(on_finish) = lock(&self.on_finish).take() {
            on_finish();
        }
        self.emit(json!({ "type": "agent_exit", "code": code, "stderr": stderr }));
        self.done.send_replace(true);
    }

    /// Waits until the agent has finished.
    pub async fn exited(&self) {
        let _ = self.done.subscribe().wait_for(|done| *done).await;
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::runtime::PIPE_QUEUE;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Plays the CLI on the other end of each opened pipe.
    struct Process {
        argv: Vec<String>,
        cwd: Option<String>,
        stdin: mpsc::Receiver<PipeInput>,
        out: mpsc::Sender<PipeEvent>,
    }

    impl Process {
        async fn input(&mut self) -> PipeInput {
            tokio::time::timeout(Duration::from_secs(2), self.stdin.recv()).await.unwrap().unwrap()
        }

        async fn send(&self, line: &str) {
            self.out.send(PipeEvent::Stdout(format!("{line}\n").into_bytes())).await.unwrap();
        }

        async fn exit(&self, code: i32) {
            self.out.send(PipeEvent::Exit(code)).await.unwrap();
        }
    }

    struct Setup {
        agent: Arc<CliAgent>,
        processes: mpsc::UnboundedReceiver<Process>,
        records: mpsc::UnboundedReceiver<Value>,
        finished: Arc<std::sync::atomic::AtomicBool>,
        dir: PathBuf,
        /// The cwd of a process that gets none.
        harness: Arc<Mutex<String>>,
    }

    fn setup(kind: AgentKind) -> Setup {
        let (opened, processes) = mpsc::unbounded_channel();
        let harness = Arc::new(Mutex::new("/workspace".to_owned()));
        let default = harness.clone();
        let spawn: Spawn = Box::new(move |argv, cwd| {
            let opened = opened.clone();
            let used = cwd.clone().unwrap_or_else(|| default.lock().unwrap().clone());
            Box::pin(async move {
                let (input, stdin) = mpsc::channel(PIPE_QUEUE);
                let (out, events) = mpsc::channel(PIPE_QUEUE);
                let _ = opened.send(Process { argv, cwd, stdin, out });
                Ok((Pipe { input, events }, used))
            })
        });
        let (tx, records) = mpsc::unbounded_channel();
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = finished.clone();
        let dir = temp_dir().join(kind.name());
        let agent = CliAgent::new(
            kind,
            dir.clone(),
            spawn,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
            Box::new(move || flag.store(true, std::sync::atomic::Ordering::SeqCst)),
        );
        Setup {
            agent: Arc::new(agent),
            processes,
            records,
            finished,
            dir,
            harness,
        }
    }

    impl Setup {
        async fn process(&mut self) -> Process {
            tokio::time::timeout(Duration::from_secs(2), self.processes.recv()).await.unwrap().unwrap()
        }

        /// Answers the `test -d` of `dir` with `code`.
        async fn probe(&mut self, dir: &str, code: i32) {
            let mut probe = self.process().await;
            assert_eq!((probe.argv.join(" "), probe.cwd.as_deref()), (format!("test -d {dir}"), Some("/")));
            assert!(matches!(probe.input().await, PipeInput::Close));
            probe.exit(code).await;
        }

        async fn record(&mut self) -> Value {
            tokio::time::timeout(Duration::from_secs(2), self.records.recv()).await.unwrap().unwrap()
        }

        /// Skips records up to one of type `kind`.
        async fn until(&mut self, kind: &str) -> Value {
            loop {
                let record = self.record().await;
                if record["type"] == kind {
                    return record;
                }
            }
        }

        async fn call(&self, command: AgentCommand) -> Result<Value> {
            self.agent.call(command).await
        }

        /// A prompt that waits for a probe or a closing process returns
        /// only after the test plays them.
        fn prompt_later(&self, message: &str) -> tokio::task::JoinHandle<Result<Value>> {
            let (agent, command) = (self.agent.clone(), prompt(message));
            tokio::spawn(async move { agent.call(command).await })
        }
    }

    fn prompt(message: &str) -> AgentCommand {
        AgentCommand::Prompt { message: message.into() }
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(iso(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[tokio::test]
    async fn claude_keeps_one_process_per_chat_and_resumes_it() {
        let mut s = setup(AgentKind::Claude);
        s.call(prompt("Fix  the build")).await.unwrap();
        let mut p = s.process().await;
        assert_eq!(p.argv, claude::argv(None, None));
        let PipeInput::Data(line) = p.input().await else { panic!("expected the prompt") };
        assert_eq!(line, claude::user_line("Fix  the build"));
        assert_eq!(s.record().await["message"]["role"], "user");
        assert_eq!(s.record().await["type"], "agent_start");
        assert_eq!(s.call(prompt("again")).await.unwrap_err().to_string(), Error::AgentBusy.to_string());

        p.send(r#"{"type":"system","subtype":"init","session_id":"s-1"}"#).await;
        p.send(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}]}}"#).await;
        p.send(r#"{"type":"result","subtype":"success","is_error":false}"#).await;
        assert_eq!(s.until("message_end").await["message"]["content"][0]["text"], "Done.");
        s.until("agent_settled").await;

        // The same process gets the next message.
        s.call(prompt("Next")).await.unwrap();
        assert!(matches!(p.input().await, PipeInput::Data(l) if l == claude::user_line("Next")));
        p.send(r#"{"type":"result","subtype":"success","is_error":false}"#).await;
        s.until("agent_settled").await;
        let state = s.call(AgentCommand::GetState).await.unwrap();
        assert_eq!(state["isStreaming"], false);
        let chat = state["sessionFile"].as_str().unwrap().to_owned();
        let messages = s.call(AgentCommand::GetMessages).await.unwrap()["messages"].clone();
        assert_eq!(messages.as_array().unwrap().len(), 3);
        let stamps: Vec<i64> = messages.as_array().unwrap().iter().map(|m| m["timestamp"].as_i64().unwrap()).collect();
        assert!(stamps.windows(2).all(|w| w[0] < w[1]));

        // A model change ends the idle process; the next prompt resumes in
        // the cwd of the session, also after a harness change.
        s.call(AgentCommand::SetModel { provider: "claude".into(), model_id: "opus".into() }).await.unwrap();
        assert!(matches!(p.input().await, PipeInput::Close));
        p.exit(0).await;
        *s.harness.lock().unwrap() = "/workspace/other".into();
        let call = s.prompt_later("More");
        s.probe("/workspace", 0).await;
        call.await.unwrap().unwrap();
        let p2 = s.process().await;
        assert_eq!(p2.argv, claude::argv(Some("s-1"), Some("opus")));
        assert_eq!(p2.cwd.as_deref(), Some("/workspace"));

        let chats = list_chats(&s.dir, AgentKind::Claude);
        assert_eq!(chats.len(), 1);
        assert_eq!((chats[0].id.as_str(), chats[0].title.as_str()), (chat.as_str(), "Fix the build"));
        assert!(list_chats(&s.dir, AgentKind::Codex).is_empty());
    }

    #[tokio::test]
    async fn claude_abort_interrupts_then_kills() {
        let mut s = setup(AgentKind::Claude);
        s.call(prompt("Long work")).await.unwrap();
        let mut p = s.process().await;
        p.input().await;
        s.call(AgentCommand::Abort).await.unwrap();
        let PipeInput::Data(line) = p.input().await else { panic!("expected the interrupt") };
        assert_eq!(line, claude::interrupt_line(1));
        // No answer: the process is killed after ABORT_LIMIT.
        assert!(matches!(p.stdin.recv().await, Some(PipeInput::Kill)));
        p.out.send(PipeEvent::Exit(137)).await.unwrap();
        let stopped = s.until("message_end").await;
        assert_eq!(stopped["message"]["role"], "user");
        assert_eq!(s.until("message_end").await["message"]["stopReason"], "aborted");
        s.until("agent_settled").await;
        assert!(!s.finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn codex_starts_a_process_per_turn_and_abort_kills_it() {
        let mut s = setup(AgentKind::Codex);
        s.call(prompt("hi")).await.unwrap();
        let mut p = s.process().await;
        assert_eq!((p.argv.clone(), p.cwd.clone()), (codex::argv(None, None), None));
        assert!(matches!(p.input().await, PipeInput::Data(d) if d == b"hi"));
        assert!(matches!(p.input().await, PipeInput::Close));
        p.send(r#"{"type":"thread.started","thread_id":"t-1"}"#).await;
        p.send(r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Hello."}}"#).await;
        p.send(r#"{"type":"turn.completed","usage":{}}"#).await;
        p.out.send(PipeEvent::Exit(0)).await.unwrap();
        s.until("agent_settled").await;

        s.call(prompt("next")).await.unwrap();
        let mut p2 = s.process().await;
        assert_eq!(p2.argv, codex::argv(Some("t-1"), None));
        p2.input().await;
        p2.input().await;
        s.call(AgentCommand::Abort).await.unwrap();
        assert!(matches!(p2.input().await, PipeInput::Kill));
        p2.out.send(PipeEvent::Stderr(b"killed\n".to_vec())).await.unwrap();
        p2.out.send(PipeEvent::Exit(137)).await.unwrap();
        assert_eq!(s.until("message_end").await["message"]["role"], "user");
        assert_eq!(s.until("message_end").await["message"]["stopReason"], "aborted");
        s.until("agent_settled").await;
        assert!(matches!(s.call(AgentCommand::GetSessionStats).await, Err(Error::Unsupported(_))));
    }

    #[tokio::test]
    async fn a_crash_ends_the_turn_with_the_stderr() {
        let mut s = setup(AgentKind::Codex);
        s.call(prompt("hi")).await.unwrap();
        let p = s.process().await;
        p.out.send(PipeEvent::Stdout(b"WARNING: no PATH aliases\n".to_vec())).await.unwrap();
        p.out.send(PipeEvent::Exit(1)).await.unwrap();
        s.until("message_end").await;
        let error = s.until("message_end").await;
        assert_eq!(error["message"]["stopReason"], "error");
        assert_eq!(error["message"]["errorMessage"], "The process ended with code 1.\nWARNING: no PATH aliases");
        s.until("agent_settled").await;
    }

    #[tokio::test]
    async fn switch_session_loads_the_transcript() {
        let mut s = setup(AgentKind::Codex);
        s.call(prompt("first chat")).await.unwrap();
        let p = s.process().await;
        p.send(r#"{"type":"thread.started","thread_id":"t-1"}"#).await;
        p.send(r#"{"type":"turn.completed"}"#).await;
        p.exit(0).await;
        s.until("agent_settled").await;
        let first = s.call(AgentCommand::GetState).await.unwrap()["sessionFile"].clone();
        // A model change and a change back to the default model both stay with the chat.
        for model in ["gpt-5", ""] {
            s.call(AgentCommand::SetModel { provider: "codex".into(), model_id: model.into() }).await.unwrap();
        }

        s.call(AgentCommand::NewSession).await.unwrap();
        s.call(AgentCommand::SetModel { provider: "codex".into(), model_id: "gpt-5".into() }).await.unwrap();
        assert_eq!(s.call(AgentCommand::GetMessages).await.unwrap(), json!({ "messages": [] }));
        assert!(matches!(
            s.call(AgentCommand::SwitchSession { chat: "../x.jsonl".into() }).await,
            Err(Error::ChatNotFound)
        ));
        s.call(AgentCommand::SwitchSession { chat: first.as_str().unwrap().into() }).await.unwrap();
        let messages = s.call(AgentCommand::GetMessages).await.unwrap();
        assert_eq!(messages["messages"][0]["content"][0]["text"], "first chat");
        s.call(prompt("more")).await.unwrap();
        assert_eq!(s.process().await.argv, codex::argv(Some("t-1"), None));
    }

    #[tokio::test]
    async fn forget_resets_the_current_chat_only() {
        let mut s = setup(AgentKind::Codex);
        s.call(prompt("hi")).await.unwrap();
        s.process().await.send(r#"{"type":"turn.completed"}"#).await;
        s.until("agent_settled").await;
        let chat = s.call(AgentCommand::GetState).await.unwrap()["sessionFile"].as_str().unwrap().to_owned();

        s.agent.forget("other.jsonl").unwrap();
        s.agent.forget(&chat).unwrap();
        assert_eq!(s.record().await, json!({ "type": "chat_reset" }));
        assert_eq!(s.call(AgentCommand::GetState).await.unwrap()["sessionFile"], Value::Null);
    }

    /// A Claude chat with a session that is idle, and its process.
    async fn claude_session(s: &mut Setup) -> Process {
        s.call(prompt("first")).await.unwrap();
        let mut p = s.process().await;
        p.input().await;
        p.send(r#"{"type":"system","subtype":"init","session_id":"s-1"}"#).await;
        p.send(r#"{"type":"result","subtype":"success","is_error":false}"#).await;
        s.until("agent_settled").await;
        p
    }

    fn chat_meta(s: &Setup, chat: &str) -> Meta {
        read(&s.dir.join(chat), false).unwrap().0
    }

    #[tokio::test]
    async fn a_gone_cwd_starts_a_new_session_for_good() {
        let mut s = setup(AgentKind::Claude);
        let p = claude_session(&mut s).await;
        let chat = s.call(AgentCommand::GetState).await.unwrap()["sessionFile"].as_str().unwrap().to_owned();
        assert_eq!(chat_meta(&s, &chat).cwd.as_deref(), Some("/workspace"));
        s.call(AgentCommand::NewSession).await.unwrap();
        p.exit(0).await;
        s.call(AgentCommand::SwitchSession { chat: chat.clone() }).await.unwrap();

        *s.harness.lock().unwrap() = "/workspace/new".into();
        let call = s.prompt_later("again");
        s.probe("/workspace", 1).await;
        call.await.unwrap().unwrap();
        let notice = s.until("message_end").await;
        let notice = if notice["message"]["role"] == "user" { s.until("message_end").await } else { notice };
        assert_eq!(notice["message"]["role"], "notice");
        assert!(notice["message"]["content"][0]["text"].as_str().unwrap().contains("(/workspace) is gone"));
        let p2 = s.process().await;
        assert_eq!((p2.argv.clone(), p2.cwd.clone()), (claude::argv(None, None), None));
        // A reload does not bring the old session back; the new cwd is kept.
        let meta = chat_meta(&s, &chat);
        assert_eq!((meta.session_id, meta.cwd.as_deref()), (None, Some("/workspace/new")));
        let messages = Transcript::open(&s.dir, AgentKind::Claude, &chat).unwrap().messages;
        assert_eq!(messages.iter().filter(|m| m["role"] == "notice").count(), 1);
    }

    #[tokio::test]
    async fn an_old_chat_with_no_cwd_starts_a_new_session() {
        let mut s = setup(AgentKind::Claude);
        std::fs::create_dir_all(&s.dir).unwrap();
        let chat = format!("{}.jsonl", uuid::Uuid::new_v4());
        std::fs::write(
            s.dir.join(&chat),
            "{\"type\":\"meta\",\"agent\":\"claude\",\"started\":\"t\",\"sessionId\":\"s-old\"}\n",
        )
        .unwrap();
        s.call(AgentCommand::SwitchSession { chat: chat.clone() }).await.unwrap();
        s.call(prompt("hi")).await.unwrap();
        // No probe: the first process is the new session.
        let p = s.process().await;
        assert_eq!(p.argv, claude::argv(None, None));
        let messages = s.call(AgentCommand::GetMessages).await.unwrap()["messages"].clone();
        assert_eq!(messages[1]["role"], "notice");
        assert!(messages[1]["content"][0]["text"].as_str().unwrap().contains("no saved folder"));
        assert_eq!(chat_meta(&s, &chat).session_id, None);
    }

    #[tokio::test]
    async fn a_failed_probe_keeps_the_session() {
        let mut s = setup(AgentKind::Claude);
        let p = claude_session(&mut s).await;
        s.call(AgentCommand::NewSession).await.unwrap();
        p.exit(0).await;
        let chat = list_chats(&s.dir, AgentKind::Claude)[0].id.clone();
        s.call(AgentCommand::SwitchSession { chat }).await.unwrap();
        let call = s.prompt_later("again");
        s.probe("/workspace", -1).await;
        call.await.unwrap().unwrap();
        assert_eq!(s.process().await.argv, claude::argv(Some("s-1"), None));
    }

    #[tokio::test]
    async fn a_closing_process_is_waited_for_and_killed_after_the_limit() {
        let mut s = setup(AgentKind::Claude);
        let mut p = claude_session(&mut s).await;
        s.call(AgentCommand::NewSession).await.unwrap();
        assert!(matches!(p.input().await, PipeInput::Close));
        // The next process waits for the old one.
        let call = s.prompt_later("next");
        assert!(tokio::time::timeout(Duration::from_millis(300), s.processes.recv()).await.is_err());
        assert!(matches!(p.stdin.recv().await, Some(PipeInput::Kill)));
        p.exit(137).await;
        call.await.unwrap().unwrap();
        let mut p2 = s.process().await;
        assert_eq!(p2.argv, claude::argv(None, None));
        p2.input().await;
        p2.send(r#"{"type":"result","subtype":"success","is_error":false}"#).await;
        s.until("agent_settled").await;

        // The finish waits for a closing process too.
        s.call(AgentCommand::NewSession).await.unwrap();
        assert!(matches!(p2.input().await, PipeInput::Close));
        s.agent.close();
        assert!(tokio::time::timeout(Duration::from_millis(300), s.agent.exited()).await.is_err());
        p2.exit(0).await;
        tokio::time::timeout(Duration::from_secs(2), s.agent.exited()).await.unwrap();
    }

    #[tokio::test]
    async fn kill_ends_the_closing_processes() {
        let mut s = setup(AgentKind::Claude);
        let mut p = claude_session(&mut s).await;
        s.call(AgentCommand::NewSession).await.unwrap();
        assert!(matches!(p.input().await, PipeInput::Close));
        s.agent.kill();
        assert!(matches!(p.input().await, PipeInput::Kill));
        p.exit(137).await;
        tokio::time::timeout(Duration::from_secs(2), s.agent.exited()).await.unwrap();
    }

    #[tokio::test]
    async fn close_ends_the_process_and_finishes() {
        let mut s = setup(AgentKind::Claude);
        s.call(prompt("hi")).await.unwrap();
        let mut p = s.process().await;
        p.input().await;
        s.agent.close();
        assert!(matches!(p.input().await, PipeInput::Close));
        assert!(matches!(s.call(prompt("x")).await, Err(Error::AgentBusy | Error::AgentClosed)));
        p.out.send(PipeEvent::Exit(0)).await.unwrap();
        s.agent.exited().await;
        assert!(s.finished.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(s.until("agent_exit").await["code"], 0);
        assert!(matches!(s.call(prompt("x")).await, Err(Error::AgentClosed)));

        // With no process, the agent finishes at once.
        let s = setup(AgentKind::Codex);
        s.agent.kill();
        tokio::time::timeout(Duration::from_secs(2), s.agent.exited()).await.unwrap();
    }

    #[test]
    fn chat_files_are_listed_renamed_and_deleted_by_id() {
        let dir = temp_dir();
        let chat = Transcript::create(&dir, AgentKind::Codex, None).unwrap();
        std::fs::write(dir.join("notes.jsonl"), "{\"type\":\"meta\",\"agent\":\"codex\",\"started\":\"t\"}\n").unwrap();
        assert_eq!(list_chats(&dir, AgentKind::Codex)[0].title, "New chat");
        assert_eq!(list_chats(&dir, AgentKind::Codex).len(), 1);
        rename_chat(&dir, AgentKind::Codex, &chat.id, "  Auth  ").unwrap();
        assert_eq!(list_chats(&dir, AgentKind::Codex)[0].title, "Auth");
        for bad in ["notes.jsonl", "../x.jsonl", "", "missing"] {
            assert!(matches!(delete_chat(&dir, bad), Err(Error::ChatNotFound)), "{bad}");
        }
        assert!(matches!(rename_chat(&dir, AgentKind::Claude, &chat.id, "x"), Err(Error::ChatNotFound)));
        delete_chat(&dir, &chat.id).unwrap();
        assert!(list_chats(&dir, AgentKind::Codex).is_empty());
        assert!(dir.join("notes.jsonl").exists());
    }
}
