import { Channel, invoke, InvokeArgs } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { isBackendError } from "./errorMessage";

/** The window event of a lost backend. The backend also emits it on a panic. */
export const BACKEND_LOST = "backend-lost";

let lostCalls = 0;

/** Every command goes through here. Three rejections in a row that are not `{ code, message }` mean that the backend is gone. */
async function call<T>(command: string, args?: InvokeArgs): Promise<T> {
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

/** A login shell in /workspace, or an agent's own TUI in the harness. `resume` continues the agent's last session. */
export type SessionKind = { type: "shell" } | { type: "agent"; agent: AgentKind; resume: boolean };

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

export type NetworkMode = "deny" | "list" | "public";

/** The allow list is kept in every mode. */
export interface NetworkPolicy {
  mode: NetworkMode;
  domains: string[];
}

/** The request locations where the proxy puts the value in place of the placeholder. */
export interface Places {
  /** Lowercase header names that get the value. Empty means no header. */
  headerFields: string[];
  query: boolean;
  body: boolean;
}

/** The value is in the Keychain. The UI can write it but never reads it. */
export interface Secret {
  name: string;
  hosts: string[];
  places: Places;
}

/** `hostLogin`: the agent's login on this Mac, read at each start. The user cannot edit it. */
export type SecretOrigin = { kind: "user" } | { kind: "hostLogin"; agent: AgentKind };

export interface SecretView extends Secret {
  placeholder: string;
  /** The Keychain has a value, or the value comes from the Mac. */
  set: boolean;
  origin: SecretOrigin;
}

export type ShieldStatus =
  | { state: "stopped" }
  | { state: "starting" }
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

/** A shell or agent terminal of a shield. With no session it is dormant, and it can open again. */
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
  secrets: (Secret & { origin?: SecretOrigin })[];
  /** The map positions that the user dragged, by node id. */
  layout: Record<string, Pos>;
  apps: App[];
  tasks: Task[];
  /** In tree order. */
  terminals: Terminal[];
  status: ShieldStatus;
  pendingRestart: boolean;
  sessions: SessionView[];
  appRuns: AppRun[];
  /** The agent runtimes that the running VM has mounted. A host Gemini CLI that loads counts as one. */
  runtimes: ("claude" | "codex" | "gemini")[];
  hostAgents: HostAgentUse[];
  /** Restart when this Mac has a newer host login than the VM, if nothing works in the shield. */
  autoRenew: boolean;
  /** The agent commands skip their permission prompts: the shield is the sandbox. */
  agentsUnprompted: boolean;
  /** The earliest expiry of the running VM's host logins, in ms since the epoch. */
  loginExpiresAt: number | null;
  /** A host login is due and this Mac has a newer one: a restart reads it. */
  renewDue: boolean;
  /** What the start did differently for the host agents, such as a fallback. */
  hostNotices: string[];
  /** The agents of the running VM with a login made in the guest: the agent can read its real token. */
  guestLogins: AgentKind[];
}

/** Output bytes, or the exit. Tauri can send small raw bodies as a number array. */
export type SessionFrame = ArrayBuffer | number[] | { exit: number };

/** The code of the backend's `InvalidOrder` error. */
export const INVALID_ORDER = "InvalidOrder";

export const listShields = () => call<ShieldView[]>("list_shields");

export const createShield = (folder: string) => call<string>("create_shield", { folder });

export const deleteShield = (id: string) => call<void>("delete_shield", { id });

/** `ids` must name each shield once. */
export const reorderShields = (ids: string[]) => call<void>("reorder_shields", { ids });

/** `null` restores the auto layout. */
export const setLayout = (id: string, layout: Record<string, Pos> | null) => call<void>("set_layout", { id, layout });

/** No `id` adds a new app. Resolves to the app id. */
export const putApp = (id: string, app: Omit<App, "id"> & { id?: string }) => call<string>("put_app", { id, app });

/** Also stops the app. */
export const removeApp = (id: string, appId: string) => call<void>("remove_app", { id, appId });

export const reorderApps = (id: string, appIds: string[]) => call<void>("reorder_apps", { id, appIds });

/** Resolves to the session id of the run. */
export const startApp = (id: string, appId: string) => call<string>("start_app", { id, appId });

export const stopApp = (id: string, appId: string) => call<void>("stop_app", { id, appId });

export const restartApp = (id: string, appId: string) => call<string>("restart_app", { id, appId });

export const reorderTerminals = (id: string, terminalIds: string[]) =>
  call<void>("reorder_terminals", { id, terminalIds });

