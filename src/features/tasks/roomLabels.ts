import { AGENT_NAMES } from "../../shared/agents.ts";
import type { RoomParticipant } from "./api.ts";

/** The provider kind and the model of a participant, for the user. An older run has no model details. */
export function participantLabel(p: RoomParticipant) {
  const name = p.kind ? AGENT_NAMES[p.kind] : p.id;
  return p.modelRecorded ? `${name} · ${p.model ?? "default model"}` : name;
}
