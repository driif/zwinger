//! The pi agent of a shield: `pi --mode rpc` in the VM, driven over JSONL pipes.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use serde_json::{Value, json};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{oneshot, watch};

use super::{AgentCommand, AgentKind, Chat, Framer, Line, STATE_DIR, Sink, TITLE_CHARS, cut, keep, lock, send_now};
use crate::error::{Error, Result};
use crate::runtime::{PipeEvent, PipeInput};

/// The pi bundle, mounted read-only.
pub const PI_DIR: &str = "/opt/zwinger/pi";
/// The host install of pi, mounted read-only when the shield uses it.
pub const HOST_DIR: &str = "/opt/zwinger/host/pi";
/// The entry point in the host package. Detection accepts only this layout.
pub const HOST_ENTRY: &str = "dist/cli.js";
pub const AGENT_DIR: &str = "/opt/zwinger/state/agent";
const SESSIONS: &str = "sessions";
/// pi 1.3 needs at least this Node version.
const NODE_MIN: (u32, u32) = (22, 19);
/// Chat files are listed only up to this size.
const CHAT_FILE: u64 = 64 << 20;
/// Only this start of a chat file is read for its title.
const CHAT_HEAD: u64 = 1 << 20;
/// And only this end, for a name that was set late.
const CHAT_TAIL: u64 = 1 << 20;
/// A longer line of a chat file is skipped.
const CHAT_LINE: usize = 64 << 10;

/// The script of the host pi (`host` true) or of the bundled pi.
pub fn cli(host: bool) -> String {
    if host { format!("{HOST_DIR}/{HOST_ENTRY}") } else { format!("{PI_DIR}/cli.js") }
}

/// The argv of the terminal pi (`rpc` false) or the agent pi (`rpc` true).
pub fn pi_argv(rpc: bool, host: bool) -> Vec<String> {
    let mut argv = vec!["node".to_owned(), cli(host)];
    argv.extend(["--session-dir", "/opt/zwinger/state/sessions"].map(str::to_owned));
    if rpc {
        argv.extend(["--mode", "rpc"].map(str::to_owned));
    }
    argv
}

impl AgentCommand {
    /// The JSON for pi. `state` is the host state dir of the shield, where a
    /// `switch_session` chat id must name a listed chat.
    /// It can read the chat files, so it runs on a blocking thread.
    pub fn wire(&self, state: &Path) -> Result<Value> {
        let mut wire = serde_json::to_value(self)?;
        if let Self::SwitchSession { chat } = self {
            if !list_chats(state).iter().any(|c| c.id == *chat) {
                return Err(Error::ChatNotFound);
            }
            wire["sessionPath"] = format!("{STATE_DIR}/{SESSIONS}/{chat}").into();
        }
        Ok(wire)
    }
}

struct Pending {
    /// Set when pi ended. A call after that fails at once.
    closed: bool,
    calls: HashMap<String, oneshot::Sender<Result<Value>>>,
}

/// One running pi process. Responses go to the waiting call; the other
/// records go to the sink.
pub struct Agent {
    input: Sender<PipeInput>,
    pending: Mutex<Pending>,
    sink: Mutex<Sink>,
    next: AtomicU64,
    /// Set by [`Agent::finish`].
    done: watch::Sender<bool>,
    /// Between pi's `agent_start` and `agent_settled`.
    busy: AtomicBool,
}

/// What [`Agent::run`] saw when pi ended.
pub struct Exit {
    code: i32,
    stderr: VecDeque<String>,
}

impl Agent {
    pub fn new(input: Sender<PipeInput>, sink: Sink) -> Self {
        Self {
            input,
            pending: Mutex::new(Pending {
                closed: false,
                calls: HashMap::new(),
            }),
            sink: Mutex::new(sink),
            next: AtomicU64::new(1),
            done: watch::Sender::new(false),
            busy: AtomicBool::new(false),
        }
    }

    /// A turn runs.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// A reopened Agents tab gets the records from now on.
    pub fn set_sink(&self, sink: Sink) {
        *lock(&self.sink) = sink;
    }

