import { AGENT_NAMES } from "../../shared/agents.ts";
import type { RoomParticipant } from "./api.ts";

/**
 * The provider kind and the model of a participant, for the user. A default model shows the model that its CLI
 * reported, if it reported one. An older run has no model details.
 */
export function participantLabel(p: RoomParticipant) {
  const name = p.kind ? AGENT_NAMES[p.kind] : p.id;
  return p.modelRecorded ? `${name} · ${p.model ?? p.reportedModel ?? "default model"}` : name;
}