/**
 * Sends the kept output of the session, and its exit if it has exited, then new output. A later attach takes over.
 * Resolves to the token for `detachSession` and `ackSession`.
 */
export function attachSession(sessionId: string, onFrame: (frame: SessionFrame) => void) {
  return call<number>("attach_session", { sessionId, onEvent: new Channel<SessionFrame>(onFrame) });
}

/** Stops the output to the attach of `token`. A later attach keeps its output. */
export const detachSession = (sessionId: string, token: number) => call<void>("detach_session", { sessionId, token });

/** The attach of `token` has written one more output frame. The session holds its process while frames wait. */
export const ackSession = (sessionId: string, token: number) => call<void>("ack_session", { sessionId, token });

/** The canonical folder of a host path: a link resolves to its target. */
export const resolveFolder = (path: string) => call<string>("resolve_folder", { path });

export const renameShield = (id: string, name: string) => call<void>("rename_shield", { id, name });

export const setHarness = (id: string, harness: string | null) => call<void>("set_harness", { id, harness });

export const listFolders = (id: string) => call<string[]>("list_folders", { id });

export const addMount = (id: string, host: string, guest: string, readonly: boolean) =>
  call<void>("add_mount", { id, host, guest, readonly });

export const removeMount = (id: string, guest: string) =>
  call<void>("remove_mount", { id, guest });

export const setMountReadonly = (id: string, guest: string, readonly: boolean) =>
  call<void>("set_mount_readonly", { id, guest, readonly });

export const setNetworkMode = (id: string, mode: NetworkMode) => call<void>("set_network_mode", { id, mode });

export const addDomain = (id: string, domain: string) => call<void>("add_domain", { id, domain });

export const removeDomain = (id: string, domain: string) => call<void>("remove_domain", { id, domain });

/** Allows the host in one edit. A shield in Deny mode changes to List mode. */
export const allowHost = (shieldId: string, host: string) => call<void>("allow_host", { shieldId, host });

/** The wildcard of the host's registrable domain, as `*.bbc.co.uk` for `news.bbc.co.uk`. `null` if there is none. */
export const wildcardFor = (host: string) => call<string | null>("wildcard_for", { host });

export type NetKind = "dns_allowed" | "dns_denied" | "tls" | "tls_denied";

/** A network event from the runtime log. `seq` counts per shield; `at` is in ms since the epoch. */
export interface NetEvent {
  seq: number;
  at: number;
  kind: NetKind;
  host: string;
  dst: string | null;
}

export const isBlocked = (event: NetEvent) => event.kind === "dns_denied" || event.kind === "tls_denied";

/** The events that the backend keeps for the shield, oldest first. */
export const listNetEvents = (shieldId: string) => call<NetEvent[]>("list_net_events", { shieldId });

/** Resolves to the function that stops the listener. */
export const onNetEvent = (handler: (shieldId: string, event: NetEvent) => void) =>
  listen<{ shieldId: string; event: NetEvent }>("net-event", ({ payload }) => handler(payload.shieldId, payload.event));

export const listSecrets = (id: string) => call<SecretView[]>("list_secrets", { id });

/** `value` null keeps the current value of an existing secret. */
export const putSecret = (id: string, secret: Secret, value: string | null) =>
  call<void>("put_secret", { id, secret, value });

export const removeSecret = (id: string, name: string) => call<void>("remove_secret", { id, name });

export const startShield = (id: string) => call<void>("start_shield", { id });

export const stopShield = (id: string) => call<void>("stop_shield", { id });

/** Adds a terminal to the end of the shield's list and opens it. Resolves to the session id. */
export const openSession = (shieldId: string, kind: SessionKind, rows: number, cols: number) =>
  call<string>("open_session", { shieldId, kind, rows, cols });

/** Opens a dormant or exited terminal again, under the same id. Resolves to the session id. */
export const reopenTerminal = (shieldId: string, terminalId: string, rows: number, cols: number) =>
  call<string>("reopen_terminal", { shieldId, terminalId, rows, cols });

export const writeSession = (id: string, data: string) =>
  call<void>("write_session", { id, data });

export const resizeSession = (id: string, rows: number, cols: number) =>
  call<void>("resize_session", { id, rows, cols });

/** Removes the terminal and ends its process. */
export const closeTerminal = (shieldId: string, terminalId: string) =>
  call<void>("close_terminal", { shieldId, terminalId });

export type AgentKind = "pi" | "claude" | "codex" | "gemini";

export const AGENT_NAMES: Record<AgentKind, string> = { pi: "pi", claude: "Claude Code", codex: "Codex", gemini: "Gemini CLI" };

