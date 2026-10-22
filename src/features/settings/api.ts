import { Channel } from "@tauri-apps/api/core";
import { AgentKind, call, HostAgentUse } from "../../shared/api";

export const setHarness = (id: string, harness: string | null) =>
  call<void>("set_harness", { id, harness });

export const listFolders = (id: string) =>
  call<string[]>("list_folders", { id });

export const addMount = (
  id: string,
  host: string,
  guest: string,
  readonly: boolean,
) => call<void>("add_mount", { id, host, guest, readonly });

export const removeMount = (id: string, guest: string) =>
  call<void>("remove_mount", { id, guest });

export const setMountReadonly = (
  id: string,
  guest: string,
  readonly: boolean,
) => call<void>("set_mount_readonly", { id, guest, readonly });

/** The agents that zwinger downloads on first use. pi is bundled. */
export type RuntimeName = "claude" | "codex";

export interface RuntimeView {
  name: RuntimeName;
  version: string;
  installed: boolean;
}

export type InstallProgress =
  | { stage: "download"; received: number; total: number | null }
  | { stage: "extract" };

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
  return call<void>("install_runtime", {
    name,
    version,
    confirm,
    onProgress: new Channel<InstallProgress>(onProgress),
  });
}

/** Where a shield's copy of a host agent comes from. */
export type RuntimeSource = "host" | "cached" | "pinned";

export type LoginState =
  | { state: "found"; expires: number | null }
  | { state: "expired" }
  | { state: "none" };

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

/** `allowHosts` also adds an Allow rule for the login's provider host. Applies on the next start. */
export const putHostAgent = (
  id: string,
  agent: HostAgentUse,
  allowHosts: boolean,
) => call<void>("put_host_agent", { id, agent, allowHosts });

/** Also removes its host login. */
export const removeHostAgent = (id: string, agent: AgentKind) =>
  call<void>("remove_host_agent", { id, agent });

/** Needs no restart: the running shield gets new agent commands at once. */
export const setAgentsUnprompted = (id: string, on: boolean) =>
  call<void>("set_agents_unprompted", { id, on });

/** Deletes the agent's login that was made in the guest. */
export const removeGuestLogin = (id: string, agent: AgentKind) =>
  call<void>("remove_guest_login", { id, agent });

/** Stops the download. The install then fails with a cancel error. */
export const cancelRuntimeInstall = (name: RuntimeName) =>
  call<void>("cancel_runtime_install", { name });

export interface Prefs {
  /** The user finished or skipped the first-run steps. */
  onboarded: boolean;
  /** Look for an update at each start. */
  autoUpdate: boolean;
  /** The theme id when `followSystem` is off. */
  theme: string;
  /** Use `lightTheme` or `darkTheme` as the system appearance changes. */
  followSystem: boolean;
  lightTheme: string;
  darkTheme: string;
  /** The width of the shield rail, in px. */
  railWidth: number;
  terminal: TerminalPrefs;
}

export type CursorStyle = "block" | "bar" | "underline";

/** The look and keys of all terminals. Sizes in px. */
export interface TerminalPrefs {
  /** A CSS font-family list. */
  fontFamily: string;
  fontSize: number;
  lineHeight: number;
  cursorStyle: CursorStyle;
  cursorBlink: boolean;
  padding: number;
  /** The lines that a terminal keeps above the screen. */
  scrollback: number;
  /** Option sends Meta, so Option+B and Option+F move by word. */
  optionAsMeta: boolean;
  /** The theme of the terminal colors. `null`: the app theme. */
  themeId: string | null;
}

export const getPreferences = () => call<Prefs>("get_preferences");

export const setPreferences = (prefs: Prefs) =>
  call<void>("set_preferences", { prefs });

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
export const removeLegacyRuntime = (legacy: Legacy) =>
  call<void>("remove_legacy_runtime", { legacy });

/** Removes the cached images that no shield uses. Resolves to the freed bytes, if known. */
export const cleanImages = () => call<number | null>("clean_images");