    fn emit(&self, record: Value) {
        (lock(&self.sink))(record);
    }

    fn line(record: &Value) -> Result<PipeInput> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        Ok(PipeInput::Data(line))
    }

    fn send_now(&self, input: PipeInput) {
        send_now(&self.input, input);
    }

    /// Sends the command and waits for its response. Returns the response
    /// `data`, or `null` if it has none.
    pub async fn call(&self, mut command: Value) -> Result<Value> {
        let id = format!("zw-{}", self.next.fetch_add(1, Ordering::Relaxed));
        command["id"] = id.clone().into();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = lock(&self.pending);
            if pending.closed {
                return Err(Error::AgentClosed);
            }
            // Before the write, so a fast response finds its call.
            pending.calls.insert(id.clone(), tx);
        }
        let sent = match Self::line(&command) {
            Ok(line) => self.input.send(line).await.map_err(|_| Error::AgentClosed),
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            lock(&self.pending).calls.remove(&id);
            return Err(e);
        }
        rx.await.unwrap_or(Err(Error::AgentClosed))
    }

    /// Asks pi to exit: pi ends when its stdin closes.
    pub fn close(&self) {
        self.send_now(PipeInput::Close);
    }

    pub fn kill(&self) {
        self.send_now(PipeInput::Kill);
    }

    /// Waits until [`Agent::finish`] ran.
    pub async fn exited(&self) {
        let _ = self.done.subscribe().wait_for(|done| *done).await;
    }

    /// Reads pi's output until the process ends.
    /// A record over 16 MB kills pi: the protocol has no way to skip it.
    pub async fn run(&self, mut events: Receiver<PipeEvent>) -> Exit {
        let mut stdout = Framer::default();
        let mut stderr_framer = Framer::default();
        let mut stderr = VecDeque::new();
        let mut code = -1;
        let mut killed = false;
        while let Some(event) = events.recv().await {
            match event {
                PipeEvent::Stdout(bytes) => {
                    for line in stdout.push(&bytes) {
                        match line {
                            Line::Record(line) => match serde_json::from_slice::<Value>(&line) {
                                Ok(record) => self.route(record),
                                Err(_) => keep(&mut stderr, format!("unreadable output: {}", cut(&line))),
                            },
                            Line::TooLong if !killed => {
                                killed = true;
                                self.kill();
                                keep(&mut stderr, "output line over 16 MB".to_owned());
                            }
                            Line::TooLong => {}
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
        Exit { code, stderr }
    }

    fn route(&self, record: Value) {
        match record["type"].as_str() {
            Some("response") => {
                let call = record["id"]
                    .as_str()
                    .and_then(|id| lock(&self.pending).calls.remove(id));
                let Some(call) = call else {
                    return self.emit(record);
                };
                let result = if record["success"].as_bool() == Some(true) {
                    Ok(record.get("data").cloned().unwrap_or(Value::Null))
                } else {
                    let message = record["error"].as_str().unwrap_or("pi refused the command");
                    Err(Error::Agent(message.to_owned()))
                };
                let _ = call.send(result);
            }
            // A dialog blocks pi until it gets an answer. The UI has no
            // dialogs yet, so each one is cancelled.
            Some("extension_ui_request")
                if matches!(record["method"].as_str(), Some("select" | "confirm" | "input" | "editor")) =>
            {
                let answer = json!({ "type": "extension_ui_response", "id": record["id"], "cancelled": true });
                if let Ok(line) = Self::line(&answer) {
                    self.send_now(line);
                }
            }
            _ => {
                match record["type"].as_str() {
                    Some("agent_start") => self.busy.store(true, Ordering::Relaxed),
                    Some("agent_settled") => self.busy.store(false, Ordering::Relaxed),
                    _ => {}
                }
                self.emit(record)
            }
        }
    }

    /// Fails the waiting calls and sends `agent_exit` with the last stderr
    /// lines. Called after the agent is removed from the shield, so a reopen
    /// in reaction to the event starts a new pi.
    pub fn finish(&self, exit: Exit) {
        let calls = {
            let mut pending = lock(&self.pending);
            pending.closed = true;
            std::mem::take(&mut pending.calls)
        };
        for call in calls.into_values() {
            let _ = call.send(Err(Error::AgentClosed));
        }
        self.emit(json!({ "type": "agent_exit", "code": exit.code, "stderr": exit.stderr }));
        self.done.send_replace(true);
    }
}

/// Accepts `v22.19.0` and newer.
pub fn check_node(version: &str) -> Result<()> {
    let version = version.trim();
    let mut parts = version.trim_start_matches('v').split('.').map(str::parse::<u32>);
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) if (major, minor) >= NODE_MIN => Ok(()),
        _ => Err(Error::NodeTooOld(version.to_owned())),
    }
}

/// Lists the chats, newest first. The guest can write the state dir, so only
/// regular `*.jsonl` files directly in a real `sessions` dir count. The dir is
/// opened once without following links and each file is opened relative to
/// it, so a link that the guest swaps in later is never followed.
pub fn list_chats(state: &Path) -> Vec<Chat> {
    let nofollow = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let Ok(dir) = rustix::fs::open(state.join(SESSIONS), nofollow | OFlags::DIRECTORY, Mode::empty()) else {
        return Vec::new();
    };
    let Ok(entries) = Dir::read_from(&dir) else {
        return Vec::new();
    };
    let mut chats: Vec<Chat> = entries
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_str().ok()?.to_owned();
            if !id.ends_with(".jsonl") || id.starts_with('.') {
                return None;
            }
            // NONBLOCK, so a FIFO does not block the open.
            let file = rustix::fs::openat(&dir, entry.file_name(), nofollow | OFlags::NONBLOCK, Mode::empty()).ok()?;
            read_chat(std::fs::File::from(file)).map(|(started, title)| Chat {
                agent: AgentKind::Pi,
                id,
                started,
                title,
            })
        })
        .collect();
    chats.sort_by(|a, b| b.started.cmp(&a.started));
    chats
}

