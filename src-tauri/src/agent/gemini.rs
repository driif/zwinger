//! Gemini CLI: one `gemini --output-format stream-json` per task call. It runs from the host install, which the shield
//! mounts read-only: Gemini CLI has no pinned runtime. It authenticates
//! with the host login (ADR 0002) or a `GEMINI_API_KEY` credential.

use serde_json::{Value, json};

/// The host package of Gemini CLI, mounted read-only.
pub const DIR: &str = "/opt/zwinger/host/gemini";
/// Its `HOME`, so `~/.gemini` is in the shield's state and never the host's.
pub const HOME: &str = "/opt/zwinger/state/gemini";

fn assistant(content: Value) -> Value {
    json!({ "type": "message_end", "message": { "role": "assistant", "content": [content] } })
}

/// Maps the events of one Gemini CLI run to pi records. Gemini CLI streams
/// the assistant text as deltas and has no end event for a message, so the
/// streamed text ends at the next tool call or at the result. The agent sets
/// the message timestamps. An unknown event passes through as it is.
#[derive(Default)]
pub struct Mapper {
    /// The text of the assistant message that streams now.
    text: Option<String>,
}

impl Mapper {
    pub fn map(&mut self, event: &Value) -> Vec<Value> {
        match event["type"].as_str() {
            Some("init") => Vec::new(),
            Some("message") if event["role"] == "assistant" => {
                let content = event["content"].as_str().unwrap_or_default();
                if event["delta"] != true {
                    let mut records = self.flush();
                    records.push(assistant(json!({ "type": "text", "text": content })));
                    return records;
                }
                let mut records = Vec::new();
                let text = self.text.get_or_insert_with(|| {
                    records.push(json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } }));
                    String::new()
                });
                text.push_str(content);
                records.push(json!({
                    "type": "message_update",
                    "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": content },
                }));
                records
            }
            Some("message") => Vec::new(),
            Some("tool_use") => {
                let mut records = self.flush();
                let id = &event["tool_id"];
                let arguments = if event["parameters"].is_object() { event["parameters"].clone() } else { json!({}) };
                records.push(assistant(json!({
                    "type": "toolCall",
                    "id": id,
                    "name": event["tool_name"].as_str().unwrap_or("tool"),
                    "arguments": arguments,
                })));
                records.push(json!({ "type": "tool_execution_start", "toolCallId": id }));
                records
            }
            Some("tool_result") => {
                let id = &event["tool_id"];
                let is_error = event["status"] != "success";
                let text = match event["output"].as_str() {
                    Some(output) => output,
                    None => event["error"]["message"].as_str().unwrap_or_default(),
                };
                let content = json!([{ "type": "text", "text": text }]);
                vec![
                    json!({ "type": "tool_execution_end", "toolCallId": id, "isError": is_error, "result": { "content": content } }),
                    json!({
                        "type": "message_end",
                        "message": { "role": "toolResult", "toolCallId": id, "content": content, "isError": is_error },
                    }),
                ]
            }
            // A warning or a retry: the turn goes on.
            Some("error") => vec![json!({ "type": "status", "message": event["message"] })],
            Some("result") => {
                let mut records = self.flush();
                if event["status"] != "success" {
                    let error = event["error"]["message"].as_str().unwrap_or("Gemini CLI failed");
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

    /// Ends the streamed message with its whole text.
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

    fn event(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    // Hand-written fixtures in the documented `stream-json` format of
    // Gemini CLI's headless mode; no run with a real login has shown them yet.
    #[test]
    fn maps_a_turn_with_a_tool_call() {
        let mut mapper = Mapper::default();
        let init = event(r#"{"type":"init","timestamp":"t","session_id":"5f1c","model":"gemini-2.5-pro"}"#);
        assert!(mapper.map(&init).is_empty());
        assert!(mapper.map(&event(r#"{"type":"message","role":"user","content":"List the files"}"#)).is_empty());

        let delta = |text: &str| json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": text } });
        let first = event(r#"{"type":"message","role":"assistant","content":"I will ","delta":true}"#);
        assert_eq!(
            mapper.map(&first),
            [json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } }), delta("I will ")]
        );
        let more = event(r#"{"type":"message","role":"assistant","content":"look.","delta":true}"#);
        assert_eq!(mapper.map(&more), [delta("look.")]);

        let call = event(
            r#"{"type":"tool_use","tool_name":"list_directory","tool_id":"ls-1","parameters":{"path":"/workspace"}}"#,
        );
        let records = mapper.map(&call);
        assert_eq!(records[0]["message"]["content"], json!([{ "type": "text", "text": "I will look." }]));
        assert_eq!(
            records[1]["message"]["content"],
            json!([{ "type": "toolCall", "id": "ls-1", "name": "list_directory", "arguments": { "path": "/workspace" } }])
        );
        assert_eq!(records[2], json!({ "type": "tool_execution_start", "toolCallId": "ls-1" }));

        let result = event(r#"{"type":"tool_result","tool_id":"ls-1","status":"success","output":"a\nb"}"#);
        let content = json!([{ "type": "text", "text": "a\nb" }]);
        assert_eq!(
            mapper.map(&result),
            [
                json!({ "type": "tool_execution_end", "toolCallId": "ls-1", "isError": false, "result": { "content": content } }),
                json!({ "type": "message_end", "message": { "role": "toolResult", "toolCallId": "ls-1", "content": content, "isError": false } }),
            ]
        );
        let failed =
            event(r#"{"type":"tool_result","tool_id":"w-1","status":"error","error":{"type":"x","message":"EROFS"}}"#);
        let records = mapper.map(&failed);
        assert_eq!(
            (records[0]["isError"].clone(), records[0]["result"]["content"][0]["text"].clone()),
            (json!(true), json!("EROFS"))
        );

        mapper.map(&event(r#"{"type":"message","role":"assistant","content":"Two files.","delta":true}"#));
        let done = event(r#"{"type":"result","status":"success","stats":{"total_tokens":10}}"#);
        let records = mapper.map(&done);
        assert_eq!(records[0]["message"]["content"], json!([{ "type": "text", "text": "Two files." }]));
        assert_eq!(records[1], json!({ "type": "agent_settled" }));
    }

    #[test]
    fn maps_errors_and_passes_unknown_events_through() {
        let mut mapper = Mapper::default();
        let warning = event(r#"{"type":"error","severity":"warning","message":"Loop detected"}"#);
        assert_eq!(mapper.map(&warning), [json!({ "type": "status", "message": "Loop detected" })]);
        let failed = event(r#"{"type":"result","status":"error","error":{"type":"auth","message":"401"}}"#);
        assert_eq!(
            mapper.map(&failed),
            [
                json!({ "type": "message_end", "message": { "role": "assistant", "content": [], "stopReason": "error", "errorMessage": "401" } }),
                json!({ "type": "agent_settled" }),
            ]
        );
        let future = event(r#"{"type":"compressed"}"#);
        assert_eq!(mapper.map(&future), std::slice::from_ref(&future));
    }
}
