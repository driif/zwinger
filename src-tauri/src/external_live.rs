//! Bounded, read-only tails of the checked-in duo runner's active stderr files.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use serde_json::{Value, json};

use crate::agent::{claude, codex};
use crate::task_artifacts::{Room, RoomParticipant};

const TICK_BYTES: usize = 64 << 10;
const CATCH_UP: u64 = 128 << 10;
const LINE_BYTES: usize = 32 << 10;
const OUTPUT_BYTES: usize = 16 << 10;
const OVERLAP: u64 = 64;
const MAPPER_RECORDS: usize = 256;
const OPEN_TOOLS: usize = 16;
const TOOL_ID_BYTES: usize = 128;
const MATCH_BYTES: usize = TOOL_ID_BYTES + 16;
const CODEX_COMPLETED: &[u8] = br#""type":"item.completed""#;

#[derive(Clone, PartialEq, Eq)]
struct Source {
    agent: String,
    path: String,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Claude,
    Codex,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Pending,
    Structured,
    Legacy,
}

#[derive(Default)]
enum LegacySection {
    #[default]
    Other,
    Exec,
    ApplyPatch,
}

fn sources(room: &Room) -> Vec<Source> {
    let Some(stage) = room.stages.iter().find(|s| s.state == "active") else {
        return Vec::new();
    };
    let role = |id: &str| room.agents.iter().any(|a| a == id).then_some(id.to_owned());
    let names: Vec<(String, String)> = match stage.id.as_str() {
        "plan" => {
            ["opus", "sol"].into_iter().filter_map(|a| Some((role(a)?, format!("turns/plan-{a}.raw.err")))).collect()
        }
        "final" => ["opus", "sol"].into_iter().filter_map(|a| Some((role(a)?, format!("final-{a}.raw.err")))).collect(),
        id if id.starts_with("exchange-") => {
            let Some(n) =
                id.strip_prefix("exchange-").filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            else {
                return Vec::new();
            };
            ["opus", "sol"].into_iter().filter_map(|a| Some((role(a)?, format!("turns/t{n}-{a}.raw.err")))).collect()
        }
        "tiebreak" => role("opus").map(|a| vec![(a, "tiebreak.raw.err".to_owned())]).unwrap_or_default(),
        "implement" | "revise" => room
            .implementer
            .as_deref()
            .and_then(role)
            .map(|a| vec![(a, format!("{}.raw.err", if stage.id == "implement" { "implement" } else { "revise" }))])
            .unwrap_or_default(),
        "crossreview" => room
            .active_actor
            .as_deref()
            .and_then(role)
            .map(|a| vec![(a, "crossreview.raw.err".to_owned())])
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    names
        .into_iter()
        .filter_map(|(agent, path)| {
            let kind = match agent.as_str() {
                "opus" => Kind::Claude,
                "sol" => Kind::Codex,
                _ => return None,
            };
            Some(Source { agent, path, kind })
        })
        .collect()
}

enum Mapper {
    Claude(claude::Mapper),
    Codex(codex::Mapper),
}

impl Mapper {
    fn new(kind: Kind) -> Self {
        match kind {
            Kind::Claude => Self::Claude(claude::Mapper::default()),
            Kind::Codex => Self::Codex(codex::Mapper::default()),
        }
    }

    fn map(&mut self, value: &Value) -> Vec<Value> {
        match self {
            Self::Claude(mapper) => mapper.map(value),
            Self::Codex(mapper) => mapper.map(value),
        }
    }
}

struct Cursor {
    dev: u64,
    ino: u64,
    modified: (i64, i64),
    offset: u64,
    overlap: Vec<u8>,
    partial: Vec<u8>,
    dropping: bool,
    open_tools: Vec<String>,
    dropped: Dropped,
    discard_first: bool,
    format: Format,
    mapper: Mapper,
    mapped: usize,
    legacy_section: LegacySection,
    legacy_reported: bool,
    skip_reported: bool,
}

impl Cursor {
    fn new(meta: &Metadata, kind: Kind, file: &mut File) -> Self {
        let offset = meta.len().saturating_sub(CATCH_UP);
        let mut preceding = [0];
        let discard_first = offset > 0
            && (file.seek(SeekFrom::Start(offset - 1)).is_err()
                || file.read_exact(&mut preceding).is_err()
                || preceding[0] != b'\n');
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            offset,
            overlap: Vec::new(),
            partial: Vec::new(),
            dropping: false,
            open_tools: Vec::new(),
            dropped: Dropped::default(),
            discard_first,
            format: match kind {
                Kind::Claude => Format::Structured,
                Kind::Codex if offset > 0 || discard_first => Format::Legacy,
                Kind::Codex => Format::Pending,
            },
            mapper: Mapper::new(kind),
            mapped: 0,
            legacy_section: LegacySection::default(),
            legacy_reported: false,
            skip_reported: offset == 0,
        }
    }

    /// Drops the current line. Only tool ids in its bytes are kept, to end the tools whose result it holds.
    fn start_drop(&mut self, kind: Kind) {
        self.dropping = true;
        self.dropped = Dropped { matched: vec![false; self.open_tools.len()], ..Dropped::default() };
        for byte in std::mem::take(&mut self.partial) {
            self.dropped.scan(byte, kind, &self.open_tools);
        }
    }

    /// Ends the dropped line: the open tools whose result it held.
    fn finish_drop(&mut self, kind: Kind) -> Vec<String> {
        let dropped = std::mem::take(&mut self.dropped);
        self.dropping = false;
        if kind == Kind::Codex && !dropped.completed {
            return Vec::new();
        }
        let mut index = 0;
        let mut ended = Vec::new();
        self.open_tools.retain(|id| {
            let matched = dropped.matched.get(index).copied().unwrap_or(false);
            index += 1;
            if matched {
                ended.push(id.clone());
            }
            !matched
        });
        ended
    }
}