/// Deletes a chat file. Only a listed chat id counts, and the file is
/// removed relative to the `sessions` dir, opened without following links,
/// only while it is a regular file.
pub fn delete_chat(state: &Path, chat: &str) -> Result<()> {
    if !list_chats(state).iter().any(|c| c.id == chat) {
        return Err(Error::ChatNotFound);
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let dir = rustix::fs::open(state.join(SESSIONS), flags, Mode::empty()).map_err(|_| Error::ChatNotFound)?;
    let stat = rustix::fs::statat(&dir, chat, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| Error::ChatNotFound)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(Error::ChatNotFound);
    }
    rustix::fs::unlinkat(&dir, chat, AtFlags::empty()).map_err(|e| Error::Io(e.into()))
}

/// Reads the header timestamp and the title of an open chat file. Reads at
/// most `CHAT_HEAD` bytes from the start and `CHAT_TAIL` bytes from the end,
/// so a name set in the middle of a long chat is not seen.
fn read_chat(file: std::fs::File) -> Option<(String, String)> {
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > CHAT_FILE {
        return None;
    }
    let mut reader = BufReader::new(file.take(CHAT_HEAD));
    let header: Value = serde_json::from_slice(&chat_line(&mut reader)??).ok()?;
    if header["type"] != "session" {
        return None;
    }
    let started = header["timestamp"].as_str()?.to_owned();
    let (mut name, mut first) = (None, None);
    title_lines(&mut reader, &mut name, &mut first);
    if meta.len() > CHAT_HEAD {
        let mut file = reader.into_inner().into_inner();
        let start = CHAT_HEAD.max(meta.len() - CHAT_TAIL);
        // From one byte before, so a line that starts at `start` is not cut off.
        file.seek(SeekFrom::Start(start - 1)).ok()?;
        let mut reader = BufReader::new(file.take(CHAT_TAIL + 1));
        chat_line(&mut reader);
        // Names only: the first user message is in the head.
        title_lines(&mut reader, &mut name, &mut Some(String::new()));
    }
    let title = name.or(first).unwrap_or_else(|| "New chat".to_owned());
    Some((started, title.chars().take(TITLE_CHARS).collect()))
}

