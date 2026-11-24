import assert from "node:assert/strict";
import { test } from "node:test";
import { participantLabel } from "./roomLabels.ts";

test("an explicit model shows the kind and the model", () => {
  assert.equal(
    participantLabel({ id: "claude-1", kind: "claude", model: "opus", modelRecorded: true, reportedModel: null }),
    "Claude Code · opus",
  );
});

test("a recorded null model is the configured default", () => {
  assert.equal(
    participantLabel({ id: "pi-2", kind: "pi", model: null, modelRecorded: true, reportedModel: null }),
    "pi · default model",
  );
});

test("a legacy run with no recorded model shows only the kind", () => {
  assert.equal(participantLabel({ id: "codex", kind: "codex", model: null, modelRecorded: false, reportedModel: null }), "Codex");
});

test("an external round shows only its name", () => {
  assert.equal(participantLabel({ id: "opus", kind: null, model: null, modelRecorded: false, reportedModel: null }), "opus");
});

test("a default model shows the model that its CLI reported", () => {
  assert.equal(
    participantLabel({ id: "claude", kind: "claude", model: null, modelRecorded: true, reportedModel: "claude-opus-5-5" }),
    "Claude Code · claude-opus-5-5",
  );
  assert.equal(
    participantLabel({ id: "claude", kind: "claude", model: "opus", modelRecorded: true, reportedModel: "claude-opus-5-5" }),
    "Claude Code · opus",
  );
});
