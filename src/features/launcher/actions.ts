import type { CommandMenuItem } from "@driif/motiv";
import { AGENT_NAMES, type AgentKind } from "../../shared/agents.ts";
import type { CageView, SessionCage, SessionView, TerminalKind, VmCage } from "../../shared/api.ts";
import { launchCage } from "../cages/sessionCage.ts";
import type { Detection } from "../settings/api.ts";
import type { Editor, Editors, Opened } from "./api.ts";

const AGENT_ORDER: AgentKind[] = ["claude", "codex", "pi", "gemini"];

/**
 * The agents of the cage, host agents first. A running cage has pi and its mounted runtimes. A stopped cage
 * lists the agents that a start can mount; Gemini CLI runs only from this Mac.
 */
export function agentsOf(cage: CageView | VmCage): AgentKind[] {
  const host = cage.hostAgents.map((a) => a.kind);
  const available: AgentKind[] =
    "status" in cage && cage.status.state === "running" ? ["pi", ...cage.runtimes] : ["claude", "codex", "pi", ...host.filter((k) => k === "gemini")];
  return [...host, ...AGENT_ORDER.filter((k) => !host.includes(k))].filter((k) => available.includes(k));
}

/** The editors as `session` sees them: its default is its own editor if that is on, else the app default. */
export function projectEditors(editors: Editors | undefined, session: SessionView): Editors | undefined {
  if (!editors) return undefined;
  const own = editors.editors.find((e) => e.id === session.editor && e.enabled);
  return { ...editors, default: own?.id ?? editors.default };
}

/** The backend calls of an editor open. Tests pass fakes. */
export interface EditorCalls {
  open: (sessionId: string, editorId: string | null, terminalId: string | null) => Promise<Opened>;
  setProjectEditor: (sessionId: string, editorId: string) => Promise<void>;
  /** Sets the app default editor and keeps the other preferences as they are saved now. */
  setAppEditor: (editorId: string) => Promise<void>;
  /** The saved choices now: the project's editor (`undefined` if the project is gone) and the editors. */
  saved: (sessionId: string) => Promise<{ project: string | null | undefined; editors: Editors }>;
}

/** An editor open, the notice about the editor that opened, and true if the shown editors are out of date. */
export interface EditorOpen {
  opened: Opened;
  notice?: string;
  stale: boolean;
}

/**
 * Opens `session` in an editor. No `editorId`: a default open from the toolbar, a pane, the ⌘K default item or ⌘⇧E.
 * The backend then picks the project's editor, else the app default, and no choice is saved. An `editorId` from a menu
 * is a choice: when that editor opened, it becomes the project's editor if the project has one, else the app default.
 * A fallback or a failed open saves nothing. A settings change during the open wins: see `saveChoice`.
 */
export async function openEditor(
  calls: EditorCalls,
  session: SessionView,
  editors: Editors | undefined,
  editorId?: string,
  terminalId?: string,
): Promise<EditorOpen> {
  const opened = await calls.open(session.id, editorId ?? null, terminalId ?? null);
  if (editorId && !opened.editorFellBack) await saveChoice(calls, session, editors, editorId);
  if (editorId) {
    const asked = editors?.names[editorId] ?? "The editor";
    const fellBack = opened.editorFellBack && asked !== opened.editor;
    return { opened, notice: fellBack ? `${asked} is not available, so ${opened.editor} opened.` : undefined, stale: false };
  }
  const shown = projectEditors(editors, session)?.default;
  const expected = shown ? editors?.names[shown] : undefined;
  const stale = opened.editor !== expected;
  return { opened, notice: stale && expected ? `The app showed ${expected}. ${opened.editor} opened.` : undefined, stale };
}

/**
 * Saves `editorId` in the scope that the menu showed. The saved choices are read again after the open: if the
 * project's editor or the app default changed, or `editorId` is now off, the newer settings stay and nothing is saved.
 */
async function saveChoice(calls: EditorCalls, session: SessionView, editors: Editors | undefined, editorId: string) {
  if (editorId === (session.editor ?? editors?.default)) return;
  const now = await calls.saved(session.id);
  if (now.project !== session.editor || !now.editors.editors.some((e) => e.id === editorId && e.enabled)) return;
  if (session.editor) await calls.setProjectEditor(session.id, editorId);
  else if (now.editors.default === editors?.default) await calls.setAppEditor(editorId);
}

/** True if `action` opens the project's default editor in `editors`, the view of `projectEditors`. */
export const isDefaultEditor = (action: LaunchAction, editors: Editors | undefined) =>
  action.type === "editor" && action.editor.id === editors?.default;

