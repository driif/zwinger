import { listen } from "@tauri-apps/api/event";
import { call } from "../../shared/api";

export type CheckLevel = "pass" | "warn" | "fail";

/** A system check of the app start, with what to do if it does not pass. */
export interface Check {
  name: string;
  level: CheckLevel;
  message: string;
}

/** The steps of the runtime start, in order. */
export type StartupStep =
  "resolve" | "reapLegacy" | "install" | "reap" | "ready";

/** The project commands work only in the `ready` phase. `blocked`: a check failed. */
export interface Startup {
  phase: "checking" | "installing" | "ready" | "blocked";
  checks: Check[];
  step: StartupStep | null;
  /** When `step` started, in milliseconds since the Unix epoch. */
  startedAt: number;
}

export const startupStatus = () => call<Startup>("startup_status");

/** Resolves to the function that stops the listener. */
export const onStartup = (handler: (startup: Startup) => void) =>
  listen<Startup>("startup", ({ payload }) => handler(payload));
