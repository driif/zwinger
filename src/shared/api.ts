import { invoke, InvokeArgs } from "@tauri-apps/api/core";
import type { AgentKind } from "./agents";
import { isBackendError } from "./errorMessage";

/** The window event of a lost backend. The backend also emits it on a panic. */
export const BACKEND_LOST = "backend-lost";

let lostCalls = 0;

/** Every command goes through here. Three rejections in a row that are not `{ code, message }` mean that the backend is gone. */
export async function call<T>(command: string, args?: InvokeArgs): Promise<T> {
  try {
    const result = await invoke<T>(command, args);
    lostCalls = 0;
    return result;
  } catch (e) {
    lostCalls = isBackendError(e) ? 0 : lostCalls + 1;
    if (lostCalls === 3) window.dispatchEvent(new Event(BACKEND_LOST));
    throw e;
  }
}

/**
 * `host`: the user's login shell on the Mac, unsandboxed, in the shield folder. `shell`: a VM shell in /workspace.
 * `agent`: an agent's own TUI in the harness; `resume` continues the agent's last session.
 */
export type SessionKind =
  { type: "host" } | { type: "shell" } | { type: "agent"; agent: AgentKind; resume: boolean };

/** The split tree of a shield's terminal view. `row` puts `b` right of `a`, `column` below it; `ratio` is the share of `a`. */
export type PaneNode =
  | { type: "split"; direction: "row" | "column"; ratio: number; a: PaneNode; b: PaneNode }
  | { type: "leaf"; terminal: string };

/** The identity of a host folder. `null`: not verified since the update. */
export interface FileId {
  dev: number;
  ino: number;
}

export interface Mount {
  host: string;
  id: FileId | null;
  guest: string;
  readonly: boolean;
}

export type DefaultAction = "block" | "allow";

export type Action = "allow" | "block";

/** A service of the catalog, or a host: `a.io` or `*.a.io`. */
export type Target =
  { kind: "service"; id: string } | { kind: "host"; host: string };

/** The request locations where the proxy puts the value in place of the placeholder. */
export interface Places {
  /** Lowercase header names that get the value. Empty means no header. */
  headerFields: string[];
  query: boolean;
  body: boolean;
}

/** `set` puts the header in each request. `replace` puts the value only where the agent sent the placeholder. */
export type Inject =
  | { mode: "set"; header: string; template: string }
  | { mode: "replace"; places: Places };

/** The value is in the Keychain. The UI can write it but never reads it. The guest gets the placeholder in `$name`. */
export interface Credential {
  name: string;
  inject: Inject;
}

export interface Rule {
  id: string;
  target: Target;
  action: Action;
  /** Only an Allow rule has credentials. */
  credentials?: Credential[];
}

/** Block rules beat Allow rules. A host that no rule matches gets the default. */
export interface NetworkPolicy {
  default: DefaultAction;
  /** Blocks all requests now and keeps the rules. */
  offline: boolean;
  rules: Rule[];
}

/** The login of a host agent, read on this Mac. The user cannot edit it. */
export interface HostLogin {
  name: string;
  hosts: string[];
  origin: { kind: "hostLogin"; agent: AgentKind };
}

/** A phase of a shield start. */
export type StartPhase =
  | { step: "vault" | "prepare" | "proxy" | "boot" | "guest" | "node" | "tui" | "gemini" | "setup" | "trust" | "wrappers" | "apps" }
  | { step: "profile"; agent: AgentKind };

/** The duration of each phase of a start, in the order they ended. Some phases run at the same time. */
export interface StartTimings {
  phases: { phase: StartPhase; ms: number }[];
  totalMs: number;
}

export type ShieldStatus =
  | { state: "stopped" }
  | { state: "starting"; phase: StartPhase }
  | { state: "running" }
  | { state: "stopping" }
  | { state: "error"; message: string };

export interface SessionView {
  id: string;
  kind: SessionKind;
  exitCode: number | null;
  /** Set for a shell or agent terminal. An app run has none. */
  terminalId: string | null;
}

/** A host shell, VM shell or agent terminal of a shield. With no session it is dormant, and it can open again. */
export interface Terminal {
  id: string;
  kind: SessionKind;
}

export interface Pos {
  x: number;
  y: number;
}

/** A saved, named, long-running command of a shield. */
export interface App {
  id: string;
  name: string;
  command: string;
  /** The guest folder where the command starts. */
  cwd: string;
  autostart: boolean;
}

/** An app that runs or has exited. An app with no run is stopped. */
export interface AppRun {
  appId: string;
  sessionId: string;
  exitCode: number | null;
}

export interface TaskAgent {
  kind: AgentKind;
  /** `null` uses the default model of the CLI. */
  model: string | null;
}

/** A request that two agents plan, build and review together. */
export interface Task {
  id: string;
  /** The folder name of its runs. */
  slug: string;
  title: string;
  request: string;
  /** Two agents of different kinds. The first one decides a tiebreak. */
  agents: [TaskAgent, TaskAgent];
  gates: string[];
  workdir: string;
}

/** A host agent of a shield: its runtime, and its read-only profile and host login if set. */
export interface HostAgentUse {
  kind: AgentKind;
  profile: boolean;
  login: boolean;
}

export interface ShieldView {
  id: string;
  name: string;
  folder: string;
  folderId: FileId | null;
  mounts: Mount[];
  network: NetworkPolicy;
  harness: string | null;
  logins: HostLogin[];
  /** The map positions that the user dragged, by node id. */
  layout: Record<string, Pos>;
  apps: App[];
  tasks: Task[];
  /** In tree order. */
  terminals: Terminal[];
  /** `null`: no pane is open. */
  paneLayout: PaneNode | null;
  status: ShieldStatus;
  pendingRestart: boolean;
  sessions: SessionView[];
  appRuns: AppRun[];
  /** The agent runtimes that the running VM has mounted. A host Gemini CLI that loads counts as one. */
  runtimes: ("claude" | "codex" | "gemini")[];
  hostAgents: HostAgentUse[];
  /** The agent commands skip their permission prompts: the shield is the sandbox. */
  agentsUnprompted: boolean;
  /** The earliest expiry of the host logins that the proxy has now, in ms since the epoch. The proxy reads a newer login when it is due. */
  loginExpiresAt: number | null;
  /** What the start did differently for the host agents, such as a fallback. */
  hostNotices: string[];
  /** The agents of the running VM with a login made in the guest: the agent can read its real token. */
  guestLogins: AgentKind[];
  /** The hosts that the shield's agents need, with the proxy's decision on each. */
  agentNeeds: AgentNeed[];
  /** The phase durations of the start of the running VM. */
  startTimings: StartTimings | null;
}

export interface AgentNeed {
  agent: AgentKind;
  /** The catalog service of the host. */
  service: string;
  host: string;
  allowed: boolean;
}

/** The code of the backend's `InvalidOrder` error. */
export const INVALID_ORDER = "InvalidOrder";

export { AGENT_NAMES } from "./agents";
export type { AgentKind } from "./agents";
