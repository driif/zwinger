import type { CommandMenuItem } from "@driif/motiv";
import { AGENT_NAMES, type AgentKind } from "../../shared/agents.ts";
import type { CageView, SessionCage, TerminalKind, VmCage } from "../../shared/api.ts";
import { launchCage } from "../cages/sessionCage.ts";
import type { Detection } from "../settings/api.ts";
import type { Editor, Editors } from "./api.ts";

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
