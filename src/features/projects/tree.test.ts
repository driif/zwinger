import assert from "node:assert/strict";
import { test } from "node:test";
import type { Workspace } from "../../shared/api.ts";
import { moveSession, stepSession, stepWorkspace } from "./tree.ts";

const tree = (...lists: string[][]): Workspace[] => lists.map((sessions, i) => ({ id: `w${i}`, name: `w${i}`, sessions }));
const order = (workspaces: Workspace[]) => workspaces.map((w) => w.sessions);

test("a session moves one row in its workspace", () => {
  assert.deepEqual(order(stepSession(tree(["a", "b", "c"]), "b", 1)), [["a", "c", "b"]]);
  assert.deepEqual(order(stepSession(tree(["a", "b", "c"]), "b", -1)), [["b", "a", "c"]]);
});

test("over a border a session goes to the start or end of the adjacent workspace", () => {
  assert.deepEqual(order(stepSession(tree(["a", "b"], ["c", "d"]), "b", 1)), [["a"], ["b", "c", "d"]]);
  assert.deepEqual(order(stepSession(tree(["a", "b"], ["c", "d"]), "c", -1)), [["a", "b", "c"], ["d"]]);
});

test("a session goes into an empty workspace and passes no other workspace", () => {
  assert.deepEqual(order(stepSession(tree(["a"], [], ["b"]), "a", 1)), [[], ["a"], ["b"]]);
  assert.deepEqual(order(stepSession(tree(["a"], [], ["b"]), "b", -1)), [["a"], ["b"], []]);
});

test("at the first or last row a session stays", () => {
  const workspaces = tree(["a"], ["b"]);
  assert.equal(stepSession(workspaces, "a", -1), workspaces);
  assert.equal(stepSession(workspaces, "b", 1), workspaces);
  assert.equal(stepSession(workspaces, "x", 1), workspaces);
});

test("a workspace moves one place and stays at the ends", () => {
  const workspaces = tree(["a"], ["b"], ["c"]);
  assert.deepEqual(stepWorkspace(workspaces, "w1", -1).map((w) => w.id), ["w1", "w0", "w2"]);
  assert.deepEqual(stepWorkspace(workspaces, "w1", 1).map((w) => w.id), ["w0", "w2", "w1"]);
  assert.equal(stepWorkspace(workspaces, "w0", -1), workspaces);
  assert.equal(stepWorkspace(workspaces, "w2", 1), workspaces);
});

test("moveSession puts a session before another or at the end", () => {
  assert.deepEqual(order(moveSession(tree(["a", "b"], ["c"]), "a", "w1", "c")), [["b"], ["a", "c"]]);
  assert.deepEqual(order(moveSession(tree(["a", "b"], ["c"]), "a", "w1")), [["b"], ["c", "a"]]);
});
