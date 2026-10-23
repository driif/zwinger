//! Claude Code: its paths in the VM, and the mapper of its stream-json output.

use serde_json::{Value, json};

/// The installed runtime, mounted read-only.
pub const DIR: &str = "/opt/zwinger/claude";
/// Its `HOME`, so `~/.claude` is in the project's state and never the host's.
pub const HOME: &str = "/opt/zwinger/state/claude";

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Maps the events of one Claude Code process to pi records. The agent sets
/// the message timestamps. An unknown event passes through as it is.
///
/// Claude Code can send one `assistant` event per content block, and the
/// room ends its streamed message at each one. So a delta's index counts
/// from the first block that no `assistant` event has ended yet.
#[derive(Default)]
pub struct Mapper {
    /// The blocks of the current model message that `assistant` events ended.
    ended: usize,
}

impl Mapper {
    pub fn map(&mut self, event: &Value) -> Vec<Value> {
        match event["type"].as_str() {
            Some("stream_event") => self.stream(event),
            Some("assistant") => {
                if event["parent_tool_use_id"].is_null() {
                    self.ended += event["message"]["content"].as_array().map_or(0, Vec::len);
                }
                map(event)
            }
            _ => map(event),
        }
    }

    fn stream(&mut self, event: &Value) -> Vec<Value> {
        // The stream of a subagent: the room shows only its tool result.
        if !event["parent_tool_use_id"].is_null() {
            return Vec::new();
        }
        let inner = &event["event"];
        match inner["type"].as_str() {
            Some("message_start") => {
                self.ended = 0;
                vec![json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } })]
            }
            Some("content_block_delta") => {
                let index = inner["index"].as_u64().unwrap_or_default() as usize;
                let index = index.saturating_sub(self.ended);
                let delta = &inner["delta"];
                let (kind, text) = match delta["type"].as_str() {
                    Some("text_delta") => ("text_delta", &delta["text"]),
                    Some("thinking_delta") => ("thinking_delta", &delta["thinking"]),
                    // The `assistant` event has the tool input and the signature.
                    Some("input_json_delta" | "signature_delta") => return Vec::new(),
                    _ => return vec![event.clone()],
                };
                vec![json!({
                    "type": "message_update",
                    "assistantMessageEvent": { "type": kind, "contentIndex": index, "delta": text },
                })]
            }
            Some("content_block_start" | "content_block_stop" | "message_delta" | "message_stop" | "ping") => {
                Vec::new()
            }
            _ => vec![event.clone()],
        }
    }
}

