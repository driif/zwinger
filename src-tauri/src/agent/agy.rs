//! Antigravity CLI (`agy`): its paths in the VM and its first settings. It
//! runs from its downloaded Linux build. It keeps a Google sign-in as a token
//! that it can read in the guest, so zwinger does not reuse the sign-in of
//! the Mac (ADR 0002). The settings select the Gemini API key mode: a
//! `GEMINI_API_KEY` credential gives the guest a placeholder, and the proxy
//! sets the real key.
//!
//! A task call is one `agy -p= --input-format stream-json --output-format
//! stream-json`: the prompt is one `user` line on stdin, and agy ends at the
//! end of stdin. The shapes here are those of agy 1.5.14.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Value, json};

use crate::host_agents;

/// Its `HOME`, so `~/.gemini/antigravity-cli` is in the project's state and never the host's.
pub const HOME: &str = "/opt/zwinger/state/agy";
/// The settings dir below the state dir.
const SETTINGS_DIR: [&str; 3] = ["agy", ".gemini", "antigravity-cli"];
const SETTINGS: &str = "settings.json";
/// The Gemini API key mode. `GEMINI_API_KEY` alone does not select it.
const API_KEY_MODE: &str = "{\n  \"modelProvider\": \"gemini\"\n}\n";

/// Writes the settings of the API key mode in the state dir `state` if it
/// has no settings file. The user's own settings stay as they are.
pub fn seed(state: &Path) -> std::io::Result<()> {
    let dir = host_agents::open_parts(state, &SETTINGS_DIR)?;
    if host_agents::type_at(&dir, SETTINGS)?.is_some() {
        return Ok(());
    }
    host_agents::write_at(&dir, SETTINGS, API_KEY_MODE.as_bytes())
}

/// The stdin of a task call: the prompt as one `user` message line.
pub fn input(prompt: &str) -> Vec<u8> {
    let mut line = json!({ "event": "user", "message": { "content": prompt } }).to_string().into_bytes();
    line.push(b'\n');
    line
}

fn assistant(content: Value) -> Value {
    json!({ "type": "message_end", "message": { "role": "assistant", "content": [content] } })
}

/// Maps the stream-json events of one agy run to pi records. agy sends each
/// step as `step_update` events: an `agent_response` streams its text in
/// `text_delta` and is `DONE` at its end; a `tool` step is `ACTIVE`, then
/// `DONE` or `ERROR`. The step index is the tool call id. The terminal
/// `result` event ends the run. An unknown event passes through as it is.
#[derive(Default)]
pub struct Mapper {
    /// The text of the response that streams now.
    text: Option<String>,
    /// The tool steps that have their call record.
    called: BTreeSet<u64>,
}

impl Mapper {
    pub fn map(&mut self, event: &Value) -> Vec<Value> {
        match event["event"].as_str() {
            Some("init") => Vec::new(),
            Some("step_update") => self.step(&event["step_update"]),
            Some("result") => {
                let result = &event["result"];
                let mut records = self.flush();
                if result["status"] != "SUCCESS" {
                    let error = result["error"].as_str().filter(|e| !e.is_empty()).unwrap_or("Antigravity CLI failed");
                    records.push(json!({
                        "type": "message_end",
                        "message": { "role": "assistant", "content": [], "stopReason": "error", "errorMessage": error },
                    }));
                }
                records.push(json!({ "type": "agent_settled" }));
                records
            }
            _ => vec![event.clone()],
        }
    }