/// Bounded scan of a dropped line: the last bytes, and which open tool ids it names as a result.
#[derive(Default)]
struct Dropped {
    carry: VecDeque<u8>,
    matched: Vec<bool>,
    completed: bool,
}

impl Dropped {
    fn scan(&mut self, byte: u8, kind: Kind, open: &[String]) {
        if self.carry.len() == MATCH_BYTES {
            self.carry.pop_front();
        }
        self.carry.push_back(byte);
        if byte != b'"' {
            return;
        }
        let ends_with = |pattern: &mut dyn Iterator<Item = u8>, len: usize| {
            self.carry.len() >= len && self.carry.iter().skip(self.carry.len() - len).copied().eq(pattern)
        };
        let completed = kind == Kind::Codex && ends_with(&mut CODEX_COMPLETED.iter().copied(), CODEX_COMPLETED.len());
        let key: &[u8] = if kind == Kind::Claude { br#""tool_use_id":""# } else { br#""id":""# };
        let found: Vec<bool> = open
            .iter()
            .map(|id| {
                let mut pattern = key.iter().copied().chain(id.bytes()).chain(*b"\"");
                ends_with(&mut pattern, key.len() + id.len() + 1)
            })
            .collect();
        self.completed |= completed;
        for (matched, found) in self.matched.iter_mut().zip(found) {
            *matched |= found;
        }
    }
}

pub struct Tail {
    selected: Vec<Source>,
    participants: Vec<RoomParticipant>,
    stage: Option<(String, Option<String>)>,
    cursors: BTreeMap<String, Cursor>,
}

pub struct Tick {
    pub reset: bool,
    pub reset_agents: BTreeSet<String>,
    pub records: Vec<(String, Value)>,
}

impl Tail {
    pub fn new() -> Self {
        Self { selected: Vec::new(), participants: Vec::new(), stage: None, cursors: BTreeMap::new() }
    }

