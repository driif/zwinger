import assert from "node:assert/strict";
import { test } from "node:test";
import type { CageView, SessionView } from "../../shared/api.ts";
import { sessionCage } from "./sessionCage.ts";

const cage = (id: string, state: CageView["status"]["state"] = "stopped") => ({ id, status: { state } }) as CageView;
const session = (effective: string | null, vmCage: string | null = null) =>
  ({ id: "s", effectiveCage: effective && { id: effective, inherited: false }, vmCage }) as SessionView;

test("config comes from the effective cage and the VM fields from the cage of the VM", () => {
  const cages = [cage("old", "running"), cage("new", "running")];
  const { cage: config, vmCage } = sessionCage(session("new", "old"), cages);
  assert.equal(config?.id, "new");
  assert.equal(vmCage?.id, "old");
});

test("a VM that runs for another session is not the session's VM", () => {
  const { cage: config, vmCage } = sessionCage(session("shared"), [cage("shared", "running")]);
  assert.equal(config?.id, "shared");
  assert.equal(vmCage, null);
});

test("both are null when the session has no cage", () => {
  assert.deepEqual(sessionCage(session(null), [cage("a")]), { session: session(null), cage: null, vmCage: null });
});

test("only the VM cage is set when the session has no cage but its VM runs", () => {
  const { cage: config, vmCage } = sessionCage(session(null, "old"), [cage("old", "running")]);
  assert.equal(config, null);
  assert.equal(vmCage?.id, "old");
});

test("a failed start shows only on the session that started it", () => {
  const cages = [cage("a", "error")];
  assert.equal(sessionCage(session("a", "a"), cages).vmCage?.id, "a");
  assert.equal(sessionCage(session("a"), cages).vmCage, null);
});
