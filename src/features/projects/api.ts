import { listen } from "@tauri-apps/api/event";
import { AgentState, CageView, call, CredentialSource, Pos, SessionView, StartPhase, FolderReport, Viewport, Workspace } from "../../shared/api";

export interface Sessions {
  sessions: SessionView[];
  cages: CageView[];
}

export const listSessions = () => call<Sessions>("list_sessions");

/** No `workspace`: the first workspace. */
export const createSession = (folder: string, workspace?: string) =>
  call<string>("create_session", { folder, workspace });

export const deleteSession = (sessionId: string) => call<void>("delete_session", { sessionId });

export const listWorkspaces = () => call<Workspace[]>("list_workspaces");

/** Adds an empty workspace at the end. Resolves to its id. */
export const createWorkspace = () => call<string>("create_workspace");

/** A blank name reverts to "Workspace". */
export const renameWorkspace = (id: string, name: string) =>
  call<void>("rename_workspace", { id, name });

/** Only an empty workspace that is not the last one. */
export const deleteWorkspace = (id: string) => call<void>("delete_workspace", { id });

/** The workspaces in order, each with its sessions in order. Must name each workspace and each session once. */
export const arrangeWorkspaces = (workspaces: Pick<Workspace, "id" | "sessions">[]) =>
  call<void>("arrange_workspaces", { workspaces });

/** `null` restores the auto layout and clears the viewport. */
export const setLayout = (sessionId: string, layout: Record<string, Pos> | null) =>
  call<void>("set_layout", { sessionId, layout });

/** `null` lets the map fit its nodes. */
export const setViewport = (sessionId: string, viewport: Viewport | null) =>
  call<void>("set_viewport", { sessionId, viewport });

/** The canonical folder of a host path: a link resolves to its target. */
export const resolveFolder = (path: string) =>
  call<string>("resolve_folder", { path });

export const renameSession = (sessionId: string, name: string) =>
  call<void>("rename_session", { sessionId, name });

/** A member session of a running VM. */
export interface Member {
  id: string;
  name: string;
}

/** The `detail` of `RestartToAdd`: the VM of the cage runs for other sessions. */
export interface RestartToAdd {
  cage: string;
  members: Member[];
  /** The running tasks that the restart cancels, as "<task> in <session>". */
  tasks: string[];
  generation: number;
}

/** The `detail` of `StopShared`: the VM `generation` also runs for these other sessions. */
export interface StopShared {
  cage: string;
  members: Member[];
  /** The running tasks that the stop or restart cancels, as "<task> in <session>". */
  tasks: string[];
  generation: number;
}

/** The other members and the VM that the user saw when they confirmed a stop or restart. */
const confirmedOf = (shared: StopShared | null) =>
  shared && { members: shared.members.map((m) => m.id), generation: shared.generation };

/**
 * Starts the VM of the session's effective cage, with the session as its only member. Fails with `RestartToAdd` when
 * that VM runs for other sessions.
 */
export const startCage = (sessionId: string) => call<void>("start_cage", { sessionId });

/** Restarts the VM with the session as one more member. Fails with `StaleRestart` if the VM or its members changed. */
export const restartCageToAdd = (sessionId: string, add: RestartToAdd) =>
  call<void>("restart_cage_to_add", { sessionId, members: add.members.map((m) => m.id), generation: add.generation });

/**
 * Stops the VM that runs for the session. Fails with `StopShared` when it has other members, unless `confirmed` is that
 * error's detail. Fails with `StaleRestart` if the VM or its members changed since.
 */
export const stopCage = (sessionId: string, confirmed: StopShared | null) =>
  call<void>("stop_cage", { sessionId, confirmed: confirmedOf(confirmed) });

/** Restarts the VM that runs for the session, with all its members. `confirmed` works as for `stopCage`. */
export const restartCage = (sessionId: string, confirmed: StopShared | null) =>
  call<void>("restart_cage", { sessionId, confirmed: confirmedOf(confirmed) });

/**
 * Puts the session in a VM of its own, or back in the shared VM of its cage. Fails with `StopShared` when its running VM
 * has other members, unless `confirmed` is that error's detail: the VM then restarts without the session.
 */
export const setSessionVm = (sessionId: string, own: boolean, confirmed: StopShared | null) =>
  call<void>("set_session_vm", { sessionId, own, confirmed: confirmedOf(confirmed) });

/** `null`: the session inherits the default cage of its workspace. */
export const setSessionCage = (sessionId: string, cageId: string | null) =>
  call<void>("set_session_cage", { sessionId, cageId });

export const setWorkspaceCage = (workspaceId: string, cageId: string | null) =>
  call<void>("set_workspace_cage", { workspaceId, cageId });

/** The user confirms the folders that are not verified since the update. */
export const confirmPaths = (sessionId: string) => call<void>("confirm_paths", { sessionId });

/** A `library` token takes no `value`: it uses the library entry CLAUDE_CODE_OAUTH_TOKEN. */
export const setClaudeToken = (
  cageId: string,
  value: string | null,
  source: CredentialSource,
  allow: boolean,
  profile: boolean,
  /** The exact expiry that `claude setup-token` printed, in ms since the epoch. Needs a `value`. */
  expiresAt: number | null = null,
) => call<void>("set_claude_token", { cageId, value, source, allow, profile, expiresAt });

/** Resolves to the function that stops the listener. */
export const onGuestLogin = (handler: (vmKey: string) => void) =>
  listen<string>("guest-login", ({ payload }) => handler(payload));

/** A live agent process changed state or exited. */
export const onAgentState = (handler: (processId: string, state: AgentState | null) => void) =>
  listen<{ processId: string; state: AgentState | null }>("agent-state", ({ payload }) =>
    handler(payload.processId, payload.state),
  );

/** The folder of a live terminal process changed. `folder` null: it is unknown. */
export const onTerminalFolder = (handler: (processId: string, report: FolderReport) => void) =>
  listen<{ processId: string } & FolderReport>("terminal-folder", ({ payload: { processId, seq, folder } }) =>
    handler(processId, { seq, folder }),
  );

/** Each phase change of a VM start. Resolves to the function that stops the listener. */
export const onStartProgress = (handler: (vmKey: string, phase: StartPhase) => void) =>
  listen<{ vmKey: string; phase: StartPhase; elapsedMs: number }>("cage-start-progress", ({ payload }) =>
    handler(payload.vmKey, payload.phase),
  );

/** Each notice comes once, such as the VMs that the app start removed. */
export const takeAppNotices = () => call<string[]>("take_app_notices");

/**
 * Calls `pull` at each `app-notices` signal, and once when the listener is on, so no notice
 * that came before it waits. Resolves to the function that stops the listener.
 */
export const onAppNotices = (pull: () => void) =>
  listen("app-notices", () => pull()).then((unlisten) => {
    pull();
    return unlisten;
  });