    pub fn tick(&mut self, root: &Path, room: &Room) -> Tick {
        let selected = sources(room);
        let stage = room.stages.iter().find(|s| s.state == "active").map(|s| (s.id.clone(), s.started.clone()));
        let mut result = Tick {
            reset: selected != self.selected || self.participants != room.participants || self.stage != stage,
            reset_agents: BTreeSet::new(),
            records: Vec::new(),
        };
        if result.reset {
            self.cursors.clear();
            self.selected = selected;
            self.participants = room.participants.clone();
            self.stage = stage;
        }
        for source in &self.selected {
            let Some(mut file) = open_file(root, &source.path) else {
                if self.cursors.remove(&source.path).is_some() {
                    result.reset_agents.insert(source.agent.clone());
                }
                continue;
            };
            let Ok(meta) = file.metadata() else { continue };
            let stale = self.cursors.get(&source.path).is_some_and(|old| {
                old.dev != meta.dev()
                    || old.ino != meta.ino()
                    || meta.len() < old.offset
                    || ((meta.mtime(), meta.mtime_nsec()) != old.modified && meta.len() == old.offset)
                    || !overlap_matches(&mut file, old)
            });
            if stale {
                self.cursors.remove(&source.path);
                result.reset_agents.insert(source.agent.clone());
            }
            let cursor =
                self.cursors.entry(source.path.clone()).or_insert_with(|| Cursor::new(&meta, source.kind, &mut file));
            if cursor.offset > 0 && !cursor.skip_reported {
                cursor.skip_reported = true;
                result.records.push((source.agent.clone(), notice("Earlier live history was skipped.")));
            }
            let count = (meta.len().saturating_sub(cursor.offset) as usize).min(TICK_BYTES);
            if count == 0 {
                continue;
            }
            if file.seek(SeekFrom::Start(cursor.offset)).is_err() {
                continue;
            }
            let mut bytes = vec![0; count];
            let Ok(read) = file.read(&mut bytes) else { continue };
            bytes.truncate(read);
            cursor.offset += read as u64;
            let read_start = cursor.offset - read as u64;
            cursor.modified = (meta.mtime(), meta.mtime_nsec());
            let start = cursor.offset.saturating_sub(OVERLAP);
            let amount = (cursor.offset - start) as usize;
            cursor.overlap = vec![0; amount];
            if file.seek(SeekFrom::Start(start)).is_ok() {
                let _ = file.read_exact(&mut cursor.overlap);
            }
            let mut emitted = 0;
            let mut skipped = false;
            for (position, byte) in bytes.into_iter().enumerate() {
                if byte == b'\n' {
                    if cursor.discard_first {
                        cursor.discard_first = false;
                    } else if cursor.dropping {
                        if emitted < MAPPER_RECORDS {
                            result.records.push((source.agent.clone(), notice("A large live record was skipped.")));
                            emitted += 1;
                        } else {
                            skipped = true;
                        }
                        // Ends of open tools pass the record bound: each removes one of at most OPEN_TOOLS ids.
                        for id in cursor.finish_drop(source.kind) {
                            result.records.push((
                                source.agent.clone(),
                                json!({ "type": "tool_execution_end", "toolCallId": id, "isError": false, "result": { "content": [] } }),
                            ));
                        }
                    } else {
                        if cursor.partial.last() == Some(&b'\r') {
                            cursor.partial.pop();
                        }
                        if let Ok(line) = std::str::from_utf8(&cursor.partial) {
                            if cursor.format == Format::Pending {
                                cursor.format = match serde_json::from_str::<Value>(line) {
                                    Ok(value)
                                        if matches!(
                                            value["type"].as_str(),
                                            Some("thread.started" | "turn.started")
                                        ) =>
                                    {
                                        Format::Structured
                                    }
                                    _ => Format::Legacy,
                                };
                            }
                            if cursor.format == Format::Structured {
                                if let Ok(value) = serde_json::from_str::<Value>(line) {
                                    for (index, record) in cursor.mapper.map(&value).into_iter().enumerate() {
                                        if record == value {
                                            continue;
                                        }
                                        if let Some(mut public) = public_record(record) {
                                            let tool = public["toolCallId"].as_str().map(str::to_owned);
                                            let open_end = public["type"] == "tool_execution_end"
                                                && cursor.open_tools.iter().any(|id| Some(id) == tool.as_ref());
                                            if open_end {
                                                cursor.open_tools.retain(|id| Some(id) != tool.as_ref());
                                            } else if emitted >= MAPPER_RECORDS {
                                                skipped = true;
                                                continue;
                                            }
                                            if public["type"] == "tool_execution_start"
                                                && let Some(id) = tool
                                                && id.len() <= TOOL_ID_BYTES
                                                && cursor.open_tools.len() < OPEN_TOOLS
                                            {
                                                cursor.open_tools.push(id);
                                            }
                                            if let Some(message) = public.get_mut("message")
                                                && let Some(object) = message.as_object_mut()
                                            {
                                                object.insert(
                                                    "id".into(),
                                                    json!(format!(
                                                        "{}:{}:{index}",
                                                        source.path,
                                                        read_start + position as u64
                                                    )),
                                                );
                                            }
                                            result.records.push((source.agent.clone(), public));
                                            emitted += 1;
                                        }
                                    }
                                    cursor.mapped += 1;
                                }
                            } else if cursor.format == Format::Legacy
                                && legacy_progress(line, &mut cursor.legacy_section)
                            {
                                if !cursor.legacy_reported {
                                    cursor.legacy_reported = true;
                                    if emitted < MAPPER_RECORDS {
                                        result.records.push((
                                            source.agent.clone(),
                                            notice(
                                                "Legacy Codex diagnostic progress. Public command output is not available.",
                                            ),
                                        ));
                                        emitted += 1;
                                    } else {
                                        skipped = true;
                                    }
                                }
                                let progress = match cursor.legacy_section {
                                    LegacySection::Exec => Some("Legacy Codex: running a command."),
                                    LegacySection::ApplyPatch => Some("Legacy Codex: applying a patch."),
                                    LegacySection::Other => None,
                                };
                                if let Some(progress) = progress {
                                    if emitted < MAPPER_RECORDS {
                                        result.records.push((source.agent.clone(), notice(progress)));
                                        emitted += 1;
                                    } else {
                                        skipped = true;
                                    }
                                }
                            }
                        } else if cursor.format == Format::Pending {
                            cursor.format = Format::Legacy;
                        }
                    }
                    cursor.partial.clear();
                } else if cursor.dropping {
                    cursor.dropped.scan(byte, source.kind, &cursor.open_tools);
                } else if !cursor.discard_first {
                    if cursor.partial.len() == LINE_BYTES {
                        cursor.start_drop(source.kind);
                        cursor.dropped.scan(byte, source.kind, &cursor.open_tools);
                        if cursor.format == Format::Pending {
                            cursor.format = Format::Legacy;
                        }
                    } else {
                        cursor.partial.push(byte);
                    }
                }
            }
            if skipped {
                result
                    .records
                    .push((source.agent.clone(), notice("Live records were skipped to keep the room responsive.")));
            }
            if cursor.mapped >= MAPPER_RECORDS {
                result.reset_agents.insert(source.agent.clone());
                result
                    .records
                    .push((source.agent.clone(), notice("Earlier live history was cleared to bound memory.")));
                cursor.mapper = Mapper::new(source.kind);
                cursor.mapped = 0;
                if !cursor.partial.is_empty() {
                    cursor.start_drop(source.kind);
                }
            }
        }
        result
    }
}

fn open_file(root: &Path, path: &str) -> Option<File> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let folder = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let dir: OwnedFd = rustix::fs::open(root, folder, Mode::empty()).ok()?;
    let (dir, name) = match path.split_once('/') {
        Some(("turns", name)) if !name.contains('/') => {
            (rustix::fs::openat(&dir, "turns", folder, Mode::empty()).ok()?, name)
        }
        None => (dir, path),
        _ => return None,
    };
    let file = File::from(rustix::fs::openat(&dir, name, flags, Mode::empty()).ok()?);
    file.metadata().ok()?.is_file().then_some(file)
}