/** A thing to start in a cage. Every launcher shows its part of one list. `disabled`: why it cannot start. */
export type LaunchAction = { id: string; label: string; disabled?: string } & (
  | { type: "session"; kind: TerminalKind }
  | { type: "editor"; editor: Editor }
  | { type: "copyPath" }
  | { type: "reveal" }
);

/** Each agent and its last session, in the cage or on this Mac. */
const agentActions = (agent: AgentKind, type: "agent" | "hostAgent", disabled?: string): LaunchAction[] => [
  { id: `agent-${agent}`, label: `New ${AGENT_NAMES[agent]}`, ...(disabled && { disabled }), type: "session", kind: { type, agent, resume: false } },
  {
    id: `resume-${agent}`,
    label: `${AGENT_NAMES[agent]}: continue last session`,
    ...(disabled && { disabled }),
    type: "session",
    kind: { type, agent, resume: true },
  },
];

/** The agents on this Mac. An agent that the detection did not find is disabled; an unknown detection disables none. */
const hostAgentActions = (detection: Detection | undefined) =>
  AGENT_ORDER.flatMap((agent) =>
    agentActions(
      agent,
      "hostAgent",
      detection && !detection.agents.some((a) => a.kind === agent) ? `${AGENT_NAMES[agent]} is not installed on this Mac` : undefined,
    ),
  );

/**
 * The shells, the agents and their last sessions, the editors that are on, then the folder actions. Only the effective
 * cage picks where the agents run: in the cage, or on this Mac with no cage. A VM that still runs after the cage
 * change does not count.
 */
export function launchActions(target: SessionCage, detection: Detection | undefined, editors: Editors | undefined): LaunchAction[] {
  const cage = target.session.effectiveCage ? launchCage(target) : null;
  return [
    { id: "host", label: "New Mac shell", type: "session", kind: { type: "host" } },
    ...(cage ? [{ id: "shell", label: "New VM shell", type: "session", kind: { type: "shell" } } as const] : []),
    ...(!target.session.effectiveCage
      ? hostAgentActions(detection)
      : (cage ? agentsOf(cage) : []).flatMap((agent) => agentActions(agent, "agent"))),
    ...(editors?.editors ?? [])
      .filter((editor) => editor.enabled)
      .map((editor): LaunchAction => ({ id: `editor-${editor.id}`, label: `Open in ${editor.name}`, type: "editor", editor })),
    { id: "copy-path", label: "Copy path", type: "copyPath" },
    { id: "reveal", label: "Reveal in Finder", type: "reveal" },
  ];
}

/**
 * The Mac agent that an open of `kind` starts, else undefined: a terminal in the cage or a Mac shell. With no effective
 * cage, a new agent is the first agent on this Mac, and an agent of the cage (a row that lost its cage) runs or
 * continues on this Mac. A `hostAgent` stays one whatever the UI shows of the cage, so its warning always shows.
 */
export function macAgent(
  kind: TerminalKind | "agent",
  hasCage: boolean,
  detection: Detection | undefined,
): Extract<TerminalKind, { type: "hostAgent" }> | undefined {
  if (kind !== "agent" && kind.type === "hostAgent") return kind;
  if (hasCage || (kind !== "agent" && kind.type !== "agent")) return undefined;
  const agent = kind === "agent" ? detection?.agents[0]?.kind : kind.agent;
  if (!agent) throw new Error("zwinger found no agent on this Mac.");
  return { type: "hostAgent", agent, resume: kind !== "agent" && kind.resume };
}

/**
 * A launch action of the project as a command. The project's default editor opens as ⌘⇧E does, with `onOpenDefault`,
 * so it saves no choice. `editors`: the view of `projectEditors`.
 */
export const projectLaunchCommand = (
  action: LaunchAction,
  label: string,
  shortcut: string | undefined,
  editors: Editors | undefined,
  onLaunch: (action: LaunchAction) => void,
  onOpenDefault: () => void,
): CommandMenuItem => launchCommand(action, label, shortcut, isDefaultEditor(action, editors) ? onOpenDefault : onLaunch);

/** A launch action as a command. The command menu has no disabled item: a disabled action shows why and does nothing. */
export const launchCommand = (
  action: LaunchAction,
  label: string,
  shortcut: string | undefined,
  onLaunch: (action: LaunchAction) => void,
): CommandMenuItem =>
  action.disabled
    ? { id: action.id, label: `${label} (${action.disabled})`, onSelect: () => {} }
    : { id: action.id, label, shortcut, onSelect: () => onLaunch(action) };
