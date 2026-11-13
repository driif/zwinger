import { listen } from "@tauri-apps/api/event";
import { AgentState, CageView, call, Pos, SessionView, StartPhase, Workspace } from "../../shared/api";

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

/** `null` restores the auto layout. */
export const setLayout = (sessionId: string, layout: Record<string, Pos> | null) =>
  call<void>("set_layout", { sessionId, layout });

/** The canonical folder of a host path: a link resolves to its target. */
export const resolveFolder = (path: string) =>
  call<string>("resolve_folder", { path });

export const renameSession = (sessionId: string, name: string) =>
  call<void>("rename_session", { sessionId, name });

/** Starts the VM of the session's effective cage. */
export const startCage = (sessionId: string) => call<void>("start_cage", { sessionId });

/** Stops the VM that runs for the session. */
export const stopCage = (sessionId: string) => call<void>("stop_cage", { sessionId });

/** `null`: the session inherits the default cage of its workspace. */
export const setSessionCage = (sessionId: string, cageId: string | null) =>
  call<void>("set_session_cage", { sessionId, cageId });

export const setWorkspaceCage = (workspaceId: string, cageId: string | null) =>
  call<void>("set_workspace_cage", { workspaceId, cageId });

/** The user confirms the folders that are not verified since the update. */
export const confirmPaths = (sessionId: string) => call<void>("confirm_paths", { sessionId });

export const setClaudeToken = (cageId: string, value: string, allow: boolean, profile: boolean) =>
  call<void>("set_claude_token", { cageId, value, allow, profile });

/** Resolves to the function that stops the listener. */
export const onGuestLogin = (handler: (cageId: string) => void) =>
  listen<string>("guest-login", ({ payload }) => handler(payload));

/** A live agent process changed state or exited. */
export const onAgentState = (handler: (processId: string, state: AgentState | null) => void) =>
  listen<{ processId: string; state: AgentState | null }>("agent-state", ({ payload }) =>
    handler(payload.processId, payload.state),
  );

/** Each phase change of a cage start. Resolves to the function that stops the listener. */
export const onStartProgress = (handler: (cageId: string, phase: StartPhase) => void) =>
  listen<{ cageId: string; phase: StartPhase; elapsedMs: number }>("cage-start-progress", ({ payload }) =>
    handler(payload.cageId, payload.phase),
  );

/** Each notice comes once, such as the VMs that the app start removed. */
export const takeAppNotices = () => call<string[]>("take_app_notices");
