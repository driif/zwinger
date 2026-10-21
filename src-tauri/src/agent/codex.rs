//! Codex: one `codex exec --json` per turn; a later turn resumes the thread.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

/// The installed runtime, mounted read-only.
pub const DIR: &str = "/opt/zwinger/codex";
/// Its `CODEX_HOME`, so `~/.codex` is in the shield's state and never the
/// host's. Codex refuses to make its helper binaries below `/tmp`.
const HOME: &str = "/opt/zwinger/state/codex";

/// `thread` resumes an earlier chat. The prompt comes on stdin (`-`), so
/// its size has no argv limit.
pub fn argv(thread: Option<&str>, model: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "env".to_owned(),
        format!("CODEX_HOME={HOME}"),
        format!("{DIR}/bin/codex"),
        "exec".to_owned(),
    ];
    if let Some(thread) = thread {
        argv.extend(["resume".to_owned(), thread.to_owned()]);
    }
    // `--dangerously-bypass-approvals-and-sandbox` is acceptable only because
    // the agent runs inside the shield: the VM is the sandbox, with only the
    // shield's mounts and allowed hosts. Codex's own sandbox does not work in
    // the VM.
    argv.extend(
        ["--json", "--dangerously-bypass-approvals-and-sandbox", "--skip-git-repo-check"].map(str::to_owned),
    );
    if let Some(model) = model {
        argv.extend(["-m".to_owned(), model.to_owned()]);
    }
    argv.push("-".to_owned());
    argv
}

/// The thread id of the chat, from the `thread.started` event.
pub fn session_id(event: &Value) -> Option<&str> {
    (event["type"] == "thread.started").then(|| event["thread_id"].as_str()).flatten()
}

/// The tool name and arguments of an item that is a tool call.
fn call(item: &Value) -> Option<(String, Value)> {
    Some(match item["type"].as_str()? {
        "command_execution" => ("bash".to_owned(), json!({ "command": item["command"] })),
        "file_change" => ("edit".to_owned(), json!({ "path": paths(item).join(", ") })),
        "mcp_tool_call" => (
            format!("{}.{}", item["server"].as_str().unwrap_or("mcp"), item["tool"].as_str().unwrap_or("tool")),
            if item["arguments"].is_object() { item["arguments"].clone() } else { json!({}) },
        ),
        "web_search" => ("web_search".to_owned(), json!({ "query": item["query"] })),
        _ => return None,
    })
}

fn paths(item: &Value) -> Vec<&str> {
    let changes = item["changes"].as_array().map(Vec::as_slice).unwrap_or_default();
    changes.iter().filter_map(|c| c["path"].as_str()).collect()
}

/// The output text of a tool call item, and if it failed.
fn output(item: &Value) -> (String, bool) {
    let failed = item["status"] == "failed";
    match item["type"].as_str() {
        Some("command_execution") => (
            item["aggregated_output"].as_str().unwrap_or_default().to_owned(),
            failed || item["exit_code"].as_i64().is_some_and(|code| code != 0),
        ),
        Some("file_change") => {
            let changes = item["changes"].as_array().map(Vec::as_slice).unwrap_or_default();
            let lines: Vec<String> = changes
                .iter()
                .map(|c| format!("{} {}", c["kind"].as_str().unwrap_or("change"), c["path"].as_str().unwrap_or("?")))
                .collect();
            (lines.join("\n"), failed)
        }
        Some("mcp_tool_call") => {
            let blocks = item["result"]["content"].as_array().map(Vec::as_slice).unwrap_or_default();
            let text: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
            let error = item["error"]["message"].as_str();
            (error.map_or_else(|| text.join("\n"), str::to_owned), failed || error.is_some())
        }
        _ => (String::new(), failed),
    }
}

fn assistant(content: Value) -> Value {
    json!({ "type": "message_end", "message": { "role": "assistant", "content": [content] } })
}

fn tool_call(id: &Value, name: String, arguments: Value) -> Value {
    assistant(json!({ "type": "toolCall", "id": id, "name": name, "arguments": arguments }))
}

fn update(event: Value) -> Value {
    json!({ "type": "message_update", "assistantMessageEvent": event })
}

/// Maps the events of one Codex run to pi records. It remembers the tool
/// calls that it saw start, so an item that only completes still gets its
/// call, and the streamed text of each message, so a delta has only the new
/// text. The agent sets the message timestamps. An unknown event passes
/// through as it is.
#[derive(Default)]
pub struct Mapper {
    started: HashSet<String>,
    streamed: HashMap<String, String>,
}

