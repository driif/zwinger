import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type SessionKind = "shell" | "pi";

export interface Mount {
  host: string;
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

export interface SecretView extends Secret {
  placeholder: string;
  /** The Keychain has a value. */
  set: boolean;
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

export interface ShieldView {
  id: string;
  name: string;
  folder: string;
  mounts: Mount[];
  network: NetworkPolicy;
  harness: string | null;
  secrets: Secret[];
  /** The map positions that the user dragged, by node id. */
  layout: Record<string, Pos>;
  apps: App[];
  status: ShieldStatus;
  pendingRestart: boolean;
  sessions: SessionView[];
  appRuns: AppRun[];
}

export type PtyEvent = { event: "output"; data: number[] } | { event: "exit"; data: number };

export const listShields = () => invoke<ShieldView[]>("list_shields");

export const createShield = (folder: string) => invoke<string>("create_shield", { folder });

export const deleteShield = (id: string) => invoke<void>("delete_shield", { id });

/** `ids` must name each shield once. */
export const reorderShields = (ids: string[]) => invoke<void>("reorder_shields", { ids });

/** `null` restores the auto layout. */
export const setLayout = (id: string, layout: Record<string, Pos> | null) => invoke<void>("set_layout", { id, layout });

/** No `id` adds a new app. Resolves to the app id. */
export const putApp = (id: string, app: Omit<App, "id"> & { id?: string }) => invoke<string>("put_app", { id, app });

/** Also stops the app. */
export const removeApp = (id: string, appId: string) => invoke<void>("remove_app", { id, appId });

export const reorderApps = (id: string, appIds: string[]) => invoke<void>("reorder_apps", { id, appIds });

/** Resolves to the session id of the run. */
export const startApp = (id: string, appId: string) => invoke<string>("start_app", { id, appId });

export const stopApp = (id: string, appId: string) => invoke<void>("stop_app", { id, appId });

export const restartApp = (id: string, appId: string) => invoke<string>("restart_app", { id, appId });

/** Sends the kept output of the run, then new output. A later attach takes over. Resolves to the token for `detachApp`. */
export function attachApp(sessionId: string, onEvent: (event: PtyEvent) => void) {
  return invoke<number>("attach_app", { sessionId, onEvent: new Channel<PtyEvent>(onEvent) });
}

/** Stops the output to the attach of `token`. A later attach keeps its output. */
export const detachApp = (sessionId: string, token: number) => invoke<void>("detach_app", { sessionId, token });

/** The canonical folder of a host path: a link resolves to its target. */
export const resolveFolder = (path: string) => invoke<string>("resolve_folder", { path });

export const renameShield = (id: string, name: string) => invoke<void>("rename_shield", { id, name });

export const setHarness = (id: string, harness: string | null) => invoke<void>("set_harness", { id, harness });

export const listFolders = (id: string) => invoke<string[]>("list_folders", { id });

export const addMount = (id: string, host: string, guest: string, readonly: boolean) =>
  invoke<void>("add_mount", { id, host, guest, readonly });

export const removeMount = (id: string, guest: string) =>
  invoke<void>("remove_mount", { id, guest });

export const setMountReadonly = (id: string, guest: string, readonly: boolean) =>
  invoke<void>("set_mount_readonly", { id, guest, readonly });

export const setNetworkMode = (id: string, mode: NetworkMode) => invoke<void>("set_network_mode", { id, mode });

export const addDomain = (id: string, domain: string) => invoke<void>("add_domain", { id, domain });

export const removeDomain = (id: string, domain: string) => invoke<void>("remove_domain", { id, domain });

/** Allows the host in one edit. A shield in Deny mode changes to List mode. */
export const allowHost = (shieldId: string, host: string) => invoke<void>("allow_host", { shieldId, host });

/** The wildcard of the host's registrable domain, as `*.bbc.co.uk` for `news.bbc.co.uk`. `null` if there is none. */
export const wildcardFor = (host: string) => invoke<string | null>("wildcard_for", { host });

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
export const listNetEvents = (shieldId: string) => invoke<NetEvent[]>("list_net_events", { shieldId });

/** Resolves to the function that stops the listener. */
export const onNetEvent = (handler: (shieldId: string, event: NetEvent) => void) =>
  listen<{ shieldId: string; event: NetEvent }>("net-event", ({ payload }) => handler(payload.shieldId, payload.event));

export const listSecrets = (id: string) => invoke<SecretView[]>("list_secrets", { id });

/** `value` null keeps the current value of an existing secret. */
export const putSecret = (id: string, secret: Secret, value: string | null) =>
  invoke<void>("put_secret", { id, secret, value });

export const removeSecret = (id: string, name: string) => invoke<void>("remove_secret", { id, name });

export const startShield = (id: string) => invoke<void>("start_shield", { id });

export const stopShield = (id: string) => invoke<void>("stop_shield", { id });

export function openSession(
  shieldId: string,
  kind: SessionKind,
  rows: number,
  cols: number,
  onEvent: (event: PtyEvent) => void,
) {
  const channel = new Channel<PtyEvent>(onEvent);
  return invoke<string>("open_session", { shieldId, kind, rows, cols, onEvent: channel });
}

export const writeSession = (id: string, data: string) =>
  invoke<void>("write_session", { id, data });

export const resizeSession = (id: string, rows: number, cols: number) =>
  invoke<void>("resize_session", { id, rows, cols });

export const closeSession = (id: string) => invoke<void>("close_session", { id });

export type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";

/** The pi RPC commands that the backend accepts. It builds the JSON for pi from these fields. */
export type AgentCommand =
  | { type: "prompt" | "steer" | "follow_up"; message: string }
  | { type: "abort" | "get_state" | "get_messages" | "get_available_models" | "new_session" | "get_session_stats" }
  | { type: "set_model"; provider: string; modelId: string }
  | { type: "set_thinking_level"; level: ThinkingLevel }
  | { type: "switch_session"; chat: string }
  | { type: "set_session_name"; name: string };

/** A pi chat file of a shield. `id` is the file name. */
export interface Chat {
  id: string;
  started: string;
  title: string;
}

/** A pi stdout record, passed through as is, or the backend's `agent_exit`. */
export type AgentRecord = { type: string } & Record<string, unknown>;

/** Opens the shield's agent, or takes over the running one. */
export function openAgent(shieldId: string, onEvent: (record: AgentRecord) => void) {
  const channel = new Channel<AgentRecord>(onEvent);
  return invoke<string>("open_agent", { shieldId, onEvent: channel });
}

/** Resolves to the `data` of pi's response. */
export const agentCall = <T>(id: string, command: AgentCommand) => invoke<T>("agent_call", { id, command });

export const closeAgent = (id: string) => invoke<void>("close_agent", { id });

export const listChats = (shieldId: string) => invoke<Chat[]>("list_chats", { shieldId });

/** Only a listed chat; a link or another file is refused. */
export const deleteChat = (shieldId: string, chat: string) => invoke<void>("delete_chat", { shieldId, chat });
