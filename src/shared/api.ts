import { invoke, type InvokeArgs } from "@tauri-apps/api/core";
import type { AgentKind } from "./agents.ts";
import { isBackendError } from "./errorMessage.ts";

/** The window event of a lost backend. The backend also emits it on a panic. */
export const BACKEND_LOST = "backend-lost";

export const revealMainWindow = (backgroundColor: string) =>
  call<void>("reveal_main_window", { backgroundColor });

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
 * `host`: the user's login shell on the Mac, unsandboxed, in the session folder. `shell`: a VM shell in /workspace.
 * `agent`: an agent's own TUI in the harness; `resume` continues the agent's last session. `hostAgent`: the Mac's
 * install of the agent, unsandboxed, in the session folder of a session with no cage.
 */
export type TerminalKind =
  | { type: "host" }
  | { type: "shell" }
  | { type: "agent"; agent: AgentKind; resume: boolean }
  | { type: "hostAgent"; agent: AgentKind; resume: boolean };

/** The split tree of a session's terminal view. `row` puts `b` right of `a`, `column` below it; `ratio` is the share of `a`. */
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

/** `local`: the value of this cage. `library`: the shared value in Settings > Credential library. */
export type CredentialSource = "local" | "library";

/** The value is in the vault file of the app. The UI can write it but never reads it. The guest gets the placeholder in `$name`. */
export interface Credential {
  name: string;
  inject: Inject;
  /** Absent means `local`. */
  source?: CredentialSource;
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
  /** The host port grant: the guest reaches `127.0.0.1:<port>` of this Mac at `http://host.microsandbox.internal:<port>`. */
  hostPort?: number;
}

/** The login of a host agent, read on this Mac. The user cannot edit it. */
export interface HostLogin {
  name: string;
  hosts: string[];
  origin: { kind: "hostLogin"; agent: AgentKind };
}

/** A phase of a cage start. */
export type StartPhase =
  | { step: "vault" | "prepare" | "proxy" | "boot" | "guest" | "node" | "tui" | "gemini" | "setup" | "trust" | "wrappers" }
  | { step: "profile"; agent: AgentKind };

/** The duration of each phase of a start, in the order they ended. Some phases run at the same time. */
export interface StartTimings {
  phases: { phase: StartPhase; ms: number }[];
  totalMs: number;
}

/** A successful start: when it ended, in ms since the epoch, and its total duration. */
export interface StartRecord {
  at: number;
  totalMs: number;
}

export type CageStatus =
  | { state: "stopped" }
  | { state: "starting"; phase: StartPhase }
  | { state: "running" }
  | { state: "stopping" }
  | { state: "error"; message: string };

export type AgentState = "active" | "blocked" | "done";

/** A live terminal process. */
export interface TerminalView {
  id: string;
  kind: TerminalKind;
  exitCode: number | null;
  /** Set for a shell or agent terminal. */
  terminalId: string | null;
  /** Set only while an agent terminal is live. */
  agent: AgentState | null;
  /** The last known folder of a live process. `null`: exited, or unknown. */
  folder: TerminalFolder | null;
  /** The order of `folder` within its process: a higher one is newer. */
  folderSeq: number;
}

/** A folder of a process and its order: a higher `seq` is newer. */
export interface FolderReport {
  seq: number;
  folder: TerminalFolder | null;
}

/** Where a live terminal process is. */
export interface TerminalFolder {
  /** The current folder: a Mac path, or a guest path if `vm`. */
  path: string;
  vm: boolean;
  /** The Mac folder that Open in editor opens for it. `null`: it is not on the Mac. */
  opens: string | null;
}

/** A host shell, VM shell or agent terminal of a session. With no process it is dormant, and it can open again. */
export interface TerminalRecord {
  id: string;
  kind: TerminalKind;
}

export interface Pos {
  x: number;
  y: number;
}