fn overlap_matches(file: &mut File, old: &Cursor) -> bool {
    if old.overlap.is_empty() {
        return true;
    }
    let start = old.offset - old.overlap.len() as u64;
    let mut bytes = vec![0; old.overlap.len()];
    file.seek(SeekFrom::Start(start)).is_ok() && file.read_exact(&mut bytes).is_ok() && bytes == old.overlap
}

fn clip(text: &str) -> String {
    text.chars()
        .scan(0, |size, ch| {
            *size += ch.len_utf8();
            (*size <= OUTPUT_BYTES).then_some(ch)
        })
        .collect()
}

fn public_record(mut record: Value) -> Option<Value> {
    match record["type"].as_str()? {
        "message_start" if record["message"]["role"] == "assistant" => Some(record),
        "message_update"
            if matches!(
                record["assistantMessageEvent"]["type"].as_str(),
                Some("text_start" | "text_delta" | "text_end")
            ) =>
        {
            let event = &mut record["assistantMessageEvent"];
            if event["contentIndex"].as_u64().is_none_or(|index| index > 32) {
                return None;
            }
            for key in ["delta", "content"] {
                if let Some(text) = event[key].as_str() {
                    event[key] = json!(clip(text));
                }
            }
            Some(record)
        }
        "message_end" => {
            let message = &mut record["message"];
            let assistant = message["role"] == "assistant";
            if !assistant && message["role"] != "toolResult" {
                return None;
            }
            let blocks = message["content"].as_array_mut()?;
            blocks.retain(|block| match block["type"].as_str() {
                Some("text") => block["text"].is_string(),
                Some("toolCall") => block["id"].is_string() && block["name"].is_string(),
                _ => false,
            });
            for block in blocks {
                if let Some(text) = block["text"].as_str() {
                    block["text"] = json!(clip(text));
                }
            }
            if assistant && message["content"].as_array().is_some_and(Vec::is_empty) {
                return None;
            }
            Some(record)
        }
        "tool_execution_start" => Some(record),
        "tool_execution_update" | "tool_execution_end" => {
            let key = if record["type"] == "tool_execution_end" { "result" } else { "partialResult" };
            if let Some(blocks) = record[key]["content"].as_array_mut() {
                for block in blocks {
                    if let Some(text) = block["text"].as_str() {
                        block["text"] = json!(clip(text));
                    }
                }
            }
            Some(record)
        }
        _ => None,
    }
}

fn notice(text: &str) -> Value {
    json!({ "type": "message_end", "message": { "role": "notice", "content": text } })
}