impl Mapper {
    pub fn map(&mut self, event: &Value) -> Vec<Value> {
        let item = &event["item"];
        let id = &item["id"];
        match event["type"].as_str() {
            Some("thread.started" | "turn.started") => Vec::new(),
            Some("item.started" | "item.updated") if matches!(item["type"].as_str(), Some("agent_message" | "reasoning")) => {
                self.stream(item)
            }
            Some("item.started") => match call(item) {
                Some((name, arguments)) => {
                    self.started.insert(id.as_str().unwrap_or_default().to_owned());
                    vec![tool_call(id, name, arguments), json!({ "type": "tool_execution_start", "toolCallId": id })]
                }
                None => Vec::new(),
            },
            Some("item.updated") if call(item).is_some() => {
                let (text, _) = output(item);
                vec![json!({
                    "type": "tool_execution_update",
                    "toolCallId": id,
                    "partialResult": { "content": [{ "type": "text", "text": text }] },
                })]
            }
            Some("item.updated") => Vec::new(),
            Some("item.completed") => match item["type"].as_str() {
                // The whole message replaces the streamed one.
                Some("agent_message") => {
                    self.streamed.remove(id.as_str().unwrap_or_default());
                    vec![assistant(json!({ "type": "text", "text": item["text"] }))]
                }
                Some("reasoning") => {
                    self.streamed.remove(id.as_str().unwrap_or_default());
                    vec![assistant(json!({ "type": "thinking", "thinking": item["text"] }))]
                }
                Some("error") => vec![json!({ "type": "status", "message": item["message"] })],
                _ => match call(item) {
                    Some((name, arguments)) => {
                        let mut records = Vec::new();
                        if !self.started.remove(id.as_str().unwrap_or_default()) {
                            records.push(tool_call(id, name, arguments));
                        }
                        let (text, is_error) = output(item);
                        let content = json!([{ "type": "text", "text": text }]);
                        records.push(json!({
                            "type": "tool_execution_end",
                            "toolCallId": id,
                            "isError": is_error,
                            "result": { "content": content },
                        }));
                        records.push(json!({
                            "type": "message_end",
                            "message": { "role": "toolResult", "toolCallId": id, "content": content, "isError": is_error },
                        }));
                        records
                    }
                    None => vec![event.clone()],
                },
            },
            Some("turn.completed") => vec![json!({ "type": "agent_settled" })],
            Some("turn.failed") => vec![
                json!({
                    "type": "message_end",
                    "message": {
                        "role": "assistant",
                        "content": [],
                        "stopReason": "error",
                        "errorMessage": event["error"]["message"].as_str().unwrap_or("Codex failed"),
                    },
                }),
                json!({ "type": "agent_settled" }),
            ],
            // A retry or a warning: the turn goes on.
            Some("error") => vec![json!({ "type": "status", "message": event["message"] })],
            _ => vec![event.clone()],
        }
    }

