import assert from "node:assert/strict";
import { test } from "node:test";
import { mostUrgent } from "./agentState.ts";

test("blocked wins over active and done", () => {
  assert.equal(mostUrgent(["done", "active", "blocked"]), "blocked");
  assert.equal(mostUrgent(["active", "done"]), "active");
  assert.equal(mostUrgent([null, "done"]), "done");
  assert.equal(mostUrgent([null]), null);
  assert.equal(mostUrgent([]), null);
});
