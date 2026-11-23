import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentKind } from "../../shared/agents.ts";
import type { CageView, SessionCage, SessionView } from "../../shared/api.ts";
import type { Detection } from "../settings/api.ts";
import type { Editors, Opened } from "./api.ts";
import { PendingEditorChoices } from "./editorChoices.ts";
import { type EditorCalls, launchActions, launchCommand, macAgent, openEditor, projectEditors, projectLaunchCommand } from "./actions.ts";

const cageView = (state: "running" | "stopped", hostAgents: CageView["hostAgents"] = []) =>
  ({ id: "c", status: { state }, runtimes: ["claude"], hostAgents }) as unknown as CageView;

/** A session whose effective cage is `cage`, and the cage of its VM. */
const target = (cage: CageView | null, vmCage: CageView | null = cage): SessionCage => ({
  session: { effectiveCage: cage && { id: cage.id, inherited: false } } as SessionView,
  cage,
  vmCage,
});

const cage = (state: "running" | "stopped", hostAgents: CageView["hostAgents"] = []) => target(cageView(state, hostAgents));

const detection = (...kinds: AgentKind[]) => ({ agents: kinds.map((kind) => ({ kind })), antigravity: null }) as Detection;

const editors: Editors = {
  editors: [
    { id: "zed", name: "Zed", path: "/Applications/Zed.app", enabled: true, icon: "data:image/png;base64,iVBORw0KGgo=" },
    { id: "cursor", name: "Cursor", path: "/Applications/Cursor.app", enabled: false, icon: null },
    { id: "vscode", name: "VS Code", path: "/Applications/Visual Studio Code.app", enabled: true, icon: null },
  ],
  default: "vscode",
  names: { zed: "Zed", cursor: "Cursor", vscode: "VS Code", nova: "Nova" },
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
  assert.deepEqual(agents, ["agent-gemini", "agent-claude", "agent-codex", "agent-pi", "agent-agy"]);
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
      ["New Antigravity CLI", "Antigravity CLI is not installed on this Mac"],
      ["Antigravity CLI: continue last session", "Antigravity CLI is not installed on this Mac"],
    ],
  );
  // Antigravity CLI is no host agent of a cage, so the detection has it apart.
  const agy = { ...detection(), antigravity: { kind: "agy" } } as Detection;
  const found = launchActions(target(null), agy, undefined).find((a) => a.id === "agent-agy");
  assert.equal(found?.disabled, undefined);
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

test("a project's editor is its default if it is on, else the app default is", () => {
  const session = (editor: string | null) => ({ editor }) as SessionView;
  assert.equal(projectEditors(undefined, session("zed")), undefined);
  assert.equal(projectEditors(editors, session(null))?.default, "vscode");
  assert.equal(projectEditors(editors, session("zed"))?.default, "zed");
  // Off, or not installed.
  assert.equal(projectEditors(editors, session("cursor"))?.default, "vscode");
  assert.equal(projectEditors(editors, session("nova"))?.default, "vscode");
  // A changed app default applies to a project whose editor is not available.
  assert.equal(projectEditors({ ...editors, default: "zed" }, session("nova"))?.default, "zed");
  // No editor is on.
  assert.equal(projectEditors({ editors: [], default: null, names: {} }, session("zed"))?.default, null);
  // The list itself stays as it is.
  assert.equal(projectEditors(editors, session("zed"))?.editors, editors.editors);
});

/**
 * Fake backend calls that record each call. Nothing opens. `opens`: the result of the open, else an error. `saved`: the
 * saved choices after the open, by default the project's editor as the menu showed it and `editors`.
 */
function fakeCalls(opens: Partial<Opened> | Error = {}, saved?: { project: string | null | undefined; editors: Editors }) {
  const log: string[] = [];
  const calls: EditorCalls = {
    open: async (sessionId, editorId, terminalId) => {
      log.push(`open ${sessionId} ${editorId} ${terminalId}`);
      if (opens instanceof Error) throw opens;
      return { editor: "VS Code", folder: "/tmp/p", editorFellBack: false, folderFellBack: false, ...opens };
    },
    setProjectEditor: async (sessionId, editorId) => void log.push(`project ${sessionId} ${editorId}`),
    setAppEditor: async (editorId) => void log.push(`app ${editorId}`),
    saved: async (sessionId) => {
      log.push(`saved ${sessionId}`);
      return saved ?? { project: current, editors };
    },
  };
  let current: string | null | undefined;
  return { calls, log, showing: (editor: string | null) => (current = editor) };
}

const project = (editor: string | null) => ({ id: "p", editor }) as SessionView;

test("each default open asks the backend for no editor and saves no choice", async () => {
  // A project editor that is on, one that is off, one that is not installed, and none.
  for (const editor of ["zed", "cursor", "nova", null]) {
    const { calls, log } = fakeCalls({ editor: editor === "zed" ? "Zed" : "VS Code" });
    const session = project(editor);
    // The toolbar opens the project folder.
    const toolbar = await openEditor(calls, session, editors);
    assert.deepEqual([toolbar.notice, toolbar.stale], [undefined, false], `editor ${editor}`);
    // A pane and ⌘⇧E open the folder of their terminal.
    await openEditor(calls, session, editors, undefined, "t1");
    // The ⌘K default item runs the ⌘⇧E action.
    const view = projectEditors(editors, session);
    const item = launchActions(target(null), undefined, view).find((a) => a.type === "editor" && a.editor.id === view?.default)!;
    projectLaunchCommand(item, "Open", undefined, view, () => log.push("launch"), () => void openEditor(calls, session, editors, undefined, "t1")).onSelect();
    await Promise.resolve();
    assert.deepEqual(log, ["open p null null", "open p null t1", "open p null t1"], `editor ${editor}`);
  }
});

