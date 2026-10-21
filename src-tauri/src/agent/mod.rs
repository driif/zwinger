//! The agents of a shield: pi, Claude Code, Codex and Gemini CLI. Each one
//! runs in the VM: as its own TUI in an agent session, or headless in a task
//! run, where its JSONL output becomes pi-shaped records.

pub mod claude;
pub mod cli;
pub mod codex;
pub mod gemini;
pub mod guest_login;
pub mod launch;
pub mod pi;

use serde::{Deserialize, Serialize};

/// The guest prefix that belongs to zwinger. User mounts may not use it.
pub const RESERVED: &str = "/opt/zwinger";
/// The per-shield agent state (`<app_data>/shields/<id>`), mounted read-write.
pub const STATE_DIR: &str = "/opt/zwinger/state";
/// The longest record that an agent may write.
const MAX_LINE: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Pi,
    Claude,
    Codex,
    Gemini,
}

impl AgentKind {
    /// Also the name of its runtime and of its command in the VM.
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