    fn step(&mut self, step: &Value) -> Vec<Value> {
        match step["step_type"].as_str() {
            Some("agent_response") => {
                let mut records = Vec::new();
                if let Some(delta) = step["text_delta"].as_str().filter(|d| !d.is_empty()) {
                    let text = self.text.get_or_insert_with(|| {
                        records.push(
                            json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } }),
                        );
                        String::new()
                    });
                    text.push_str(delta);
                    records.push(json!({
                        "type": "message_update",
                        "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": delta },
                    }));
                }
                if step["state"] != "ACTIVE" {
                    records.extend(self.flush());
                }
                records
            }
            Some("tool") => {
                let mut records = self.flush();
                let index = step["step_index"].as_u64().unwrap_or_default();
                let id = format!("step-{index}");
                let info = &step["tool_info"];
                if self.called.insert(index) {
                    let arguments = if info["parameters"].is_object() { info["parameters"].clone() } else { json!({}) };
                    let name = step["tool_name"].as_str().or(info["name"].as_str()).unwrap_or("tool");
                    records
                        .push(assistant(json!({ "type": "toolCall", "id": id, "name": name, "arguments": arguments })));
                    records.push(json!({ "type": "tool_execution_start", "toolCallId": id }));
                }
                if step["state"] == "ACTIVE" {
                    return records;
                }
                let is_error = step["state"] != "DONE";
                let text = match info["output"].as_str() {
                    Some(output) => output,
                    None => info["error"]["message"].as_str().unwrap_or_default(),
                };
                let content = json!([{ "type": "text", "text": text }]);
                records.push(json!({ "type": "tool_execution_end", "toolCallId": id, "isError": is_error, "result": { "content": content } }));
                records.push(json!({
                    "type": "message_end",
                    "message": { "role": "toolResult", "toolCallId": id, "content": content, "isError": is_error },
                }));
                records
            }
            Some("user_input" | "system_message") => Vec::new(),
            _ => vec![json!({ "event": "step_update", "step_update": step })],
        }
    }

    /// Ends the streamed response with its whole text.
    fn flush(&mut self) -> Vec<Value> {
        match self.text.take() {
            Some(text) => vec![assistant(json!({ "type": "text", "text": text }))],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_selects_the_api_key_mode_once() {
        let state = crate::test_dir::root().join(format!("zwinger-agy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state).unwrap();
        seed(&state).unwrap();
        let file = state.join("agy/.gemini/antigravity-cli/settings.json");
        let settings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(settings["modelProvider"], "gemini");
        // The user's own settings stay.
        std::fs::write(&file, "{\"model\":\"x\"}").unwrap();
        seed(&state).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"model\":\"x\"}");
        let _ = std::fs::remove_dir_all(state);
    }

    fn event(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    /// Lines of a real agy 1.5.14 run, shortened.
    #[test]
    fn maps_a_turn_with_a_tool_call() {
        let mut mapper = Mapper::default();
        let lines = [
            r#"{"event":"init","conversation_id":"9c32","init":{"cwd":"/w","tools":["read_file"],"permission_mode":"request-review"}}"#,
            r#"{"event":"step_update","step_update":{"conversation_id":"9c32","step_index":0,"state":"DONE","step_type":"user_input"}}"#,
            r#"{"event":"step_update","step_update":{"conversation_id":"9c32","step_index":1,"state":"DONE","step_type":"agent_response","duration_seconds":2.6}}"#,
        ];
        for line in lines {
            assert!(mapper.map(&event(line)).is_empty(), "{line}");
        }
        let call = r#"{"event":"step_update","step_update":{"conversation_id":"9c32","step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"Path":"/w/note.txt"}}}}"#;
        let records = mapper.map(&event(call));
        assert_eq!(
            records[0]["message"]["content"],
            json!([{ "type": "toolCall", "id": "step-2", "name": "read_file", "arguments": { "Path": "/w/note.txt" } }])
        );
        assert_eq!(records[1], json!({ "type": "tool_execution_start", "toolCallId": "step-2" }));
        let failed = r#"{"event":"step_update","step_update":{"conversation_id":"9c32","step_index":2,"state":"ERROR","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"Path":"/w/note.txt"},"error":{"type":"TOOL_ERROR","message":"file does not exist"}}}}"#;
        let records = mapper.map(&event(failed));
        assert_eq!(records.len(), 2);
        assert_eq!(
            (records[0]["isError"].clone(), records[0]["result"]["content"][0]["text"].clone()),
            (json!(true), json!("file does not exist"))
        );
        let done = r#"{"event":"step_update","step_update":{"conversation_id":"9c32","step_index":4,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"ls"},"output":"a\nb"}}}"#;
        let records = mapper.map(&event(done));
        assert_eq!(records.len(), 4);
        assert_eq!(
            (records[2]["isError"].clone(), records[2]["result"]["content"][0]["text"].clone()),
            (json!(false), json!("a\nb"))
        );

        let delta = |text: &str| json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": text } });
        let first = r#"{"event":"step_update","step_update":{"conversation_id":"9c32","step_index":6,"state":"ACTIVE","step_type":"agent_response","text_delta":"OK"}}"#;
        assert_eq!(
            mapper.map(&event(first)),
            [json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } }), delta("OK")]
        );
        let last = r#"{"event":"step_update","step_update":{"conversation_id":"9c32","step_index":6,"state":"DONE","step_type":"agent_response","text_delta":"\n","usage":{"total_tokens":3}}}"#;
        let records = mapper.map(&event(last));
        assert_eq!(records[0], delta("\n"));
        assert_eq!(records[1]["message"]["content"], json!([{ "type": "text", "text": "OK\n" }]));
        let result = r#"{"event":"result","result":{"conversation_id":"9c32","status":"SUCCESS","response":"OK\n","num_turns":1}}"#;
        assert_eq!(mapper.map(&event(result)), [json!({ "type": "agent_settled" })]);
    }

    #[test]
    fn maps_a_failed_run_and_passes_unknown_events_through() {
        let mut mapper = Mapper::default();
        let failed = r#"{"event":"result","result":{"conversation_id":"8200","status":"ERROR","response":"","error":"failed to decode stream input"}}"#;
        assert_eq!(
            mapper.map(&event(failed)),
            [
                json!({ "type": "message_end", "message": { "role": "assistant", "content": [], "stopReason": "error", "errorMessage": "failed to decode stream input" } }),
                json!({ "type": "agent_settled" }),
            ]
        );
        let future = event(r#"{"event":"compacted"}"#);
        assert_eq!(mapper.map(&future), std::slice::from_ref(&future));
    }

    #[test]
    fn the_prompt_is_one_user_line() {
        let line = input("Fix \"it\"\nnow");
        assert!(line.ends_with(b"\n") && !line[..line.len() - 1].contains(&b'\n'));
        let value: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value, json!({ "event": "user", "message": { "content": "Fix \"it\"\nnow" } }));
    }
}