/// The pi records for one Claude Code event that is not a stream event.
fn map(event: &Value) -> Vec<Value> {
    match event["type"].as_str() {
        Some("system") if event["subtype"] == "init" => Vec::new(),
        Some("control_response") => Vec::new(),
        Some("assistant") => {
            let blocks = event["message"]["content"].as_array().map(Vec::as_slice).unwrap_or_default();
            let mut started = Vec::new();
            let content: Vec<Value> = blocks
                .iter()
                .filter_map(|block| match block["type"].as_str()? {
                    "text" => Some(json!({ "type": "text", "text": block["text"] })),
                    "thinking" => Some(json!({ "type": "thinking", "thinking": block["thinking"] })),
                    "tool_use" => {
                        started.push(json!({ "type": "tool_execution_start", "toolCallId": block["id"] }));
                        Some(json!({ "type": "toolCall", "id": block["id"], "name": block["name"], "arguments": block["input"] }))
                    }
                    _ => None,
                })
                .collect();
            let mut records =
                vec![json!({ "type": "message_end", "message": { "role": "assistant", "content": content } })];
            records.extend(started);
            records
        }
        Some("user") => {
            let blocks = event["message"]["content"].as_array().map(Vec::as_slice).unwrap_or_default();
            blocks
                .iter()
                .filter(|block| block["type"] == "tool_result")
                .flat_map(|block| {
                    let id = &block["tool_use_id"];
                    let is_error = block["is_error"].as_bool().unwrap_or(false);
                    let content = json!([{ "type": "text", "text": text_of(&block["content"]) }]);
                    [
                        json!({ "type": "tool_execution_end", "toolCallId": id, "isError": is_error, "result": { "content": content } }),
                        json!({
                            "type": "message_end",
                            "message": { "role": "toolResult", "toolCallId": id, "content": content, "isError": is_error },
                        }),
                    ]
                })
                .collect()
        }
        Some("result") => {
            let mut records = Vec::new();
            if event["is_error"].as_bool() == Some(true) {
                let message = match event["result"].as_str() {
                    Some(result) if !result.is_empty() => result.to_owned(),
                    _ => event["subtype"].as_str().unwrap_or("Claude Code failed").to_owned(),
                };
                records.push(json!({
                    "type": "message_end",
                    "message": { "role": "assistant", "content": [], "stopReason": "error", "errorMessage": message },
                }));
            }
            records.push(json!({ "type": "agent_settled" }));
            records
        }
        _ => vec![event.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    // Hand-written fixtures in the stream-json format of Claude Code.
    #[test]
    fn maps_a_turn_with_a_tool_call() {
        let init = event(
            r#"{"type":"system","subtype":"init","session_id":"0b1c","model":"claude-opus","tools":["Bash"],"cwd":"/workspace"}"#,
        );
        assert!(map(&init).is_empty());

        let thinking = event(
            r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"thinking","thinking":"Look first."}]},"session_id":"0b1c"}"#,
        );
        assert_eq!(
            map(&thinking),
            [
                json!({ "type": "message_end", "message": { "role": "assistant", "content": [{ "type": "thinking", "thinking": "Look first." }] } })
            ]
        );

        let tool = event(
            r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]},"session_id":"0b1c"}"#,
        );
        assert_eq!(
            map(&tool),
            [
                json!({
                    "type": "message_end",
                    "message": {
                        "role": "assistant",
                        "content": [{ "type": "toolCall", "id": "toolu_1", "name": "Bash", "arguments": { "command": "ls" } }],
                    },
                }),
                json!({ "type": "tool_execution_start", "toolCallId": "toolu_1" }),
            ]
        );

        let result = event(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"a\nb"}],"is_error":false}]},"session_id":"0b1c"}"#,
        );
        let content = json!([{ "type": "text", "text": "a\nb" }]);
        assert_eq!(
            map(&result),
            [
                json!({ "type": "tool_execution_end", "toolCallId": "toolu_1", "isError": false, "result": { "content": content } }),
                json!({
                    "type": "message_end",
                    "message": { "role": "toolResult", "toolCallId": "toolu_1", "content": content, "isError": false },
                }),
            ]
        );

        let failed = event(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"No such file","is_error":true}]}}"#,
        );
        assert_eq!(map(&failed)[1]["message"]["content"], json!([{ "type": "text", "text": "No such file" }]));
        assert_eq!(map(&failed)[1]["message"]["isError"], true);

        let text = event(
            r#"{"type":"assistant","message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"Two files."}]}}"#,
        );
        assert_eq!(map(&text)[0]["message"]["content"], json!([{ "type": "text", "text": "Two files." }]));

        let done = event(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Two files.","session_id":"0b1c","total_cost_usd":0.01}"#,
        );
        assert_eq!(map(&done), [json!({ "type": "agent_settled" })]);
    }

    /// One line of `--include-partial-messages` output.
    fn stream(inner: &str, parent: &str) -> Value {
        event(&format!(
            r#"{{"type":"stream_event","event":{inner},"parent_tool_use_id":{parent},"session_id":"0b1c"}}"#
        ))
    }

    fn delta(kind: &str, index: usize, text: &str) -> Value {
        json!({ "type": "message_update", "assistantMessageEvent": { "type": kind, "contentIndex": index, "delta": text } })
    }

    // Hand-written: the docs give these shapes, but no run with a real key
    // has shown them yet.
    #[test]
    fn streams_one_block() {
        let mut mapper = Mapper::default();
        let start =
            stream(r#"{"type":"message_start","message":{"id":"msg_1","role":"assistant","content":[]}}"#, "null");
        assert_eq!(
            mapper.map(&start),
            [json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } })]
        );
        assert!(
            mapper
                .map(&stream(
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                    "null"
                ))
                .is_empty()
        );
        for (text, piece) in [("Two", "Two"), (" files.", " files.")] {
            let line = stream(
                &format!(
                    r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#
                ),
                "null",
            );
            assert_eq!(mapper.map(&line), [delta("text_delta", 0, piece)]);
        }
        assert!(mapper.map(&stream(r#"{"type":"content_block_stop","index":0}"#, "null")).is_empty());
        // The whole message ends the streamed one, so its text is shown once.
        let whole = event(
            r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"Two files."}]},"parent_tool_use_id":null}"#,
        );
        assert_eq!(
            mapper.map(&whole),
            [
                json!({ "type": "message_end", "message": { "role": "assistant", "content": [{ "type": "text", "text": "Two files." }] } })
            ]
        );
        assert!(
            mapper
                .map(&stream(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}"#, "null"))
                .is_empty()
        );
        assert!(mapper.map(&stream(r#"{"type":"message_stop"}"#, "null")).is_empty());
    }

    #[test]
    fn streams_several_blocks_from_index_zero_after_each_end() {
        let mut mapper = Mapper::default();
        mapper.map(&stream(r#"{"type":"message_start","message":{"role":"assistant","content":[]}}"#, "null"));
        let thinking = stream(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Look."}}"#,
            "null",
        );
        assert_eq!(mapper.map(&thinking), [delta("thinking_delta", 0, "Look.")]);
        assert!(
            mapper
                .map(&stream(
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}"#,
                    "null"
                ))
                .is_empty()
        );
        mapper.map(&event(r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Look."}]},"parent_tool_use_id":null}"#));
        let text = stream(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"I list."}}"#,
            "null",
        );
        assert_eq!(mapper.map(&text), [delta("text_delta", 0, "I list.")]);
        mapper.map(&event(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I list."}]},"parent_tool_use_id":null}"#));
        let input = stream(
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"[1"}}"#,
            "null",
        );
        assert!(mapper.map(&input).is_empty());
        mapper.map(&event(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]},"parent_tool_use_id":null}"#));

        // A subagent streams while the tool runs: nothing of it is shown, and
        // its messages do not move the count.
        let sub = stream(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"sub"}}"#,
            r#""toolu_1""#,
        );
        assert!(mapper.map(&sub).is_empty());
        assert!(mapper.map(&stream(r#"{"type":"message_start","message":{}}"#, r#""toolu_1""#)).is_empty());
        mapper.map(&event(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"sub"}]},"parent_tool_use_id":"toolu_1"}"#));
        assert_eq!(mapper.ended, 3);

        // The next model message counts from zero again.
        mapper.map(&stream(r#"{"type":"message_start","message":{"role":"assistant","content":[]}}"#, "null"));
        let next =
            stream(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}"#, "null");
        assert_eq!(mapper.map(&next), [delta("text_delta", 0, "Done.")]);
        let unknown = stream(r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta"}}"#, "null");
        assert_eq!(mapper.map(&unknown), std::slice::from_ref(&unknown));
        let future = stream(r#"{"type":"future_event"}"#, "null");
        assert_eq!(mapper.map(&future), std::slice::from_ref(&future));
    }

    #[test]
    fn maps_errors_and_passes_unknown_events_through() {
        let failed = event(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"","session_id":"0b1c"}"#,
        );
        assert_eq!(
            map(&failed),
            [
                json!({
                    "type": "message_end",
                    "message": { "role": "assistant", "content": [], "stopReason": "error", "errorMessage": "error_during_execution" },
                }),
                json!({ "type": "agent_settled" }),
            ]
        );
        let auth = event(r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key"}"#);
        assert_eq!(map(&auth)[0]["message"]["errorMessage"], "Invalid API key");

        assert!(
            map(&event(r#"{"type":"control_response","response":{"subtype":"success","request_id":"zw-1"}}"#))
                .is_empty()
        );
        let unknown = event(r#"{"type":"system","subtype":"compact_boundary","x":1}"#);
        assert_eq!(map(&unknown), std::slice::from_ref(&unknown));
        let future = event(r#"{"type":"rate_limit_event","info":{}}"#);
        assert_eq!(map(&future), std::slice::from_ref(&future));
    }
}