    /// The item text can be the whole text so far. A delta gets only the
    /// new end; a text that does not go on from the old one replaces it.
    fn stream(&mut self, item: &Value) -> Vec<Value> {
        let kind = if item["type"] == "reasoning" { "thinking" } else { "text" };
        let text = item["text"].as_str().unwrap_or_default();
        let id = item["id"].as_str().unwrap_or_default();
        let mut records = Vec::new();
        let old = match self.streamed.get(id) {
            Some(old) => old.as_str(),
            None => {
                records.push(json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } }));
                ""
            }
        };
        match text.strip_prefix(old) {
            Some("") => {}
            Some(delta) => records.push(update(json!({ "type": format!("{kind}_delta"), "contentIndex": 0, "delta": delta }))),
            None => records.push(update(json!({ "type": format!("{kind}_end"), "contentIndex": 0, "content": text }))),
        }
        self.streamed.insert(id.to_owned(), text.to_owned());
        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn argv_starts_and_resumes() {
        let tail = ["--json", "--dangerously-bypass-approvals-and-sandbox", "--skip-git-repo-check"];
        let head = ["env", "CODEX_HOME=/opt/zwinger/state/codex", "/opt/zwinger/codex/bin/codex", "exec"];
        let mut first: Vec<&str> = head.to_vec();
        first.extend(tail);
        first.push("-");
        assert_eq!(argv(None, None), first);

        let mut resumed: Vec<&str> = head.to_vec();
        resumed.extend(["resume", "01a1"]);
        resumed.extend(tail);
        resumed.extend(["-m", "gpt-5", "-"]);
        assert_eq!(argv(Some("01a1"), Some("gpt-5")), resumed);
    }

    // Hand-written fixtures in the format of `codex exec --json`; the first
    // lines follow docs/reference/agent-cli-samples.txt.
    #[test]
    fn maps_a_turn_with_a_command() {
        let mut mapper = Mapper::default();
        let thread = event(r#"{"type":"thread.started","thread_id":"01a15e33-dab4-72d1-b5e4-ce21c97cd698"}"#);
        assert_eq!(session_id(&thread), Some("01a15e33-dab4-72d1-b5e4-ce21c97cd698"));
        assert!(mapper.map(&thread).is_empty());
        assert!(mapper.map(&event(r#"{"type":"turn.started"}"#)).is_empty());

        let reasoning = event(r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"List the files."}}"#);
        assert_eq!(
            mapper.map(&reasoning),
            [json!({ "type": "message_end", "message": { "role": "assistant", "content": [{ "type": "thinking", "thinking": "List the files." }] } })]
        );

        let started = event(r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#);
        assert_eq!(
            mapper.map(&started),
            [
                json!({
                    "type": "message_end",
                    "message": {
                        "role": "assistant",
                        "content": [{ "type": "toolCall", "id": "item_1", "name": "bash", "arguments": { "command": "bash -lc ls" } }],
                    },
                }),
                json!({ "type": "tool_execution_start", "toolCallId": "item_1" }),
            ]
        );
        let done = event(r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"a\nb\n","exit_code":0,"status":"completed"}}"#);
        let content = json!([{ "type": "text", "text": "a\nb\n" }]);
        assert_eq!(
            mapper.map(&done),
            [
                json!({ "type": "tool_execution_end", "toolCallId": "item_1", "isError": false, "result": { "content": content } }),
                json!({
                    "type": "message_end",
                    "message": { "role": "toolResult", "toolCallId": "item_1", "content": content, "isError": false },
                }),
            ]
        );

        // Seen only at completion: the call comes first.
        let edit = event(r#"{"type":"item.completed","item":{"id":"item_2","type":"file_change","changes":[{"path":"/workspace/a.txt","kind":"add"}],"status":"failed"}}"#);
        let records = mapper.map(&edit);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["message"]["content"][0]["arguments"], json!({ "path": "/workspace/a.txt" }));
        assert_eq!(records[2]["message"]["content"][0]["text"], "add /workspace/a.txt");
        assert_eq!(records[2]["message"]["isError"], true);

        let message = event(r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Two files."}}"#);
        assert_eq!(mapper.map(&message)[0]["message"]["content"], json!([{ "type": "text", "text": "Two files." }]));

        let usage = event(r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5}}"#);
        assert_eq!(mapper.map(&usage), [json!({ "type": "agent_settled" })]);
    }

    // Hand-written: the docs give these shapes, but no run with a real key
    // has shown them yet.
    #[test]
    fn streams_only_the_new_text_of_each_item() {
        let mut mapper = Mapper::default();
        let delta = |kind: &str, text: &str| {
            json!({ "type": "message_update", "assistantMessageEvent": { "type": kind, "contentIndex": 0, "delta": text } })
        };
        let start = json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } });
        let reasoning = event(r#"{"type":"item.started","item":{"id":"item_0","type":"reasoning","text":"Look"}}"#);
        assert_eq!(mapper.map(&reasoning), [start.clone(), delta("thinking_delta", "Look")]);
        let more = event(r#"{"type":"item.updated","item":{"id":"item_0","type":"reasoning","text":"Look first."}}"#);
        assert_eq!(mapper.map(&more), [delta("thinking_delta", " first.")]);
        let done = event(r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"Look first."}}"#);
        assert_eq!(mapper.map(&done)[0]["message"]["content"], json!([{ "type": "thinking", "thinking": "Look first." }]));

        let empty = event(r#"{"type":"item.started","item":{"id":"item_1","type":"agent_message","text":""}}"#);
        assert_eq!(mapper.map(&empty), [start]);
        let part = event(r#"{"type":"item.updated","item":{"id":"item_1","type":"agent_message","text":"Two"}}"#);
        assert_eq!(mapper.map(&part), [delta("text_delta", "Two")]);
        assert!(mapper.map(&part).is_empty());
        // Not a continuation: the whole text replaces the block.
        let changed = event(r#"{"type":"item.updated","item":{"id":"item_1","type":"agent_message","text":"Three files."}}"#);
        assert_eq!(
            mapper.map(&changed),
            [json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_end", "contentIndex": 0, "content": "Three files." } })]
        );
        let tail = event(r#"{"type":"item.updated","item":{"id":"item_1","type":"agent_message","text":"Three files. Done."}}"#);
        assert_eq!(mapper.map(&tail), [delta("text_delta", " Done.")]);
        let message = event(r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"Three files. Done."}}"#);
        assert_eq!(mapper.map(&message)[0]["type"], "message_end");
        assert!(mapper.streamed.is_empty());
    }

    #[test]
    fn maps_errors_and_passes_unknown_events_through() {
        let mut mapper = Mapper::default();
        let retry = event(r#"{"type":"error","message":"Reconnecting... 2/5 (stream disconnected before completion)"}"#);
        assert_eq!(
            mapper.map(&retry),
            [json!({ "type": "status", "message": "Reconnecting... 2/5 (stream disconnected before completion)" })]
        );
        let warning = event(r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Falling back from WebSockets to HTTPS transport."}}"#);
        assert_eq!(mapper.map(&warning)[0]["type"], "status");

        let failed = event(r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#);
        assert_eq!(
            mapper.map(&failed),
            [
                json!({
                    "type": "message_end",
                    "message": { "role": "assistant", "content": [], "stopReason": "error", "errorMessage": "401 Unauthorized" },
                }),
                json!({ "type": "agent_settled" }),
            ]
        );

        let todo = event(r#"{"type":"item.completed","item":{"id":"item_4","type":"todo_list","items":[]}}"#);
        assert_eq!(mapper.map(&todo), std::slice::from_ref(&todo));
        let future = event(r#"{"type":"thread.compacted"}"#);
        assert_eq!(mapper.map(&future), std::slice::from_ref(&future));
    }
}
