import assert from "node:assert/strict";
import { test } from "node:test";
import type { CageView, SessionView } from "../../shared/api.ts";
import { mapLayout } from "./mapLayout.ts";

const session = {
  name: "Example",
  folder: "/projects/example",
  guestFolder: "/workspace/example",
  harness: "/workspace/src/tools",
  guestHarness: "/workspace/example/src/tools",
  layout: {},
} as SessionView;
const cage: CageView = {
  id: "dev", name: "Dev", mounts: [], hostAgents: [], logins: [], agentsUnprompted: false,
  network: { default: "block", offline: false, rules: [] }, claudeToken: null, vms: [],
};
const metrics = { rowHeight: 24, space: 4, charWidth: 8 };

test("the map uses shared VM paths and marks the folder that contains the harness", () => {
  const { nodes } = mapLayout(session, cage, null, ["s", "src"], [], [], 0, metrics);
  assert.equal(nodes.find((node) => node.id === "workspace")?.data.label, "/workspace/example");
  const folder = nodes.find((node) => node.id === "folder:src");
  assert.equal(folder?.data.title, "/workspace/example/src, harness /workspace/example/src/tools");
  assert.deepEqual(folder?.data.badges, ["harness"]);
  assert.deepEqual(nodes.find((node) => node.id === "folder:s")?.data.badges, []);
});

test("a dragged folder keeps its saved position inside the boundary", () => {
  const layout = { "folder:src": { x: 13, y: 26 } };
  const { nodes } = mapLayout({ ...session, layout }, cage, null, ["src"], [], [], 0, metrics);
  const folder = nodes.find((node) => node.id === "folder:src");
  assert.equal(folder?.parentId, "boundary");
  assert.deepEqual(folder?.position, { x: 13, y: 26 });
});
