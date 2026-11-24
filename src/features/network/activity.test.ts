import assert from "node:assert/strict";
import { test } from "node:test";
import { groupActivity } from "./activity.ts";
import type { NetEvent } from "./api.ts";

const event = (seq: number, host: string, allowed: boolean, port?: number): NetEvent => ({
  seq,
  at: seq,
  method: "GET",
  host,
  port,
  path: "/",
  status: 200,
  up: 0,
  down: 0,
  ms: 1,
  allowed,
  reason: "default",
  credentials: [],
} as NetEvent);

const events = [
  event(1, "api.github.com", true),
  event(2, "registry.npmjs.org", false),
  event(3, "api.github.com", false),
  event(4, "host.microsandbox.internal", true, 11434),
];

test("an empty search keeps every host, newest first", () => {
  assert.deepEqual(groupActivity(events, "all", " ").map((g) => g.key), [
    "host.microsandbox.internal:11434",
    "api.github.com",
    "registry.npmjs.org",
  ]);
});

test("the search matches part of the host in any case", () => {
  const groups = groupActivity(events, "all", " GitHub ");
  assert.deepEqual(groups.map((g) => [g.key, g.count]), [["api.github.com", 2]]);
});

test("the search matches the port of the gateway name", () => {
  assert.deepEqual(groupActivity(events, "all", ":11434").map((g) => g.key), ["host.microsandbox.internal:11434"]);
});

test("the search and the filter apply together", () => {
  assert.deepEqual(groupActivity(events, "blocked", "github").map((g) => [g.key, g.count]), [["api.github.com", 1]]);
  assert.deepEqual(groupActivity(events, "allowed", "npm"), []);
});
