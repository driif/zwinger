import { AGENT_NAMES, type AgentKind } from "../../shared/agents.ts";
import type { SessionKind, ProjectView } from "../../shared/api.ts";
import type { Editor, Editors } from "./api.ts";

const AGENT_ORDER: AgentKind[] = ["claude", "codex", "pi", "gemini"];

/**
 * The agents of the project, host agents first. A running cage has pi and its mounted runtimes. A stopped cage
 * lists the agents that a start can mount; Gemini CLI runs only from this Mac.
 */
export function agentsOf(project: ProjectView): AgentKind[] {
  const host = project.hostAgents.map((a) => a.kind);
  const available: AgentKind[] =
    project.status.state === "running" ? ["pi", ...project.runtimes] : ["claude", "codex", "pi", ...host.filter((k) => k === "gemini")];
  return [...host, ...AGENT_ORDER.filter((k) => !host.includes(k))].filter((k) => available.includes(k));
}

/** A thing to start in a cage. Every launcher shows its part of one list. */
export type LaunchAction = { id: string; label: string } & (
  | { type: "session"; kind: SessionKind }
  | { type: "editor"; editor: Editor }
  | { type: "copyPath" }
  | { type: "reveal" }
);

/** The shells, the agents and their last sessions, the editors that are on, then the folder actions. */
export function launchActions(project: ProjectView, editors: Editors | undefined): LaunchAction[] {
  return [
    { id: "host", label: "New Mac shell", type: "session", kind: { type: "host" } },
    { id: "shell", label: "New VM shell", type: "session", kind: { type: "shell" } },
    ...agentsOf(project).flatMap((agent): LaunchAction[] => [
      { id: `agent-${agent}`, label: `New ${AGENT_NAMES[agent]}`, type: "session", kind: { type: "agent", agent, resume: false } },
      {
        id: `resume-${agent}`,
        label: `${AGENT_NAMES[agent]}: continue last session`,
        type: "session",
        kind: { type: "agent", agent, resume: true },
      },
    ]),
    ...(editors?.editors ?? [])
      .filter((editor) => editor.enabled)
      .map((editor): LaunchAction => ({ id: `editor-${editor.id}`, label: `Open in ${editor.name}`, type: "editor", editor })),
    { id: "copy-path", label: "Copy path", type: "copyPath" },
    { id: "reveal", label: "Reveal in Finder", type: "reveal" },
  ];
}
