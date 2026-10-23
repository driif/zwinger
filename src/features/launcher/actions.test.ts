import assert from "node:assert/strict";
import { test } from "node:test";
import type { ProjectView } from "../../shared/api.ts";
import type { Editors } from "./api.ts";
import { launchActions } from "./actions.ts";

const project = (state: "running" | "stopped", hostAgents: ProjectView["hostAgents"] = []) =>
  ({ status: { state }, runtimes: ["claude"], hostAgents }) as unknown as ProjectView;

const editors: Editors = {
  editors: [
    { id: "zed", name: "Zed", path: "/Applications/Zed.app", enabled: true },
    { id: "cursor", name: "Cursor", path: "/Applications/Cursor.app", enabled: false },
    { id: "vscode", name: "VS Code", path: "/Applications/Visual Studio Code.app", enabled: true },
  ],
  default: "vscode",
};

test("the list has the shells, each agent and its last session, the editors that are on, and the folder actions", () => {
  const actions = launchActions(project("running"), editors);
  assert.deepEqual(
    actions.map((a) => a.label),
    [
      "New Mac shell",
      "New VM shell",
      "New Claude Code",
      "Claude Code: continue last session",
      "New pi",
      "pi: continue last session",
      "Open in Zed",
      "Open in VS Code",
      "Copy path",
      "Reveal in Finder",
    ],
  );
  assert.deepEqual(actions[3], {
    id: "resume-claude",
    label: "Claude Code: continue last session",
    type: "session",
    kind: { type: "agent", agent: "claude", resume: true },
  });
  assert.equal(new Set(actions.map((a) => a.id)).size, actions.length);
});

test("a stopped cage lists the agents that a start can mount, host agents first", () => {
  const host = [{ kind: "gemini" as const, profile: false, login: false }];
  const agents = launchActions(project("stopped", host), undefined)
    .filter((a) => a.type === "session" && a.kind.type === "agent" && !a.kind.resume)
    .map((a) => a.id);
  assert.deepEqual(agents, ["agent-gemini", "agent-claude", "agent-codex", "agent-pi"]);
});

test("no editors, or all editors off, give no editor actions", () => {
  for (const list of [undefined, { editors: editors.editors.map((e) => ({ ...e, enabled: false })), default: null }])
    assert.equal(launchActions(project("running"), list).filter((a) => a.type === "editor").length, 0);
});