/// Reads the session name (the last one wins) and, while `first` is `None`,
/// the first user message.
fn title_lines(reader: &mut impl BufRead, name: &mut Option<String>, first: &mut Option<String>) {
    while let Some(line) = chat_line(reader) {
        let Some(line) = line else {
            continue;
        };
        // Only the lines that can hold a title are parsed.
        let has = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
        if !has(b"\"session_info\"") && (first.is_some() || !has(b"\"user\"")) {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match entry["type"].as_str() {
            Some("session_info") => *name = entry["name"].as_str().map(str::to_owned),
            Some("message") if first.is_none() && entry["message"]["role"] == "user" => {
                *first = user_text(&entry["message"]["content"]);
            }
            _ => {}
        }
    }
}

/// The next line without its LF, or `Some(None)` for a line over
/// `CHAT_LINE` bytes, which is skipped. `None` at the end or on an error.
fn chat_line(reader: &mut impl BufRead) -> Option<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = reader.by_ref().take(CHAT_LINE as u64 + 1).read_until(b'\n', &mut line).ok()?;
    if read == 0 {
        return None;
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    } else if line.len() > CHAT_LINE {
        loop {
            let buf = reader.fill_buf().ok()?;
            if buf.is_empty() {
                break;
            }
            let (used, end) = buf.iter().position(|&b| b == b'\n').map_or((buf.len(), false), |i| (i + 1, true));
            reader.consume(used);
            if end {
                break;
            }
        }
        return Some(None);
    }
    Some(Some(line))
}