fn legacy_progress(line: &str, section: &mut LegacySection) -> bool {
    *section = match line {
        "exec" => LegacySection::Exec,
        "apply_patch" => LegacySection::ApplyPatch,
        "thinking" | "codex" | "user" | "tokens used" | "file update" => LegacySection::Other,
        _ => return false,
    };
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_artifacts::RoundReader;
    use std::io::Write;
    use std::path::PathBuf;

    fn round(stage: &str) -> (PathBuf, Room) {
        let dir = std::env::temp_dir().join(format!("zwinger-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        std::fs::write(dir.join("turns/plan-opus.prompt.md"), "public").unwrap();
        std::fs::write(dir.join("turns/plan-sol.prompt.md"), "public").unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            format!(r#"{{"at":"2026-01-01T00:00:00Z","kind":"stage","detail":"{stage}"}}"#),
        )
        .unwrap();
        let mut reader = RoundReader::new(dir.clone());
        reader.refresh();
        (dir, reader.room())
    }

    fn refresh(dir: &Path) -> Room {
        let mut reader = RoundReader::new(dir.to_owned());
        reader.refresh();
        reader.room()
    }

    fn text_event(text: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}},"parent_tool_use_id":null}}"#
        )
    }

    #[test]
    fn live_tail_handles_split_records_and_replay_without_hidden_text() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let mut tail = Tail::new();
        assert!(tail.tick(&dir, &room).records.is_empty());
        std::fs::write(&path, format!("{}\r", text_event("Public first"))).unwrap();
        assert!(tail.tick(&dir, &room).records.is_empty());
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "\n{}\n", text_event("Public second")).unwrap();
        let records = tail.tick(&dir, &room).records;
        assert_eq!(records.len(), 2);
        assert_ne!(records[0].1["message"]["id"], records[1].1["message"]["id"]);
        assert_eq!(records[1].1["message"]["content"][0]["text"], "Public second");
        assert!(tail.tick(&dir, &room).records.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_tail_frames_utf8_and_recovers_after_bad_and_oversize_lines() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let event = text_event("Café");
        let bytes = event.as_bytes();
        let split = bytes.iter().position(|b| *b == 0xc3).unwrap() + 1;
        std::fs::write(&path, &bytes[..split]).unwrap();
        let mut tail = Tail::new();
        assert!(tail.tick(&dir, &room).records.is_empty());
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&bytes[split..]).unwrap();
        file.write_all(b"\n{broken json}\n").unwrap();
        file.write_all(&vec![b'x'; LINE_BYTES + 1]).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(format!("{}\n", text_event("After bad lines")).as_bytes()).unwrap();
        let records = tail.tick(&dir, &room).records;
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].1["message"]["content"][0]["text"], "Café");
        assert_eq!(records[1].1, notice("A large live record was skipped."));
        assert_eq!(records[2].1["message"]["content"][0]["text"], "After bad lines");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn public_filter_suppresses_thinking_signatures_and_unknown_records() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let hidden = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"reasoning sentinel"},{"type":"redacted_thinking","data":"signature sentinel"},{"type":"text","text":"Public answer"}]},"parent_tool_use_id":null}"#;
        std::fs::write(&path, format!("{hidden}\n{{\"type\":\"unknown\",\"text\":\"unknown sentinel\"}}\n")).unwrap();
        let mut tail = Tail::new();
        let records = tail.tick(&dir, &room).records;
        assert_eq!(records.len(), 1);
        let visible = records[0].1.to_string();
        assert!(visible.contains("Public answer"));
        assert!(!visible.contains("sentinel"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn structured_codex_keeps_public_text_and_tool_result() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-sol.raw.err");
        let lines = [
            r#"{"type":"thread.started","thread_id":"thread-1"}"#,
            r#"{"type":"item.completed","item":{"id":"one","type":"agent_message","text":"Public text"}}"#,
            r#"{"type":"item.completed","item":{"id":"hidden","type":"reasoning","text":"reasoning sentinel"}}"#,
            r#"{"type":"item.completed","item":{"id":"tool-1","type":"command_execution","command":"pwd","aggregated_output":"public result","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"passthrough sentinel"}]}}"#,
            r#"{"type":"item.completed","item":{"id":"bad","type":"agent_message","text":{"secret":"malformed sentinel"}}}"#,
        ];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let mut tail = Tail::new();
        let records = tail.tick(&dir, &room).records;
        assert_eq!(records.len(), 4);
        assert!(records.iter().any(|(_, r)| r["message"]["content"][0]["text"] == "Public text"));
        assert!(records.iter().any(|(_, r)| r["message"]["role"] == "toolResult"));
        assert!(records.iter().all(|(_, r)| !r.to_string().contains("sentinel")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_tail_resets_on_overlap_change_replacement_and_stage_end() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let mut tail = Tail::new();
        std::fs::write(&path, format!("{}\n", text_event("First"))).unwrap();
        assert_eq!(tail.tick(&dir, &room).records.len(), 1);
        let replacement = format!("{}\n", text_event("Other"));
        std::fs::write(&path, replacement).unwrap();
        let reset = tail.tick(&dir, &room);
        assert!(!reset.reset);
        assert!(reset.reset_agents.contains("opus"));
        assert_eq!(reset.records.len(), 1);
        std::fs::remove_file(&path).unwrap();
        assert!(tail.tick(&dir, &room).reset_agents.contains("opus"));
        std::fs::write(&path, format!("{}\n", text_event("Again"))).unwrap();
        assert!(tail.tick(&dir, &room).records.len() == 1);
        let events = dir.join("events.jsonl");
        let mut file = std::fs::OpenOptions::new().append(true).open(&events).unwrap();
        writeln!(file, "\n{{\"at\":\"2026-01-01T00:00:01Z\",\"kind\":\"stage\",\"detail\":\"gates\"}}").unwrap();
        let reset = tail.tick(&dir, &refresh(&dir));
        assert!(reset.reset);
        assert!(reset.records.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn participant_file_reset_and_legacy_burst_leave_other_live_output_intact() {
        let (dir, room) = round("plan");
        let opus = dir.join("turns/plan-opus.raw.err");
        let sol = dir.join("turns/plan-sol.raw.err");
        std::fs::write(&opus, format!("{}\n", text_event("Opus stays"))).unwrap();
        std::fs::write(&sol, "exec\nfirst\n").unwrap();
        let mut tail = Tail::new();
        let first = tail.tick(&dir, &room);
        assert!(
            first.records.iter().any(|(agent, record)| agent == "opus" && record.to_string().contains("Opus stays"))
        );

        std::fs::write(&sol, "exec\nsecond\n").unwrap();
        let reset = tail.tick(&dir, &room);
        assert!(!reset.reset);
        assert_eq!(reset.reset_agents.into_iter().collect::<Vec<_>>(), ["sol"]);
        assert!(reset.records.iter().all(|(agent, _)| agent == "sol"));

        let mut file = std::fs::OpenOptions::new().append(true).open(&sol).unwrap();
        for _ in 0..300 {
            writeln!(file, "exec\ncommand").unwrap();
        }
        let burst = tail.tick(&dir, &room);
        assert!(!burst.reset);
        assert!(burst.reset_agents.is_empty());
        assert!(burst.records.len() <= MAPPER_RECORDS + 1);
        assert!(burst.records.iter().any(|(_, record)| record.to_string().contains("skipped")));

        std::fs::remove_file(&sol).unwrap();
        tail.tick(&dir, &room);
        let mut file = std::fs::File::create(&sol).unwrap();
        writeln!(file, "{{\"type\":\"thread.started\"}}").unwrap();
        for _ in 0..MAPPER_RECORDS {
            writeln!(file, "{{\"type\":\"turn.started\"}}").unwrap();
        }
        let bound = tail.tick(&dir, &room);
        assert!(!bound.reset);
        assert_eq!(bound.reset_agents.into_iter().collect::<Vec<_>>(), ["sol"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn catch_up_keeps_record_when_offset_follows_newline() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let record = format!("{}\n", text_event("At boundary"));
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(&vec![b'x'; 90]).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(record.as_bytes()).unwrap();
        file.write_all(&vec![b'\n'; CATCH_UP as usize - record.len()]).unwrap();
        let records = Tail::new().tick(&dir, &room).records;
        assert!(records.iter().any(|(_, value)| value.to_string().contains("At boundary")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_tail_rejects_links_and_legacy_reasoning() {
        let (dir, room) = round("plan");
        let outside = dir.join("outside");
        std::fs::write(&outside, text_event("Outside")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("turns/plan-opus.raw.err")).unwrap();
        std::fs::write(
            dir.join("turns/plan-sol.raw.err"),
            "OpenAI Codex\nthinking\nprivate reasoning sentinel\nexec\npublic-command\n",
        )
        .unwrap();
        let mut tail = Tail::new();
        let records = tail.tick(&dir, &room).records;
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|(agent, record)| agent == "sol" && !record.to_string().contains("sentinel")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_codex_never_maps_embedded_json_or_command_text() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-sol.raw.err");
        let lines = [
            "OpenAI Codex",
            "exec",
            "bash -lc 'echo cmd-sentinel'",
            "Process exited with code 0",
            "Final output:",
            r#"{"type":"item.completed","item":{"id":"one","type":"agent_message","text":"message sentinel"}}"#,
            r#"{"type":"item.completed","item":{"id":"two","type":"command_execution","command":"secret sentinel","aggregated_output":"output sentinel","status":"completed"}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"pi sentinel"}]}}"#,
            "thinking",
            "reasoning sentinel",
            r#"{"type":"item.completed","item":{"id":"three","type":"agent_message","text":"thinking sentinel"}}"#,
            "apply_patch",
            "*** Begin Patch secret sentinel",
        ];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let records = Tail::new().tick(&dir, &room).records;
        assert_eq!(records.len(), 3);
        assert_eq!(records.iter().filter(|(_, record)| record.to_string().contains("running a command")).count(), 1);
        assert_eq!(records.iter().filter(|(_, record)| record.to_string().contains("applying a patch")).count(), 1);
        assert!(records.iter().all(|(_, record)| record["message"]["role"] == "notice"));
        assert!(records.iter().all(|(_, record)| !record.to_string().contains("sentinel")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_format_stays_legacy_until_cursor_resets() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-sol.raw.err");
        std::fs::write(
            &path,
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"one\",\"type\":\"agent_message\",\"text\":\"hidden\"}}\n",
        )
        .unwrap();
        let mut tail = Tail::new();
        assert!(tail.tick(&dir, &room).records.is_empty());
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{\"type\":\"thread.started\"}}").unwrap();
        writeln!(file, "{{\"type\":\"item.completed\",\"item\":{{\"id\":\"two\",\"type\":\"agent_message\",\"text\":\"still hidden\"}}}}").unwrap();
        assert!(tail.tick(&dir, &room).records.is_empty());
        assert!(matches!(tail.cursors["turns/plan-sol.raw.err"].format, Format::Legacy));
        std::fs::remove_file(&path).unwrap();
        tail.tick(&dir, &room);
        std::fs::write(&path, format!("{{\"type\":\"turn.started\"}}\n{{\"type\":\"item.completed\",\"item\":{{\"id\":\"three\",\"type\":\"agent_message\",\"text\":\"visible\"}}}}\n")).unwrap();
        let records = tail.tick(&dir, &room).records;
        assert!(records.iter().any(|(_, record)| record.to_string().contains("visible")));
        assert!(matches!(tail.cursors["turns/plan-sol.raw.err"].format, Format::Structured));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_tail_rejects_fifo_directory_and_linked_turns() {
        let (dir, room) = round("plan");
        let fifo = dir.join("turns/plan-opus.raw.err");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        std::fs::create_dir(dir.join("turns/plan-sol.raw.err")).unwrap();
        assert!(Tail::new().tick(&dir, &room).records.is_empty());

        std::fs::remove_dir_all(dir.join("turns")).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("plan-opus.raw.err"), format!("{}\n", text_event("Outside"))).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("turns")).unwrap();
        assert!(Tail::new().tick(&dir, &room).records.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sparse_log_larger_than_artifact_limit_keeps_recent_record() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len(9 << 20).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(format!("\n{}\n", text_event("Recent after sparse gap")).as_bytes()).unwrap();
        let len = file.metadata().unwrap().len();
        let mut tail = Tail::new();
        let mut records = tail.tick(&dir, &room).records;
        assert!(tail.cursors["turns/plan-opus.raw.err"].offset >= len - CATCH_UP);
        for _ in 0..3 {
            records.extend(tail.tick(&dir, &room).records);
        }
        assert!(records.iter().any(|(_, record)| record.to_string().contains("Recent after sparse gap")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn public_filter_rejects_malformed_message_content() {
        assert!(
            public_record(json!({ "type": "message_end", "message": { "role": "assistant", "content": "bad" } }))
                .is_none()
        );
        assert!(
            public_record(json!({ "type": "message_end", "message": { "role": "toolResult", "content": null } }))
                .is_none()
        );
        let record = public_record(json!({ "type": "message_end", "message": { "role": "assistant", "content": [
            { "type": "text", "text": 4 },
            { "type": "toolCall", "id": null, "name": "bash" },
            { "type": "text", "text": "Public" }
        ] } }))
        .unwrap();
        assert_eq!(record["message"]["content"], json!([{ "type": "text", "text": "Public" }]));
    }

    #[test]
    fn large_initial_tail_skips_to_record_boundary() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(&vec![b'x'; CATCH_UP as usize + 100]).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{}", text_event("Recent public text")).unwrap();
        let mut tail = Tail::new();
        let first = tail.tick(&dir, &room);
        assert!(first.records.iter().any(|(_, r)| r.to_string().contains("skipped")));
        assert!(tail.cursors.values().all(|c| c.partial.len() <= LINE_BYTES));
        assert!(tail.cursors.values().all(|c| c.offset <= 1 + 100 + TICK_BYTES as u64 + 200));
        for _ in 0..3 {
            tail.tick(&dir, &room);
        }
        assert!(
            tail.cursors
                .values()
                .all(|c| c.offset <= std::fs::metadata(dir.join("turns/plan-opus.raw.err")).unwrap().len())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn tool_use_event(id: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{id}","name":"Read","input":{{"file_path":"a.txt"}}}}]}},"parent_tool_use_id":null}}"#
        )
    }

    #[test]
    fn oversized_tool_result_gives_notice_and_ends_open_tool() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        std::fs::write(&path, format!("{}\n", tool_use_event("toolu_big"))).unwrap();
        let mut tail = Tail::new();
        let started = tail.tick(&dir, &room).records;
        assert!(started.iter().any(|(_, r)| r["type"] == "tool_execution_start" && r["toolCallId"] == "toolu_big"));
        let big = "dropped sentinel ".repeat(LINE_BYTES / 8);
        let result = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_big","content":"{big}"}}]}},"tool_use_result":"{big}"}}"#
        );
        assert!(result.len() > LINE_BYTES);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{result}").unwrap();
        writeln!(file, "{}", text_event("After large result")).unwrap();
        let mut records = Vec::new();
        for _ in 0..4 {
            records.extend(tail.tick(&dir, &room).records);
        }
        assert!(records.iter().any(|(_, r)| *r == notice("A large live record was skipped.")));
        let ended: Vec<_> = records.iter().filter(|(_, r)| r["type"] == "tool_execution_end").collect();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].1["toolCallId"], "toolu_big");
        assert_eq!(ended[0].1["isError"], false);
        assert!(records.iter().any(|(_, r)| r.to_string().contains("After large result")));
        assert!(records.iter().all(|(_, r)| !r.to_string().contains("sentinel")));
        assert!(tail.cursors["turns/plan-opus.raw.err"].open_tools.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_tool_ids_are_bounded_and_cleared_on_reset() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let mut lines = String::new();
        for n in 0..OPEN_TOOLS + 10 {
            lines.push_str(&tool_use_event(&format!("toolu_{n}")));
            lines.push('\n');
        }
        lines.push_str(&tool_use_event(&"x".repeat(TOOL_ID_BYTES + 1)));
        lines.push('\n');
        std::fs::write(&path, &lines).unwrap();
        let mut tail = Tail::new();
        tail.tick(&dir, &room);
        let open = &tail.cursors["turns/plan-opus.raw.err"].open_tools;
        assert_eq!(open.len(), OPEN_TOOLS);
        assert!(open.iter().all(|id| id.len() <= TOOL_ID_BYTES));
        std::fs::write(&path, format!("{}\n", text_event("Replaced"))).unwrap();
        let reset = tail.tick(&dir, &room);
        assert!(reset.reset_agents.contains("opus"));
        assert!(tail.cursors["turns/plan-opus.raw.err"].open_tools.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn ended(records: &[(String, Value)]) -> Vec<String> {
        records
            .iter()
            .filter(|(_, r)| r["type"] == "tool_execution_end")
            .map(|(_, r)| r["toolCallId"].as_str().unwrap().to_owned())
            .collect()
    }

    fn drain(tail: &mut Tail, dir: &Path, room: &Room) -> Vec<(String, Value)> {
        (0..4).flat_map(|_| tail.tick(dir, room).records).collect()
    }

    fn big_result(id: &str, parent: &str) -> String {
        let big = "dropped sentinel ".repeat(LINE_BYTES / 8);
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{id}","content":"{big}"}}]}},"parent_tool_use_id":"{parent}"}}"#
        )
    }

    #[test]
    fn oversized_result_ends_only_its_tool_not_parent_or_sibling() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let lines = [tool_use_event("toolu_task"), tool_use_event("toolu_read"), tool_use_event("toolu_other")];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let mut tail = Tail::new();
        tail.tick(&dir, &room);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", big_result("toolu_read", "toolu_task")).unwrap();
        let records = drain(&mut tail, &dir, &room);
        assert!(records.iter().any(|(_, r)| *r == notice("A large live record was skipped.")));
        assert_eq!(ended(&records), ["toolu_read"]);
        assert_eq!(tail.cursors["turns/plan-opus.raw.err"].open_tools, ["toolu_task", "toolu_other"]);
        assert!(records.iter().all(|(_, r)| !r.to_string().contains("sentinel")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_tool_use_line_ends_nothing() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        std::fs::write(&path, format!("{}\n", tool_use_event("toolu_task"))).unwrap();
        let mut tail = Tail::new();
        tail.tick(&dir, &room);
        let big = "write sentinel ".repeat(LINE_BYTES / 8);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_write","name":"Write","input":{{"content":"{big}"}}}}]}},"parent_tool_use_id":"toolu_task"}}"#
        )
        .unwrap();
        let records = drain(&mut tail, &dir, &room);
        assert!(records.iter().any(|(_, r)| *r == notice("A large live record was skipped.")));
        assert!(ended(&records).is_empty());
        assert_eq!(tail.cursors["turns/plan-opus.raw.err"].open_tools, ["toolu_task"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_structured_codex_completion_ends_its_command() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-sol.raw.err");
        let lines = [
            r#"{"type":"thread.started","thread_id":"thread-1"}"#,
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"cat big","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
            r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"sleep","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
        ];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let mut tail = Tail::new();
        let started = tail.tick(&dir, &room).records;
        assert_eq!(started.iter().filter(|(_, r)| r["type"] == "tool_execution_start").count(), 2);
        let big = "output sentinel ".repeat(LINE_BYTES / 8);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, r#"{{"type":"item.updated","item":{{"id":"item_2","type":"command_execution","command":"sleep","aggregated_output":"{big}","status":"in_progress"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"item.completed","item":{{"id":"item_1","type":"command_execution","command":"cat big","aggregated_output":"{big}","exit_code":0,"status":"completed"}}}}"#).unwrap();
        let records = drain(&mut tail, &dir, &room);
        assert_eq!(records.iter().filter(|(_, r)| *r == notice("A large live record was skipped.")).count(), 2);
        assert_eq!(ended(&records), ["item_1"]);
        assert_eq!(tail.cursors["turns/plan-sol.raw.err"].open_tools, ["item_2"]);
        assert!(records.iter().all(|(_, r)| !r.to_string().contains("sentinel")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dropped_tool_id_split_across_reads_is_matched() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        std::fs::write(&path, format!("{}\n", tool_use_event("toolu_split"))).unwrap();
        let mut tail = Tail::new();
        tail.tick(&dir, &room);
        let big = "dropped sentinel ".repeat(LINE_BYTES / 8);
        let line = format!(
            r#"{{"type":"user","tool_use_result":"{big}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_split","content":"x"}}]}}}}"#
        );
        let cut = line.find("toolu_split").unwrap() + 5;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&line.as_bytes()[..cut]).unwrap();
        let mut records = tail.tick(&dir, &room).records;
        assert!(tail.cursors["turns/plan-opus.raw.err"].dropping);
        assert!(tail.cursors["turns/plan-opus.raw.err"].dropped.carry.len() <= MATCH_BYTES);
        file.write_all(&line.as_bytes()[cut..]).unwrap();
        file.write_all(b"\n").unwrap();
        records.extend(tail.tick(&dir, &room).records);
        assert_eq!(ended(&records), ["toolu_split"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mapper_reset_keeps_open_tools_and_reports_cut_line() {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        let mut text = format!("{}\n", tool_use_event("toolu_cut"));
        for n in 0..MAPPER_RECORDS {
            text.push_str(&text_event(&format!("Line {n}")));
            text.push('\n');
        }
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_cut","content":"cut sentinel"}]}}"#;
        let cut = line.find("cut sentinel").unwrap();
        text.push_str(&line[..cut]);
        std::fs::write(&path, &text).unwrap();
        let mut tail = Tail::new();
        let first = tail.tick(&dir, &room);
        assert!(first.reset_agents.contains("opus"));
        assert_eq!(tail.cursors["turns/plan-opus.raw.err"].open_tools, ["toolu_cut"]);
        assert!(tail.cursors["turns/plan-opus.raw.err"].dropping);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", &line[cut..]).unwrap();
        let records = tail.tick(&dir, &room).records;
        assert!(records.iter().any(|(_, r)| *r == notice("A large live record was skipped.")));
        assert_eq!(ended(&records), ["toolu_cut"]);
        assert!(records.iter().all(|(_, r)| !r.to_string().contains("sentinel")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn open_tool_end_passes_record_bound(result_line: &str) {
        let (dir, room) = round("plan");
        let path = dir.join("turns/plan-opus.raw.err");
        std::fs::write(&path, format!("{}\n", tool_use_event("toolu_a"))).unwrap();
        let mut tail = Tail::new();
        tail.tick(&dir, &room);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        for n in 0..MAPPER_RECORDS + 10 {
            writeln!(file, "{}", text_event(&format!("{n}"))).unwrap();
        }
        writeln!(file, "{result_line}").unwrap();
        assert!(file.metadata().unwrap().len() - tail.cursors["turns/plan-opus.raw.err"].offset <= TICK_BYTES as u64);
        let burst = tail.tick(&dir, &room);
        assert!(burst.records.iter().any(|(_, r)| r.to_string().contains("were skipped")));
        assert_eq!(ended(&burst.records), ["toolu_a"]);
        assert!(tail.cursors["turns/plan-opus.raw.err"].open_tools.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_tool_end_passes_record_bound_for_normal_result() {
        open_tool_end_passes_record_bound(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_a","content":"done"}]}}"#,
        );
    }

    #[test]
    fn open_tool_end_passes_record_bound_for_oversized_result() {
        let big = "s".repeat(LINE_BYTES + 100);
        open_tool_end_passes_record_bound(&format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_a","content":"{big}"}}]}}}}"#
        ));
    }
}
