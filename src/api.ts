import { Channel, invoke } from "@tauri-apps/api/core";

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

export interface ShieldView {
  id: string;
  name: string;
  folder: string;
  mounts: Mount[];
  network: NetworkPolicy;
  harness: string | null;
  secrets: Secret[];
  status: ShieldStatus;
  pendingRestart: boolean;
  sessions: SessionView[];
}

export type PtyEvent = { event: "output"; data: number[] } | { event: "exit"; data: number };

export const listShields = () => invoke<ShieldView[]>("list_shields");

export const createShield = (folder: string) => invoke<string>("create_shield", { folder });

export const deleteShield = (id: string) => invoke<void>("delete_shield", { id });

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
