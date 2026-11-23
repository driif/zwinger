import assert from "node:assert/strict";
import { test } from "node:test";
import type { CageView, SessionView, VmView } from "../../shared/api.ts";
import { restartPending, sessionCage } from "./sessionCage.ts";

const vm = (key: string, state: VmView["status"]["state"] = "stopped") =>
  ({ key, own: false, status: { state }, members: [] }) as unknown as VmView;
const cage = (id: string, state: VmView["status"]["state"] = "stopped", ...own: VmView[]) =>
  ({ id, vms: [vm(id, state), ...own] }) as unknown as CageView;
const session = (effective: string | null, vmCage: string | null = null, vmKey: string | null = vmCage) =>
  ({ id: "s", effectiveCage: effective && { id: effective, inherited: false }, vmCage, vmKey }) as SessionView;

test("config comes from the effective cage and the VM fields from the cage of the VM", () => {
  const cages = [cage("old", "running"), cage("new", "running")];
  const { cage: config, vmCage } = sessionCage(session("new", "old"), cages);
  assert.equal(config?.id, "new");
  assert.equal(vmCage?.id, "old");
  assert.equal(vmCage?.key, "old");
});

test("a VM that runs for another session is not the session's VM", () => {
  const { cage: config, vmCage } = sessionCage(session("shared", null, "shared"), [cage("shared", "running")]);
  assert.equal(config?.id, "shared");
  assert.equal(vmCage, null);
});

test("an own VM gives its own state", () => {
  const cages = [cage("a", "stopped", { ...vm("a-2", "running"), own: true })];
  const { vmCage } = sessionCage(session("a", "a", "a-2"), cages);
  assert.equal(vmCage?.id, "a");
  assert.equal(vmCage?.key, "a-2");
  assert.equal(vmCage?.status.state, "running");
});

test("a stopped own VM is the session's VM, so its network log and start progress show", () => {
  const cages = [cage("a", "running", { ...vm("a-2"), own: true })];
  const { vmCage } = sessionCage(session("a", null, "a-2"), cages);
  assert.equal(vmCage?.key, "a-2");
  assert.equal(vmCage?.status.state, "stopped");
});

test("a stopped shared VM is the session's VM", () => {
  assert.equal(sessionCage(session("a", null, "a"), [cage("a")]).vmCage?.key, "a");
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
  assert.equal(sessionCage(session("a", null, "a"), cages).vmCage, null);
});

test("a restart is pending after a cage change or a config edit of the running VM, not with no cage", () => {
  const pending = (s: SessionView, cages: CageView[]) => restartPending(sessionCage(s, cages));
  assert.equal(pending(session("new", "old"), [cage("old", "running"), cage("new")]), true);
  assert.equal(pending(session("a", "a"), [cage("a", "running")]), false);
  const edited = cage("a", "running");
  edited.vms[0].pendingRestart = true;
  assert.equal(pending(session("a", "a"), [edited]), true);
  assert.equal(pending(session(null, "old"), [cage("old", "running")]), false);
});