/** A pi-shaped record of a task run's live agent output. */
export type AgentRecord = { type: string } & Record<string, unknown>;

/** The agents that zwinger downloads on first use. pi is bundled. */
export type RuntimeName = "claude" | "codex";

export interface RuntimeView {
  name: RuntimeName;
  version: string;
  installed: boolean;
}

export type InstallProgress = { stage: "download"; received: number; total: number | null } | { stage: "extract" };

export const listRuntimes = () => call<RuntimeView[]>("list_runtimes");

/**
 * Downloads `version`, else the pinned version, and checks its integrity. Resolves when it is installed.
 * A version with no pinned hash fails until the user confirms it once with `confirm`.
 */
export function installRuntime(
  name: RuntimeName,
  onProgress: (progress: InstallProgress) => void,
  version: string | null = null,
  confirm = false,
) {
  return call<void>("install_runtime", { name, version, confirm, onProgress: new Channel<InstallProgress>(onProgress) });
}

/** Where a shield's copy of a host agent comes from. */
export type RuntimeSource = "host" | "cached" | "pinned";

/** Gemini CLI authenticates with a `GEMINI_API_KEY` secret only. */
export const GEMINI_NO_LOGIN =
  "Subscription login is not supported yet (needs the app proxy, #16). Add a GEMINI_API_KEY secret to the shield instead.";

export type LoginState = { state: "found"; expires: number | null } | { state: "expired" } | { state: "none" };

/** An agent that is installed on this Mac. */
export interface HostAgent {
  kind: AgentKind;
  version: string;
  /** The host path of the install. */
  path: string;
  source: RuntimeSource;
  /** zwinger pins a hash for the Linux build of this version. Without a pin, the download needs one confirm. */
  pinned: boolean;
  /** The Mac has a config dir that the profile can copy. */
  profile: boolean;
  login: LoginState;
}

export interface Detection {
  agents: HostAgent[];
  /** Antigravity is installed. It is not supported yet. */
  antigravity: boolean;
}

/** The last detection, else a new one. */
export const hostAgents = () => call<Detection>("host_agents");

export const detectHostAgents = () => call<Detection>("detect_host_agents");

/** `allowHosts` also adds the login's provider host to the allow list. Applies on the next start. */
export const putHostAgent = (id: string, agent: HostAgentUse, allowHosts: boolean) =>
  call<void>("put_host_agent", { id, agent, allowHosts });

/** Also removes its host login. */
export const removeHostAgent = (id: string, agent: AgentKind) => call<void>("remove_host_agent", { id, agent });

/** Needs no restart: the running shield gets new agent commands at once. */
export const setAgentsUnprompted = (id: string, on: boolean) => call<void>("set_agents_unprompted", { id, on });

export const setAutoRenew = (id: string, on: boolean) => call<void>("set_auto_renew", { id, on });

/** Resolves to the function that stops the listener. */
export const onRenewDue = (handler: (shieldId: string) => void) =>
  listen<string>("renew-due", ({ payload }) => handler(payload));

/** The user confirms the folders that are not verified since the update. */
export const confirmPaths = (id: string) => call<void>("confirm_paths", { id });

/** Deletes the agent's login that was made in the guest. */
export const removeGuestLogin = (id: string, agent: AgentKind) => call<void>("remove_guest_login", { id, agent });

/** Resolves to the function that stops the listener. */
export const onGuestLogin = (handler: (shieldId: string) => void) =>
  listen<string>("guest-login", ({ payload }) => handler(payload));

/** Each notice comes once, such as the VMs that the app start removed. */
export const takeAppNotices = () => call<string[]>("take_app_notices");

/** Stops the download. The install then fails with a cancel error. */
export const cancelRuntimeInstall = (name: RuntimeName) => call<void>("cancel_runtime_install", { name });

/** No `id` adds a new task. Resolves to the task id. */
export const putTask = (id: string, task: Omit<Task, "id" | "slug"> & { id?: string }) =>
  call<string>("put_task", { id, task });

/** Also removes its runs. */
export const removeTask = (id: string, taskId: string) => call<void>("remove_task", { id, taskId });

/** Resolves to the run name. */
export const startTaskRun = (shieldId: string, taskId: string) => call<string>("start_task_run", { shieldId, taskId });

export type RunControl =
  | { type: "message" | "answer"; text: string }
  | { type: "pause" | "resume" | "cancel" }
  | { type: "pickImplementer"; agent: string };

export const controlTaskRun = (shieldId: string, taskId: string, run: string, control: RunControl) =>
  call<void>("control_task_run", { shieldId, taskId, run, control });

export type RunState = "running" | "paused" | "waiting" | "done" | "failed" | "cancelled";

