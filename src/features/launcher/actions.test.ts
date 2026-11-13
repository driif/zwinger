import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentKind } from "../../shared/agents.ts";
import type { CageView, SessionCage, SessionView } from "../../shared/api.ts";
import type { Detection } from "../settings/api.ts";
import type { Editors } from "./api.ts";
import { launchActions, launchCommand, macAgent } from "./actions.ts";

const cageView = (state: "running" | "stopped", hostAgents: CageView["hostAgents"] = []) =>
  ({ id: "c", status: { state }, runtimes: ["claude"], hostAgents }) as unknown as CageView;

/** A session whose effective cage is `cage`, and the cage of its VM. */
const target = (cage: CageView | null, vmCage: CageView | null = cage): SessionCage => ({
  session: { effectiveCage: cage && { id: cage.id, inherited: false } } as SessionView,
  cage,
  vmCage,
});

const cage = (state: "running" | "stopped", hostAgents: CageView["hostAgents"] = []) => target(cageView(state, hostAgents));

const detection = (...kinds: AgentKind[]) => ({ agents: kinds.map((kind) => ({ kind })), antigravity: false }) as Detection;

const editors: Editors = {
  editors: [
    { id: "zed", name: "Zed", path: "/Applications/Zed.app", enabled: true },
    { id: "cursor", name: "Cursor", path: "/Applications/Cursor.app", enabled: false },
    { id: "vscode", name: "VS Code", path: "/Applications/Visual Studio Code.app", enabled: true },
  ],
  default: "vscode",
};

test("the list has the shells, each agent and its last session, the editors that are on, and the folder actions", () => {
  const actions = launchActions(cage("running"), undefined, editors);
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
  const agents = launchActions(cage("stopped", host), undefined, undefined)
    .filter((a) => a.type === "session" && a.kind.type === "agent" && !a.kind.resume)
    .map((a) => a.id);
  assert.deepEqual(agents, ["agent-gemini", "agent-claude", "agent-codex", "agent-pi"]);
});

test("no editors, or all editors off, give no editor actions", () => {
  for (const list of [undefined, { editors: editors.editors.map((e) => ({ ...e, enabled: false })), default: null }])
    assert.equal(launchActions(cage("running"), undefined, list).filter((a) => a.type === "editor").length, 0);
});

test("with no cage, each agent runs on this Mac, and an agent that is not installed is disabled", () => {
  const sessions = launchActions(target(null), detection("codex", "pi"), undefined).filter((a) => a.type === "session");
  assert.deepEqual(
    sessions.map((a) => [a.label, a.disabled]),
    [
      ["New Mac shell", undefined],
      ["New Claude Code", "Claude Code is not installed on this Mac"],
      ["Claude Code: continue last session", "Claude Code is not installed on this Mac"],
      ["New Codex", undefined],
      ["Codex: continue last session", undefined],
      ["New pi", undefined],
      ["pi: continue last session", undefined],
      ["New Gemini CLI", "Gemini CLI is not installed on this Mac"],
      ["Gemini CLI: continue last session", "Gemini CLI is not installed on this Mac"],
    ],
  );
  assert.deepEqual(sessions[4], {
    id: "resume-codex",
    label: "Codex: continue last session",
    type: "session",
    kind: { type: "hostAgent", agent: "codex", resume: true },
  });
  // An unknown detection disables no agent: the backend checks at the open.
  assert.ok(launchActions(target(null), undefined, undefined).every((a) => !a.disabled));
});

test("only the effective cage routes: a VM that still runs after the cage change gives Mac agents", () => {
  const kinds = (t: SessionCage) =>
    launchActions(t, detection("claude"), undefined).flatMap((a) => (a.type === "session" ? [a.kind.type] : []));
  assert.deepEqual(new Set(kinds(target(null, cageView("running")))), new Set(["host", "hostAgent"]));
  assert.deepEqual(new Set(kinds(cage("stopped"))), new Set(["host", "shell", "agent"]));
});

test("with no cage, continue on a VM agent row continues the agent on this Mac", () => {
  assert.deepEqual(macAgent({ type: "agent", agent: "codex", resume: true }, false, detection("claude")), {
    type: "hostAgent",
    agent: "codex",
    resume: true,
  });
  // With a cage, it stays in the VM.
  assert.equal(macAgent({ type: "agent", agent: "codex", resume: true }, true, detection("claude")), undefined);
});

test("with no cage, a new agent is the first agent on this Mac, and none found is an error", () => {
  assert.deepEqual(macAgent("agent", false, detection("pi", "claude")), { type: "hostAgent", agent: "pi", resume: false });
  assert.throws(() => macAgent("agent", false, detection()), /no agent on this Mac/);
});

test("a Mac agent stays a Mac agent when the UI shows a cage, so its warning shows", () => {
  const kind = { type: "hostAgent", agent: "claude", resume: false } as const;
  assert.equal(macAgent(kind, true, undefined), kind);
  assert.equal(macAgent({ type: "shell" }, false, undefined), undefined);
  assert.equal(macAgent({ type: "host" }, false, undefined), undefined);
});

test("a disabled launch action is a command that shows why and starts nothing", () => {
  const [, claude] = launchActions(target(null), detection("codex"), undefined);
  const launched: string[] = [];
  const command = launchCommand(claude, claude.label, "⌘N", (a) => launched.push(a.id));
  command.onSelect();
  assert.deepEqual([command.label, command.shortcut, launched], ["New Claude Code (Claude Code is not installed on this Mac)", undefined, []]);
  launchCommand(launchActions(target(null), detection("claude"), undefined)[1], "New Claude Code", undefined, (a) => launched.push(a.id)).onSelect();
  assert.deepEqual(launched, ["agent-claude"]);
});
