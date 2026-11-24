import { Badge } from "@driif/motiv";
import { Box, CircleAlert, CircleCheck, CirclePlay, Laptop, ShieldOff } from "lucide-react";
import { AGENT_NAMES, AgentState, TerminalKind } from "../../shared/api";
import type { Pane } from "../../App";
import { AGENT_ICONS } from "../tasks/roomParts";

const kindName = (kind: TerminalKind) =>
  kind.type === "host"
    ? "Mac shell"
    : kind.type === "shell"
      ? "VM shell"
      : kind.type === "hostAgent"
        ? `${AGENT_NAMES[kind.agent]} (no cage)`
        : AGENT_NAMES[kind.agent];

const NO_CAGE = "No cage: runs on this Mac";

export function AgentStatusGlyph({ state }: { state: AgentState | null }) {
  if (!state) return null;
  const label = {
    blocked: "Agent blocked: waiting for you",
    active: "Agent active: working",
    done: "Agent done: waiting for a new prompt",
  }[state];
  const Icon = { blocked: CircleAlert, active: CirclePlay, done: CircleCheck }[state];
  const tone = ({ blocked: "warning", active: "success", done: "neutral" } as const)[state];
  return (
    <Badge tone={tone} size="sm" role="img" aria-label={label} title={label} className="agent-status">
      <Icon size={12} aria-hidden="true" />
    </Badge>
  );
}

/** Where a terminal runs, from its kind: on this Mac, in the cage, or an agent on this Mac with no cage. */
export function KindIcon({ kind }: { kind: TerminalKind }) {
  if (kind.type === "hostAgent")
    return (
      <span role="img" aria-label={NO_CAGE} title={NO_CAGE} className="warn">
        <ShieldOff size={14} aria-hidden="true" />
      </span>
    );
  if (kind.type === "agent") {
    const Icon = AGENT_ICONS[kind.agent];
    return <Icon size={14} aria-label="In the VM" />;
  }
  return kind.type === "host" ? <Laptop size={14} aria-label="On this Mac" /> : <Box size={14} aria-label="In the VM" />;
}

/** The name of a terminal: its kind, and its number among the session's terminals of that kind. */
export function paneName(pane: Pane, panes: Pane[]) {
  const same = panes.filter((p) => p.sessionId === pane.sessionId && kindName(p.kind) === kindName(pane.kind));
  return `${kindName(pane.kind)} ${same.indexOf(pane) + 1}`;
}
