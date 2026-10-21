//! The agents of a shield: pi, Claude Code, Codex and Gemini CLI. Each one runs in the VM
//! and talks JSONL over pipes. The Agents tab gets pi-shaped records from each.

pub mod claude;
pub mod cli;
pub mod codex;
pub mod gemini;
pub mod pi;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;

use crate::runtime::PipeInput;

/// The guest prefix that belongs to zwinger. User mounts may not use it.
pub const RESERVED: &str = "/opt/zwinger";
/// The per-shield agent state (`<app_data>/shields/<id>`), mounted read-write.
pub const STATE_DIR: &str = "/opt/zwinger/state";
const STDERR_LINES: usize = 20;
/// The longest record that an agent may write, and the longest stderr line.
const MAX_LINE: usize = 16 << 20;
/// A kept stderr line, or the copy of unreadable output, is cut to this size.
const STDERR_LINE: usize = 2 << 10;
const TITLE_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Pi,
    Claude,
    Codex,
    Gemini,
}

impl AgentKind {
    /// Also the name of its runtime and of its chat dir.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    /// The name that the user sees.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini CLI",
        }
    }
}

/// Splits a byte stream into records on LF only, as pi's protocol asks:
/// U+2028 and U+2029 are valid inside JSON strings. A CR before the LF is
/// dropped. An incomplete last line waits for the next push. A line over
/// `MAX_LINE` becomes one `TooLong`, and its bytes up to the next LF are dropped.
#[derive(Default)]
pub struct Framer {
    partial: Vec<u8>,
    /// The bytes of `partial` that have no LF.
    scanned: usize,
    dropping: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Record(Vec<u8>),
    TooLong,
}

impl Framer {
    pub fn push(&mut self, mut bytes: &[u8]) -> Vec<Line> {
        let mut lines = Vec::new();
        if self.dropping {
            let Some(end) = bytes.iter().position(|&b| b == b'\n') else {
                return lines;
            };
            bytes = &bytes[end + 1..];
            self.dropping = false;
        }
        self.partial.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(i) = self.partial[self.scanned..].iter().position(|&b| b == b'\n') {
            let end = self.scanned + i;
            if end - start > MAX_LINE {
                lines.push(Line::TooLong);
            } else {
                let mut line = self.partial[start..end].to_vec();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                lines.push(Line::Record(line));
            }
            start = end + 1;
            self.scanned = start;
        }
        self.partial.drain(..start);
        self.scanned = self.partial.len();
        if self.partial.len() > MAX_LINE {
            self.partial = Vec::new();
            self.scanned = 0;
            self.dropping = true;
            lines.push(Line::TooLong);
        }
        lines
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// The commands that the UI may send. Rust builds the JSON that pi gets,
/// so fields that are not listed here never reach pi. Claude Code, Codex
/// and Gemini CLI get no JSON: [`cli::CliAgent`] does the command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentCommand {
    Prompt { message: String },
    Steer { message: String },
    FollowUp { message: String },
    Abort,
    GetState,
    GetMessages,
    SetModel { provider: String, model_id: String },
    GetAvailableModels,
    SetThinkingLevel { level: ThinkingLevel },
    NewSession,
    /// `chat` is a chat id from the chat list, never a path.
    SwitchSession {
        #[serde(skip_serializing)]
        chat: String,
    },
    GetSessionStats,
    SetSessionName { name: String },
}

pub type Sink = Box<dyn Fn(Value) + Send + Sync>;

/// A chat of a shield.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub agent: AgentKind,
    /// The file name. It is unique only per agent.
    pub id: String,
    /// The ISO timestamp of the chat start.
    pub started: String,
    /// The chat name, else the start of the first user message.
    pub title: String,
}

/// A running agent of a shield.
#[derive(Clone)]
pub enum Handle {
    Pi(Arc<pi::Agent>),
    Cli(Arc<cli::CliAgent>),
}

impl Handle {
    /// A reopened Agents tab gets the records from now on.
    pub fn set_sink(&self, sink: Sink) {
        match self {
            Self::Pi(agent) => agent.set_sink(sink),
            Self::Cli(agent) => agent.set_sink(sink),
        }
    }

    /// A turn runs.
    pub fn is_busy(&self) -> bool {
        match self {
            Self::Pi(agent) => agent.is_busy(),
            Self::Cli(agent) => agent.is_busy(),
        }
    }

    /// Asks the agent to exit.
    pub fn close(&self) {
        match self {
            Self::Pi(agent) => agent.close(),
            Self::Cli(agent) => agent.close(),
        }
    }

    pub fn kill(&self) {
        match self {
            Self::Pi(agent) => agent.kill(),
            Self::Cli(agent) => agent.kill(),
        }
    }

    /// Waits until the agent has ended and sent `agent_exit`.
    pub async fn exited(&self) {
        match self {
            Self::Pi(agent) => agent.exited().await,
            Self::Cli(agent) => agent.exited().await,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Queues `input` without a wait. If the queue is full, a task sends it
/// when there is space.
fn send_now(queue: &Sender<PipeInput>, input: PipeInput) {
    if let Err(TrySendError::Full(input)) = queue.try_send(input) {
        let queue = queue.clone();
        tokio::spawn(async move { queue.send(input).await });
    }
}

fn keep(lines: &mut VecDeque<String>, line: String) {
    if lines.len() == STDERR_LINES {
        lines.pop_front();
    }
    lines.push_back(line);
}

/// The first `STDERR_LINE` bytes of `line` as text.
fn cut(line: &[u8]) -> String {
    String::from_utf8_lossy(&line[..line.len().min(STDERR_LINE)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: Vec<Line>) -> Vec<String> {
        lines
            .into_iter()
            .map(|l| match l {
                Line::Record(l) => String::from_utf8(l).unwrap(),
                Line::TooLong => "<too long>".to_owned(),
            })
            .collect()
    }

    #[test]
    fn framer_splits_on_lf_only_and_keeps_partial_lines() {
        let mut framer = Framer::default();
        assert!(framer.push(b"{\"a\":").is_empty());
        assert_eq!(text(framer.push(b"1}\r\n{\"b\":2}\n{\"c\"")), ["{\"a\":1}", "{\"b\":2}"]);
        let separators = "{\"t\":\"x\u{2028}y\u{2029}z\"}\n";
        assert_eq!(text(framer.push(b":3}\n")), ["{\"c\":3}"]);
        assert_eq!(text(framer.push(separators.as_bytes())), [separators.trim_end()]);
        // A multi-byte character split across two reads.
        let bytes = "{\"e\":\"é\"}\n".as_bytes();
        assert!(framer.push(&bytes[..7]).is_empty());
        assert_eq!(text(framer.push(&bytes[7..])), ["{\"e\":\"é\"}"]);
    }

    #[test]
    fn framer_drops_a_line_over_the_limit() {
        let mut framer = Framer::default();
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..16 {
            assert!(framer.push(&chunk).is_empty());
        }
        assert_eq!(text(framer.push(b"x")), ["<too long>"]);
        assert!(framer.push(&chunk).is_empty());
        assert_eq!(text(framer.push(b"x\nok\n")), ["ok"]);

        let mut long = vec![b'y'; MAX_LINE + 1];
        long.extend_from_slice(b"\nnext\n");
        assert_eq!(text(framer.push(&long)), ["<too long>", "next"]);
        let mut fits = vec![b'z'; MAX_LINE];
        fits.push(b'\n');
        assert_eq!(framer.push(&fits), [Line::Record(vec![b'z'; MAX_LINE])]);
    }
}
