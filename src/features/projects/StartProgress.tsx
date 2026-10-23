import { Progress } from "@driif/motiv";
import { AGENT_NAMES, StartPhase } from "../../shared/api";

// The guest checks run at once, during "guest", so they are no steps of the bar.
const STEPS: StartPhase["step"][] = ["vault", "prepare", "profile", "proxy", "boot", "guest", "wrappers"];

export function phaseText(phase: StartPhase) {
  switch (phase.step) {
    case "vault":
      return "Reading the credentials…";
    case "prepare":
      return "Preparing the agents…";
    case "profile":
      return `Copying ${AGENT_NAMES[phase.agent]} profile…`;
    case "proxy":
      return "Starting the network proxy…";
    case "boot":
      return "Starting VM…";
    case "guest":
      return "Setting up the guest…";
    case "node":
      return "Checking Node…";
    case "tui":
      return "Checking the pi of this Mac…";
    case "gemini":
      return "Checking Gemini CLI…";
    case "setup":
      return "Adding the agent commands to login shells…";
    case "trust":
      return "Trusting the network proxy…";
    case "wrappers":
      return "Writing the agent commands…";
    case "apps":
      return "Starting the apps…";
  }
}

/** The name of a phase in the timings of the last start. */
export function phaseName(phase: StartPhase) {
  return phase.step === "profile" ? `${AGENT_NAMES[phase.agent]} profile` : phase.step;
}

/** The current phase of a cage start, as text with a progress bar. */
export function StartProgress({ phase }: { phase: StartPhase }) {
  const text = phaseText(phase);
  return (
    <span className="start-progress">
      <Progress value={((STEPS.indexOf(phase.step) + 1) / (STEPS.length + 1)) * 100} label={text} />
      <span>{text}</span>
    </span>
  );
}