export const isLive = (state: RunState) => state === "running" || state === "paused" || state === "waiting";

export interface RunSummary {
  taskId: string;
  run: string;
  state: RunState;
  /** The active stage of a live run. */
  stage: string | null;
  ship: boolean | null;
  started: string | null;
}

/** The runs of the shield's tasks, oldest first per task. */
export const listTaskRuns = (shieldId: string) => call<RunSummary[]>("list_task_runs", { shieldId });

/** The change of a run: the commits since its start, the status, then the diff from its start. */
export const runDiff = (shieldId: string, taskId: string, run: string) =>
  call<string>("run_diff", { shieldId, taskId, run });

/** True if the folder looks like a duo round, so a drop opens its room. */
export const isRoundFolder = (path: string) => call<boolean>("is_round_folder", { path });

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

export interface Room {
  agents: string[];
  stages: RoomStage[];
  messages: RoomMessage[];
  positions: { agent: string; plan: string; implementer: string | null; source: string }[];
  turns: { turn: number; replies: { agent: string; agreed: boolean; implementer: string | null; conceded: string[] }[]; converged: string | null }[];
  disagreements: { turn: number; about: "implementer" | "agreement"; detail: string; resolved: string | null }[];
  concessions: { turn: number; agent: string; point: string }[];
  implementer: string | null;
  decidedBy: "agreement" | "tiebreak" | "walkover" | "user" | null;
  tiebreak: { implementer: string | null; why: string } | null;
  crossreview: {
    reviewer: string | null;
    verdict: string;
    findings: { severity: string; file: string; line: number | null; what: string; why: string; fix: string }[];
    message: string;
  } | null;
  finals: { agent: string; ship: boolean; blocking: Blocking[]; residual: string[]; taskMet: string }[];
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

export type RoomSource = { kind: "run"; shieldId: string; taskId: string; run: string } | { kind: "folder"; path: string };

export type RoomEvent =
  | { type: "room"; room: Room; run: RunStatus | null }
  | { type: "live"; agent: string; stage: string; record: AgentRecord };

/** Sends the room now and on each change. Resolves to the token for `unwatchRoom`. */
export const watchRoom = (source: RoomSource, onEvent: (event: RoomEvent) => void) =>
  call<number>("watch_room", { source, onEvent: new Channel<RoomEvent>(onEvent) });

export const unwatchRoom = (token: number) => call<void>("unwatch_room", { token });

export type CheckLevel = "pass" | "warn" | "fail";

/** A system check of the app start, with what to do if it does not pass. */
export interface Check {
  name: string;
  level: CheckLevel;
  message: string;
}

/** The steps of the runtime start, in order. */
export type StartupStep = "resolve" | "reapLegacy" | "install" | "reap" | "ready";

/** The shield commands work only in the `ready` phase. `blocked`: a check failed. */
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

export interface Prefs {
  /** The user finished or skipped the first-run steps. */
  onboarded: boolean;
  /** Look for an update at each start. */
  autoUpdate: boolean;
}

export const getPreferences = () => call<Prefs>("get_preferences");

export const setPreferences = (prefs: Prefs) => call<void>("set_preferences", { prefs });

/** Sizes in bytes. `null`: unknown. */
export interface StorageView {
  appData: string;
  images: number | null;
  runtimes: number | null;
}

export const storage = () => call<StorageView>("storage");

/** A microsandbox home of an earlier zwinger build. `appData`: only zwinger used it. `shared`: `~/.microsandbox`. */
export type Legacy = "appData" | "shared";

/** `size` in bytes, `null`: unknown. */
export interface LegacyView {
  legacy: Legacy;
  path: string;
  size: number | null;
}

export const legacyStorage = () => call<LegacyView[]>("legacy_storage");

/** Removes zwinger's VMs and image. Deletes the `appData` home; keeps the `shared` home and its other data. */
export const removeLegacyRuntime = (legacy: Legacy) => call<void>("remove_legacy_runtime", { legacy });

/** Removes the cached images that no shield uses. Resolves to the freed bytes, if known. */
export const cleanImages = () => call<number | null>("clean_images");

export interface UpdateInfo {
  version: string;
  notes: string | null;
}

/** `null`: this is the newest version. */
export const checkUpdate = () => call<UpdateInfo | null>("check_update");

/**
 * Installs the update and restarts. Fails with `ShieldsRunning` while a shield is live, unless `stopShields`.
 * With `stopShields` it stops them first; if one does not stop, the update applies at the next start.
 */
export const installUpdate = (stopShields: boolean) => call<void>("install_update", { stopShields });
