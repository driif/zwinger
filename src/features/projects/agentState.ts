import type { AgentState } from "../../shared/api";

export function mostUrgent(states: (AgentState | null)[]): AgentState | null {
  if (states.includes("blocked")) return "blocked";
  if (states.includes("active")) return "active";
  if (states.includes("done")) return "done";
  return null;
}