fn user_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// Copies the bundled pi once per version to `<runtime>/pi-<version>` and
/// returns that dir. The copy is in the app data dir, so no user mount can
/// reach it, and a partial copy is never used: it is renamed into place.
pub fn install_pi(bundle: &Path, runtime: &Path) -> Result<PathBuf> {
    let version = std::fs::read_to_string(bundle.join("VERSION"))?;
    let version = version.trim();
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return Err(Error::Io(std::io::Error::other(format!("invalid pi VERSION: {version}"))));
    }
    let dest = runtime.join(format!("pi-{version}"));
    if dest.is_dir() {
        return Ok(dest);
    }
    std::fs::create_dir_all(runtime)?;
    let tmp = runtime.join(format!(".pi-{version}-{}", uuid::Uuid::new_v4()));
    let copied = copy_dir(bundle, &tmp).and_then(|()| std::fs::rename(&tmp, &dest));
    if copied.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        // Another app instance may have won the rename.
        if !dest.is_dir() {
            copied?;
        }
    }
    Ok(dest)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::agent::STDERR_LINE;
    use crate::runtime::PIPE_QUEUE;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn command(json: Value) -> Result<Value> {
        serde_json::from_value::<AgentCommand>(json)?.wire(Path::new("/nonexistent"))
    }

    #[test]
    fn commands_are_built_from_typed_fields() {
        assert_eq!(
            command(json!({ "type": "prompt", "message": "hi", "images": [1], "id": "x" })).unwrap(),
            json!({ "type": "prompt", "message": "hi" })
        );
        assert_eq!(
            command(json!({ "type": "set_model", "provider": "anthropic", "modelId": "m" })).unwrap(),
            json!({ "type": "set_model", "provider": "anthropic", "modelId": "m" })
        );
        assert_eq!(
            command(json!({ "type": "follow_up", "message": "then" })).unwrap(),
            json!({ "type": "follow_up", "message": "then" })
        );
        assert_eq!(
            command(json!({ "type": "set_thinking_level", "level": "xhigh" })).unwrap(),
            json!({ "type": "set_thinking_level", "level": "xhigh" })
        );
        assert_eq!(command(json!({ "type": "abort" })).unwrap(), json!({ "type": "abort" }));
        assert_eq!(
            command(json!({ "type": "set_session_name", "name": "Auth" })).unwrap(),
            json!({ "type": "set_session_name", "name": "Auth" })
        );
        for refused in [
            json!({ "type": "bash", "command": "rm -rf /" }),
            json!({ "type": "export_html", "outputPath": "/tmp/x" }),
            json!({ "type": "set_thinking_level", "level": "ultra" }),
            json!({ "type": "switch_session", "sessionPath": "/etc/passwd" }),
        ] {
            assert!(command(refused.clone()).is_err(), "{refused}");
        }
    }

    #[test]
    fn switch_session_maps_a_listed_chat_to_its_guest_path() {
        let state = temp_dir();
        std::fs::create_dir(state.join(SESSIONS)).unwrap();
        std::fs::write(
            state.join(SESSIONS).join("a.jsonl"),
            "{\"type\":\"session\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n",
        )
        .unwrap();
        let switch = |chat: &str| AgentCommand::SwitchSession { chat: chat.into() }.wire(&state);
        assert_eq!(
            switch("a.jsonl").unwrap(),
            json!({ "type": "switch_session", "sessionPath": "/opt/zwinger/state/sessions/a.jsonl" })
        );
        for chat in ["../a.jsonl", "b.jsonl", "/etc/passwd"] {
            assert!(matches!(switch(chat), Err(Error::ChatNotFound)), "{chat}");
        }
    }

    #[test]
    fn list_chats_reads_titles_and_skips_links_and_other_files() {
        let state = temp_dir();
        let sessions = state.join(SESSIONS);
        std::fs::create_dir(&sessions).unwrap();
        let header = |at: &str| format!("{{\"type\":\"session\",\"version\":3,\"id\":\"u\",\"timestamp\":\"{at}\",\"cwd\":\"/w\"}}\n");
        let user = "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Fix  the\\nbuild\"}]}}\n";
        std::fs::write(sessions.join("old.jsonl"), header("2026-01-01T00:00:00Z") + user).unwrap();
        let named = header("2026-02-01T00:00:00Z")
            + user
            + "{\"type\":\"session_info\",\"name\":\"Auth refactor\"}\n";
        std::fs::write(sessions.join("new.jsonl"), named).unwrap();
        std::fs::write(sessions.join("empty.jsonl"), header("2026-01-15T00:00:00Z")).unwrap();
        std::fs::write(sessions.join("notes.txt"), header("2026-03-01T00:00:00Z")).unwrap();
        std::fs::write(sessions.join("bad.jsonl"), "not json\n").unwrap();
        let outside = temp_dir().join("secret.jsonl");
        std::fs::write(&outside, header("2026-04-01T00:00:00Z")).unwrap();
        std::os::unix::fs::symlink(&outside, sessions.join("link.jsonl")).unwrap();
        std::fs::create_dir(sessions.join("dir.jsonl")).unwrap();
        let fifo = std::process::Command::new("mkfifo").arg(sessions.join("fifo.jsonl")).status();
        assert!(fifo.unwrap().success());

        let chats = list_chats(&state);
        let got: Vec<(&str, &str)> = chats.iter().map(|c| (c.id.as_str(), c.title.as_str())).collect();
        assert_eq!(
            got,
            [("new.jsonl", "Auth refactor"), ("empty.jsonl", "New chat"), ("old.jsonl", "Fix the build")]
        );
    }

    #[test]
    fn list_chats_refuses_a_linked_sessions_dir() {
        let state = temp_dir();
        let real = temp_dir();
        std::fs::write(real.join("a.jsonl"), "{\"type\":\"session\",\"timestamp\":\"t\"}\n").unwrap();
        std::os::unix::fs::symlink(&real, state.join(SESSIONS)).unwrap();
        assert!(list_chats(&state).is_empty());
        assert!(list_chats(&temp_dir()).is_empty());
    }

    #[test]
    fn delete_chat_removes_only_a_listed_regular_file() {
        let state = temp_dir();
        let sessions = state.join(SESSIONS);
        std::fs::create_dir(&sessions).unwrap();
        let header = "{\"type\":\"session\",\"timestamp\":\"t\"}\n";
        std::fs::write(sessions.join("a.jsonl"), header).unwrap();
        let outside = temp_dir().join("secret.jsonl");
        std::fs::write(&outside, header).unwrap();
        std::os::unix::fs::symlink(&outside, sessions.join("link.jsonl")).unwrap();
        std::fs::create_dir(sessions.join("dir.jsonl")).unwrap();
        std::fs::write(state.join("b.jsonl"), header).unwrap();

        for chat in ["link.jsonl", "dir.jsonl", "../b.jsonl", "/etc/passwd", "", "missing.jsonl"] {
            assert!(matches!(delete_chat(&state, chat), Err(Error::ChatNotFound)), "{chat}");
        }
        assert!(outside.exists() && sessions.join("dir.jsonl").is_dir() && state.join("b.jsonl").exists());
        delete_chat(&state, "a.jsonl").unwrap();
        assert!(!sessions.join("a.jsonl").exists());
        assert!(matches!(delete_chat(&state, "a.jsonl"), Err(Error::ChatNotFound)));

        let linked = temp_dir();
        let real = temp_dir();
        std::fs::write(real.join("c.jsonl"), header).unwrap();
        std::os::unix::fs::symlink(&real, linked.join(SESSIONS)).unwrap();
        assert!(matches!(delete_chat(&linked, "c.jsonl"), Err(Error::ChatNotFound)));
        assert!(real.join("c.jsonl").exists());
    }

    #[test]
    fn node_version_check() {
        for ok in ["v22.19.0\n", "v22.20.1", "v24.0.0"] {
            assert!(check_node(ok).is_ok(), "{ok}");
        }
        for old in ["v22.18.9", "v20.11.0", "", "bash: node: not found"] {
            assert!(matches!(check_node(old), Err(Error::NodeTooOld(_))), "{old}");
        }
    }

    #[test]
    fn install_pi_copies_once_per_version() {
        let bundle = temp_dir();
        std::fs::write(bundle.join("VERSION"), "1.3.0\n").unwrap();
        std::fs::create_dir(bundle.join("chunks")).unwrap();
        std::fs::write(bundle.join("chunks").join("a.js"), "a").unwrap();
        let runtime = temp_dir().join("runtime");

        let dest = install_pi(&bundle, &runtime).unwrap();
        assert_eq!(dest, runtime.join("pi-1.3.0"));
        assert_eq!(std::fs::read_to_string(dest.join("chunks").join("a.js")).unwrap(), "a");

        std::fs::write(bundle.join("chunks").join("a.js"), "changed").unwrap();
        assert_eq!(install_pi(&bundle, &runtime).unwrap(), dest);
        assert_eq!(std::fs::read_to_string(dest.join("chunks").join("a.js")).unwrap(), "a");
        let names: Vec<_> = std::fs::read_dir(&runtime).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["pi-1.3.0"]);

        std::fs::write(bundle.join("VERSION"), "../x").unwrap();
        assert!(install_pi(&bundle, &runtime).is_err());
    }

    /// Plays pi on the other end of the pipe.
    struct Pi {
        stdin: mpsc::Receiver<PipeInput>,
        out: mpsc::Sender<PipeEvent>,
    }

    impl Pi {
        async fn read(&mut self) -> Value {
            match tokio::time::timeout(std::time::Duration::from_secs(2), self.stdin.recv()).await {
                Ok(Some(PipeInput::Data(line))) => {
                    assert_eq!(line.last(), Some(&b'\n'));
                    serde_json::from_slice(&line).unwrap()
                }
                other => panic!("expected a line, got {other:?}"),
            }
        }

        async fn send(&self, chunk: &str) {
            self.out.send(PipeEvent::Stdout(chunk.as_bytes().to_vec())).await.unwrap();
        }
    }

    fn agent() -> (std::sync::Arc<Agent>, Pi, mpsc::Receiver<PipeEvent>, mpsc::UnboundedReceiver<Value>) {
        let (input, stdin) = mpsc::channel(PIPE_QUEUE);
        let (out, events) = mpsc::channel(PIPE_QUEUE);
        let (records_tx, records) = mpsc::unbounded_channel();
        let agent = Agent::new(input, Box::new(move |r| {
            let _ = records_tx.send(r);
        }));
        (std::sync::Arc::new(agent), Pi { stdin, out }, events, records)
    }

    #[tokio::test]
    async fn responses_go_to_their_call_and_records_to_the_sink() {
        let (agent, mut pi, events, mut records) = agent();
        let runner = tokio::spawn({
            let agent = agent.clone();
            async move { agent.run(events).await }
        });
        let call = tokio::spawn({
            let agent = agent.clone();
            async move { agent.call(json!({ "type": "get_state" })).await }
        });
        let sent = pi.read().await;
        assert_eq!(sent["type"], "get_state");
        let id = sent["id"].as_str().unwrap().to_owned();

        let response = format!("{{\"id\":\"{id}\",\"type\":\"response\",\"command\":\"get_state\",\"success\":true,\"data\":{{\"n\":1}}}}\r\n");
        let (a, b) = response.split_at(10);
        pi.send("{\"type\":\"agent_start\"}\nnot json\n").await;
        pi.send(a).await;
        pi.send(b).await;
        pi.send("{\"type\":\"future_event\",\"x\":1}\n").await;
        pi.send("{\"type\":\"extension_ui_request\",\"id\":\"d1\",\"method\":\"confirm\",\"title\":\"?\"}\n").await;
        pi.send("{\"type\":\"extension_ui_request\",\"id\":\"n1\",\"method\":\"notify\",\"message\":\"hi\"}\n").await;

        assert_eq!(call.await.unwrap().unwrap(), json!({ "n": 1 }));
        assert_eq!(records.recv().await.unwrap(), json!({ "type": "agent_start" }));
        assert_eq!(records.recv().await.unwrap(), json!({ "type": "future_event", "x": 1 }));
        assert_eq!(
            pi.read().await,
            json!({ "type": "extension_ui_response", "id": "d1", "cancelled": true })
        );
        assert_eq!(records.recv().await.unwrap()["method"], "notify");

        pi.out.send(PipeEvent::Stderr(b"boom\npartial".to_vec())).await.unwrap();
        pi.out.send(PipeEvent::Exit(1)).await.unwrap();
        let exit = runner.await.unwrap();
        assert_eq!(exit.code, 1);
        assert_eq!(exit.stderr, ["unreadable output: not json", "boom"]);
    }

    #[tokio::test]
    async fn failed_command_is_an_error() {
        let (agent, mut pi, events, _records) = agent();
        tokio::spawn({
            let agent = agent.clone();
            async move { agent.run(events).await }
        });
        let call = tokio::spawn({
            let agent = agent.clone();
            async move { agent.call(json!({ "type": "set_model" })).await }
        });
        let id = pi.read().await["id"].as_str().unwrap().to_owned();
        pi.send(&format!("{{\"id\":\"{id}\",\"type\":\"response\",\"success\":false,\"error\":\"Model not found\"}}\n")).await;
        assert!(matches!(call.await.unwrap(), Err(Error::Agent(m)) if m == "Model not found"));
    }

    #[tokio::test]
    async fn exit_fails_the_waiting_calls_and_later_calls() {
        let (agent, mut pi, events, mut records) = agent();
        let call = tokio::spawn({
            let agent = agent.clone();
            async move { agent.call(json!({ "type": "abort" })).await }
        });
        pi.read().await;
        pi.out.send(PipeEvent::Stderr(b"fatal\n".to_vec())).await.unwrap();
        pi.out.send(PipeEvent::Exit(137)).await.unwrap();
        let exit = agent.run(events).await;
        assert!(!call.is_finished());
        agent.finish(exit);

        assert!(matches!(call.await.unwrap(), Err(Error::AgentClosed)));
        assert_eq!(
            records.recv().await.unwrap(),
            json!({ "type": "agent_exit", "code": 137, "stderr": ["fatal"] })
        );
        assert!(matches!(agent.call(json!({ "type": "get_state" })).await, Err(Error::AgentClosed)));
    }

    #[tokio::test]
    async fn kill_and_close_do_not_wait_on_a_full_queue() {
        let (agent, mut pi, _events, _records) = agent();
        // No one reads the events, so the process side is held.
        for _ in 0..PIPE_QUEUE {
            pi.out.try_send(PipeEvent::Stdout(b"{}\n".to_vec())).unwrap();
        }
        assert!(pi.out.try_send(PipeEvent::Exit(0)).is_err());
        agent.close();
        agent.kill();
        assert!(matches!(pi.stdin.recv().await, Some(PipeInput::Close)));
        assert!(matches!(pi.stdin.recv().await, Some(PipeInput::Kill)));
    }

    #[tokio::test]
    async fn kill_waits_for_room_in_a_full_input_queue() {
        let (agent, mut pi, _events, _records) = agent();
        for _ in 0..PIPE_QUEUE {
            agent.input.try_send(PipeInput::Data(Vec::new())).unwrap();
        }
        agent.kill();
        for _ in 0..PIPE_QUEUE {
            assert!(matches!(pi.stdin.recv().await, Some(PipeInput::Data(_))));
        }
        assert!(matches!(pi.stdin.recv().await, Some(PipeInput::Kill)));
    }

    #[tokio::test]
    async fn a_record_over_the_limit_kills_pi() {
        let (agent, mut pi, events, _records) = agent();
        let runner = tokio::spawn({
            let agent = agent.clone();
            async move { agent.run(events).await }
        });
        let call = tokio::spawn({
            let agent = agent.clone();
            async move { agent.call(json!({ "type": "get_state" })).await }
        });
        pi.read().await;
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..17 {
            pi.out.send(PipeEvent::Stdout(chunk.clone())).await.unwrap();
        }
        assert!(matches!(pi.stdin.recv().await, Some(PipeInput::Kill)));
        pi.out.send(PipeEvent::Stderr(vec![b'e'; 3000])).await.unwrap();
        pi.out.send(PipeEvent::Stderr(b"\n".to_vec())).await.unwrap();
        pi.out.send(PipeEvent::Exit(137)).await.unwrap();
        let exit = runner.await.unwrap();
        assert_eq!(exit.stderr, ["output line over 16 MB".to_owned(), "e".repeat(STDERR_LINE)]);
        agent.finish(exit);
        assert!(matches!(call.await.unwrap(), Err(Error::AgentClosed)));
    }

    #[test]
    fn read_chat_skips_long_lines_and_reads_the_head_and_the_tail() {
        let state = temp_dir();
        let sessions = state.join(SESSIONS);
        std::fs::create_dir(&sessions).unwrap();
        let header = "{\"type\":\"session\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n";
        let user = |text: &str| {
            format!("{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n")
        };
        let long = user(&"a".repeat(CHAT_LINE));
        let filler = format!("{{\"type\":\"x\",\"pad\":\"{}\"}}\n", "p".repeat(1000));
        let mut late = format!("{header}{long}{}", user("Second"));
        while (late.len() as u64) < CHAT_HEAD {
            late.push_str(&filler);
        }
        let mut middle = late.clone();
        late.push_str("{\"type\":\"session_info\",\"name\":\"Late name\"}\n");
        std::fs::write(sessions.join("late.jsonl"), late).unwrap();
        // A name between the head and the tail is not seen.
        middle.push_str("{\"type\":\"session_info\",\"name\":\"Lost\"}\n");
        while (middle.len() as u64) < 2 * CHAT_HEAD + CHAT_TAIL {
            middle.push_str(&filler);
        }
        std::fs::write(sessions.join("middle.jsonl"), middle).unwrap();
        std::fs::write(sessions.join("one-line.jsonl"), "x".repeat(2 << 20)).unwrap();
        let big = std::fs::File::create(sessions.join("big.jsonl")).unwrap();
        std::io::Write::write_all(&mut &big, header.as_bytes()).unwrap();
        big.set_len(CHAT_FILE + 1).unwrap();

        let chats = list_chats(&state);
        let mut got: Vec<(&str, &str)> = chats.iter().map(|c| (c.id.as_str(), c.title.as_str())).collect();
        got.sort();
        assert_eq!(got, [("late.jsonl", "Late name"), ("middle.jsonl", "Second")]);
    }
}