test("an editor from a menu opens first, then it is saved: the project's editor if it has one, else the app default", async () => {
  const run = async (editor: string | null, chosen: string) => {
    const { calls, log, showing } = fakeCalls({ editor: editors.names[chosen] });
    showing(editor);
    const { notice, stale } = await openEditor(calls, project(editor), editors, chosen, "t1");
    assert.deepEqual([notice, stale], [undefined, false]);
    return log;
  };
  assert.deepEqual(await run("zed", "vscode"), ["open p vscode t1", "saved p", "project p vscode"]);
  assert.deepEqual(await run("zed", "zed"), ["open p zed t1"]);
  assert.deepEqual(await run(null, "zed"), ["open p zed t1", "saved p", "app zed"]);
  assert.deepEqual(await run(null, "vscode"), ["open p vscode t1"]);
  // The ⌘K item of an editor that is not the default is a menu choice.
  const view = projectEditors(editors, project(null));
  const zed = launchActions(target(null), undefined, view).find((a) => a.id === "editor-zed")!;
  const launched: string[] = [];
  projectLaunchCommand(zed, "Open", undefined, view, (a) => launched.push(a.id), () => launched.push("default")).onSelect();
  assert.deepEqual(launched, ["editor-zed"]);
});

test("a menu choice that is gone falls back and saves nothing, in the project and in the app", async () => {
  for (const editor of ["zed", null]) {
    const { calls, log } = fakeCalls({ editor: "Zed", editorFellBack: true });
    const { notice } = await openEditor(calls, project(editor), editors, "vscode");
    assert.deepEqual(log, ["open p vscode null"], `editor ${editor}`);
    assert.equal(notice, "VS Code is not available, so Zed opened.");
  }
});

test("a failed open saves nothing", async () => {
  for (const editor of ["zed", null]) {
    const { calls, log } = fakeCalls(new Error("No enabled editor is available."));
    await assert.rejects(openEditor(calls, project(editor), editors, "vscode"), /No enabled editor/);
    assert.deepEqual(log, ["open p vscode null"], `editor ${editor}`);
  }
});

test("a settings change during the open stays: the choice is saved only over the state that the menu showed", async () => {
  const off = (id: string): Editors => ({ ...editors, editors: editors.editors.map((e) => (e.id === id ? { ...e, enabled: false } : e)) });
  const cases: [string | null, { project: string | null | undefined; editors: Editors }][] = [
    // The project's editor was cleared, set to an other editor, or the project is gone.
    ["vscode", { project: null, editors }],
    ["vscode", { project: "cursor", editors }],
    ["vscode", { project: undefined, editors }],
    // A project editor was set, or the app default changed.
    [null, { project: "vscode", editors }],
    [null, { project: null, editors: { ...editors, default: "cursor" } }],
    // The chosen editor was turned off.
    ["vscode", { project: "vscode", editors: off("zed") }],
    [null, { project: null, editors: off("zed") }],
  ];
  for (const [editor, saved] of cases) {
    const { calls, log } = fakeCalls({ editor: "Zed" }, saved);
    assert.deepEqual(await openEditor(calls, project(editor), editors, "zed"), {
      opened: { editor: "Zed", folder: "/tmp/p", editorFellBack: false, folderFellBack: false },
      notice: undefined,
      stale: false,
    });
    assert.deepEqual(log, ["open p zed null", "saved p"], JSON.stringify([editor, saved]));
  }
});

test("a default open of an editor that is not the one shown names the two editors", async () => {
  // The project shows Zed, but Zed is gone and the backend opened the app default.
  const stale = fakeCalls({ editor: "VS Code" });
  assert.deepEqual(await openEditor(stale.calls, project("zed"), editors), {
    opened: { editor: "VS Code", folder: "/tmp/p", editorFellBack: false, folderFellBack: false },
    notice: "The app showed Zed. VS Code opened.",
    stale: true,
  });
  // The project's editor is not installed, so VS Code shows, but it is back and the backend opened it.
  const restored = fakeCalls({ editor: "Nova" });
  const { notice, stale: changed } = await openEditor(restored.calls, project("nova"), editors);
  assert.deepEqual([notice, changed], ["The app showed VS Code. Nova opened.", true]);
  assert.deepEqual([...stale.log, ...restored.log], ["open p null null", "open p null null"]);
});

test("the last named open in each scope wins, in either completion order", () => {
  for (const firstFinishesFirst of [true, false]) {
    const choices = new PendingEditorChoices();
    const first = choices.begin({ id: "a", editor: null }, "vscode");
    const otherProject = choices.begin({ id: "b", editor: "zed" }, "nova");
    const last = choices.begin({ id: "c", editor: null }, "xcode");
    assert.equal(first.current(), false);
    if (firstFinishesFirst) first.finish();
    assert.equal(last.current(), true);
    last.finish();
    if (!firstFinishesFirst) first.finish();
    assert.equal(otherProject.current(), true);
  }
});

test("settings changes cancel only affected pending editor choices", () => {
  const choices = new PendingEditorChoices();
  const app = choices.begin({ id: "a", editor: null }, "xcode");
  const project = choices.begin({ id: "b", editor: "nova" }, "vscode");
  choices.projectChanged("a");
  assert.equal(app.current(), false);
  assert.equal(project.current(), true);
  const nextApp = choices.begin({ id: "a", editor: null }, "xcode");
  choices.preferencesChanged({ default: "zed", enabled: null }, { default: "zed", enabled: ["xcode", "zed"] });
  assert.equal(project.current(), false);
  assert.equal(nextApp.current(), true);
  choices.preferencesChanged({ default: "zed", enabled: null }, { default: "nova", enabled: null });
  assert.equal(nextApp.current(), false);
});