/** The pan and zoom of a map. */
export interface Viewport {
  x: number;
  y: number;
  zoom: number;
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

/** A host agent of a cage: its runtime, and its read-only profile and host login if set. */
export interface HostAgentUse {
  kind: AgentKind;
  profile: boolean;
  login: boolean;
}

/** A host folder with its terminals, pane layout and tasks. It uses the cage `cage`. */
export interface SessionView {
  id: string;
  name: string;
  folder: string;
  folderId: FileId | null;
  /** The guest folder name of the session, fixed when the session is made. */
  slug: string;
  /** The id of its attached cage. `null`: it inherits the default cage of its workspace. */
  cage: string | null;
  /** The shared VM of its cage, or a VM of the session alone with the key `key` of cage `cage`. */
  vm: { kind: "shared" } | { kind: "own"; key: string; cage: string };
  harness: string | null;
  /** The id of the editor that opens the project. `null`: the app default. */
  editor: string | null;
  /** The project's mark color, `#rrggbb`. `null`: the theme color. */
  accent: string | null;
  /** In tree order. */
  terminals: TerminalRecord[];
  /** `null`: no pane is open. */
  paneLayout: PaneNode | null;
  tasks: Task[];
  /** The map positions that the user dragged, by node id. */
  layout: Record<string, Pos>;
  /** The map pan and zoom that the user chose. `null`: the map fits its nodes. */
  viewport: Viewport | null;
  /** The live terminal processes. */
  processes: TerminalView[];
  /** The hosts that the session's agents need, with the proxy's decision on each. */
  agentNeeds: AgentNeed[];
  /** The cage that the session uses. `inherited`: the default cage of its workspace. */
  effectiveCage: { id: string; inherited: boolean } | null;
  /**
   * The cage whose VM starts or runs for the session, or whose start for the session failed. After a cage change it is
   * the old cage until that VM stops.
   */
  vmCage: string | null;
  /**
   * The key of that VM: the cage id for the shared VM, `<cage id>-<n>` for an own VM. With no such VM, the key of the VM
   * that the session uses next, if it has one. The network events are per VM key.
   */
  vmKey: string | null;
  /** The session's folder in the running VM: `/workspace`, or `/workspace/<slug>` in a shared VM. `null`: not a member. */
  guestFolder: string | null;
  /** The folder where the session's agents start in the running VM. `null`: not a member. */
  guestHarness: string | null;
}

/** The sandbox config of a session and the state of its VMs. */
export interface CageView {
  id: string;
  name: string;
  mounts: Mount[];
  network: NetworkPolicy;
  logins: HostLogin[];
  hostAgents: HostAgentUse[];
  /** The agent commands skip their permission prompts: the cage is the sandbox. */
  agentsUnprompted: boolean;
  /** The CPUs and memory of each VM of the cage. A change applies on the next start. */
  resources: Resources;
  /** `library`: the token comes from the credential library. */
  claudeToken: { stored: boolean; library: boolean; expiresAt: number | null } | null;
  /** The shared VM, also when it is stopped, then the own VMs that start, run, stop or failed. */
  vms: VmView[];
}

/** A VM of a cage: the shared VM of its sessions, or the own VM of one session. */
export interface VmView {
  key: string;
  own: boolean;
  status: CageStatus;
  /** The names of the sessions of the VM that starts, runs or stops. */
  members: string[];
  pendingRestart: boolean;
  /** The agent runtimes that the running VM has mounted. A host Gemini CLI that loads counts as one. */
  runtimes: ("claude" | "codex" | "gemini")[];
  /** The earliest expiry of the host logins that the proxy has now, in ms since the epoch. The proxy reads a newer login when it is due. */
  loginExpiresAt: number | null;
  /** What the start did differently for the host agents, such as a fallback. */
  hostNotices: string[];
  /** The agents of the running VM with a login made in the guest: the agent can read its real token. */
  guestLogins: AgentKind[];
  /** The phase durations of the start of the running VM. */
  startTimings: StartTimings | null;
  /** The last successful starts of the VM, oldest first, also when it is stopped. */
  startHistory: StartRecord[];
  /** The resources that the running VM started with. */
  appliedResources: Resources | null;
}

export interface Resources {
  cpus: number;
  memoryMib: number;
}

/** A cage with the state of one of its VMs. `id` is the cage id, `key` the VM key. */
export type VmCage = CageView & VmView;

/** A session with its cages, as the UI shows them: one row per session. */
export interface SessionCage {
  session: SessionView;
  /** The effective cage: each config edit goes to it. `null`: the session has no cage. */
  cage: CageView | null;
  /**
   * The cage of the VM that starts, runs or stops for the session, or whose start for the session failed: the VM
   * state, Start/Stop, the network test and the network events read it. `null`: no VM for the session. It differs from
   * `cage` after a cage change, until the VM restarts.
   */
  vmCage: VmCage | null;
}

/** A group of sessions in the rail. Each session is in exactly one workspace. */
export interface Workspace {
  id: string;
  name: string;
  /** Session ids, in rail order. */
  sessions: string[];
  /** The cage of each session of the workspace that inherits its cage. */
  defaultCage: string | null;
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

export { AGENT_NAMES } from "./agents.ts";
export type { AgentKind } from "./agents.ts";
