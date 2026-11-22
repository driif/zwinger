import { Channel } from "@tauri-apps/api/core";
import { AgentKind, call, Task } from "../../shared/api";

/** A pi-shaped record of a task run's live agent output. */
export type AgentRecord = { type: string } & Record<string, unknown>;

/** No `id` adds a new task. Resolves to the task id. */
export const putTask = (
  sessionId: string,
  task: Omit<Task, "id" | "slug"> & { id?: string },
) => call<string>("put_task", { sessionId, task });

/** Also removes its runs. */
export const removeTask = (sessionId: string, taskId: string) =>
  call<void>("remove_task", { sessionId, taskId });

/** Resolves to the run name. */
export const startTaskRun = (sessionId: string, taskId: string) =>
  call<string>("start_task_run", { sessionId, taskId });

/** Continues an ended run in a new run. Resolves to the new run name. */
export const resumeTaskRun = (sessionId: string, taskId: string, run: string) =>
  call<string>("resume_task_run", { sessionId, taskId, run });

export type RunControl =
  | { type: "message" | "answer"; text: string }
  | { type: "pause" | "resume" | "cancel" }
  | { type: "pickImplementer"; agent: string };

export const controlTaskRun = (
  sessionId: string,
  taskId: string,
  run: string,
  control: RunControl,
) => call<void>("control_task_run", { sessionId, taskId, run, control });

export type RunState =
  "running" | "paused" | "waiting" | "done" | "failed" | "cancelled";

export const isLive = (state: RunState) =>
  state === "running" || state === "paused" || state === "waiting";

export interface RunSummary {
  taskId: string;
  run: string;
  state: RunState;
  /** The active stage of a live run. */
  stage: string | null;
  ship: boolean | null;
  started: string | null;
  /** The run that this run continues. */
  continues: string | null;
  /** Whether an ended run can continue; null while the run is live. */
  resume:
    | { from: string; label: string; repeats: boolean; recheck: string | null }
    | { unavailable: string }
    | null;
}

/** The runs of the session's tasks, oldest first per task. */
export const listTaskRuns = (sessionId: string) =>
  call<RunSummary[]>("list_task_runs", { sessionId });

/** The change of a run: the commits since its start, the status, then the diff from its start. */
export const runDiff = (sessionId: string, taskId: string, run: string) =>
  call<string>("run_diff", { sessionId, taskId, run });

/** A local A2A share of a run: a tool on this Mac connects with these details. */
export interface RunShare {
  /** The base URL of the A2A HTTP+JSON interface on 127.0.0.1. */
  url: string;
  cardUrl: string;
  /** The A2A task ID of the run. */
  taskId: string;
  contextId: string;
  /** The bearer credential of this share. */
  token: string;
}

/** Starts the share of a run of this app session, or resolves to the share that runs. */
export const shareTaskRun = (sessionId: string, taskId: string, run: string) =>
  call<RunShare>("share_task_run", { sessionId, taskId, run });

export const taskRunShare = (sessionId: string, taskId: string, run: string) =>
  call<RunShare | null>("task_run_share", { sessionId, taskId, run });

/** Ends the share: its credential stops at once. */
export const stopTaskRunShare = (sessionId: string, taskId: string, run: string) =>
  call<void>("stop_task_run_share", { sessionId, taskId, run });

/** True if the folder looks like a duo round, so a drop opens its room. */
export const isRoundFolder = (path: string) =>
  call<boolean>("is_round_folder", { path });

/** The state of an in-app run of this app session. */
export interface RunStatus {
  state: RunState;
  stage: string | null;
  /** A pause that starts after the current stage. */
  pausing: boolean;
  picked: string | null;
  /** The agents that the user can pick: the ones with a plan. */
  pickable: string[];
  /** The implementer is fixed. */
  decided: boolean;
  ship: boolean | null;
}

export type StageState = "done" | "active" | "pending" | "skipped" | "stopped";

export interface RoomStage {
  id: string;
  label: string;
  state: StageState;
  started: string | null;
}

/** One typed message of a round. `body` is Markdown. */
export interface RoomMessage {
  /** Stable: the source of the message in the round. */
  id: string;
  from: string;
  to: string;
  kind: string;
  stage: string;
  at: string | null;
  summary: string;
  body: string;
}

export interface Blocking {
  what: string;
  why: string;
  where: string;
}

/** A participant of a run. `kind` is `null` for a name of an external round. */
export interface RoomParticipant {
  id: string;
  kind: AgentKind | null;
  /** `null` is the configured default model, but only when `modelRecorded` is true. */
  model: string | null;
  /** False for an older run or an external round, which saved no model. */
  modelRecorded: boolean;
}

export interface Room {
  /** The participant ids, in order. */
  agents: string[];
  participants: RoomParticipant[];
  stages: RoomStage[];
  messages: RoomMessage[];
  positions: {
    agent: string;
    plan: string;
    implementer: string | null;
    source: string;
  }[];
  turns: {
    turn: number;
    replies: {
      agent: string;
      agreed: boolean;
      implementer: string | null;
      conceded: string[];
    }[];
    converged: string | null;
  }[];
  disagreements: {
    turn: number;
    about: "implementer" | "agreement";
    detail: string;
    resolved: string | null;
  }[];
  concessions: { turn: number; agent: string; point: string }[];
  implementer: string | null;
  decidedBy: "agreement" | "tiebreak" | "walkover" | "user" | null;
  tiebreak: { implementer: string | null; why: string } | null;
  crossreview: {
    reviewer: string | null;
    verdict: string;
    findings: {
      severity: string;
      file: string;
      line: number | null;
      what: string;
      why: string;
      fix: string;
    }[];
    message: string;
  } | null;
  finals: {
    agent: string;
    ship: boolean;
    blocking: Blocking[];
    residual: string[];
    taskMet: string;
  }[];
  outcome: {
    ship: boolean;
    shipBasis: string;
    blocking: Blocking[];
    backlog: string[];
    stopped: string | null;
    /** The commit at the start of an in-app run. */
    base: string | null;
  } | null;
  degraded: { agent: string | null; stage: string; detail: string }[];
  violations: { agent: string | null; stage: string; detail: string }[];
  gates: { command: string; code: number | null; output: string }[];
  questions: string[];
  ended: "done" | "failed" | "stopped" | "cancelled" | null;
  endedAt: string | null;
}

export type RoomSource =
  | { kind: "run"; sessionId: string; taskId: string; run: string }
  | { kind: "folder"; path: string };

export type RoomEvent =
  | { type: "room"; room: Room; run: RunStatus | null }
  | { type: "live"; agent: string; stage: string; record: AgentRecord };

/** Sends the room now and on each change. Resolves to the token for `unwatchRoom`. */
export const watchRoom = (
  source: RoomSource,
  onEvent: (event: RoomEvent) => void,
) =>
  call<number>("watch_room", {
    source,
    onEvent: new Channel<RoomEvent>(onEvent),
  });

export const unwatchRoom = (token: number) =>
  call<void>("unwatch_room", { token });
