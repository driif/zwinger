import assert from "node:assert/strict";
import { test } from "node:test";
import { chatReducer, initialChat, textOf } from "./agentChat.ts";
import type { Message } from "./agentChat.ts";

const text = (id: string, value: string) => ({ type: "message_end", message: { id, role: "assistant", content: [{ type: "text", text: value }] } });

test("distinct timestamp-free messages survive and a replay does not", () => {
  const first = chatReducer(initialChat, { type: "record", record: text("line-1", "First") });
  const second = chatReducer(first, { type: "record", record: text("line-2", "Second") });
  const replay = chatReducer(second, { type: "record", record: text("line-1", "First") });
  assert.equal(replay.messages.length, 2);
  assert.equal(textOf(replay.messages[1].content), "Second");
});

test("distinct tool results survive and base merge deduplicates a timestamp", () => {
  const result = (id: string) => ({ type: "message_end", message: { role: "toolResult", toolCallId: id, content: "Done" } });
  const one = chatReducer(initialChat, { type: "record", record: result("a") });
  const two = chatReducer(one, { type: "record", record: result("b") });
  assert.equal(two.messages.length, 2);
  const base: Message = { role: "assistant", content: "Saved", timestamp: 10 };
  const loading = chatReducer(initialChat, { type: "loading", load: 1 });
  const buffered = chatReducer(loading, { type: "record", record: { type: "message_end", message: base } });
  const merged = chatReducer(buffered, { type: "base", load: 1, messages: [base], running: true });
  assert.equal(merged.messages.length, 1);
});

test("textOf ignores malformed content", () => {
  assert.equal(textOf(null as any), "");
  assert.equal(textOf({} as any), "");
  assert.equal(textOf([{ type: "text", text: 4 }] as any), "");
});


test("a long live message stays bounded and keeps updating", () => {
  const delta = (d: string) => ({ type: "message_update", assistantMessageEvent: { type: "text_delta", contentIndex: 0, delta: d } });
  let state = chatReducer(initialChat, { type: "record", record: delta("a".repeat(16384)) });
  state = chatReducer(state, { type: "record", record: delta("END") });
  const live = textOf(state.partial!.content);
  assert.equal(live.length, 16384);
  assert.ok(live.endsWith("END"));
});
