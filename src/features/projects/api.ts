import { listen } from "@tauri-apps/api/event";
import { call, Pos, ProjectView, StartPhase, Workspace } from "../../shared/api";

export const listProjects = () => call<ProjectView[]>("list_projects");

/** No `workspace`: the first workspace. */
export const createProject = (folder: string, workspace?: string) =>
  call<string>("create_project", { folder, workspace });

export const deleteProject = (id: string) => call<void>("delete_project", { id });

export const listWorkspaces = () => call<Workspace[]>("list_workspaces");

/** Adds an empty workspace at the end. Resolves to its id. */
export const createWorkspace = () => call<string>("create_workspace");

/** A blank name reverts to "Workspace". */
export const renameWorkspace = (id: string, name: string) =>
  call<void>("rename_workspace", { id, name });

/** Only an empty workspace that is not the last one. */
export const deleteWorkspace = (id: string) => call<void>("delete_workspace", { id });

/** The workspaces in order, each with its sessions in order. Must name each workspace and each project once. */
export const arrangeWorkspaces = (workspaces: Pick<Workspace, "id" | "sessions">[]) =>
  call<void>("arrange_workspaces", { workspaces });

/** `null` restores the auto layout. */
export const setLayout = (id: string, layout: Record<string, Pos> | null) =>
  call<void>("set_layout", { id, layout });

/** The canonical folder of a host path: a link resolves to its target. */
export const resolveFolder = (path: string) =>
  call<string>("resolve_folder", { path });

export const renameProject = (id: string, name: string) =>
  call<void>("rename_project", { id, name });

export const startCage = (id: string) => call<void>("start_cage", { id });

export const stopCage = (id: string) => call<void>("stop_cage", { id });

/** The user confirms the folders that are not verified since the update. */
export const confirmPaths = (id: string) => call<void>("confirm_paths", { id });

/** Resolves to the function that stops the listener. */
export const onGuestLogin = (handler: (projectId: string) => void) =>
  listen<string>("guest-login", ({ payload }) => handler(payload));

/** Each phase change of a cage start. Resolves to the function that stops the listener. */
export const onStartProgress = (handler: (projectId: string, phase: StartPhase) => void) =>
  listen<{ projectId: string; phase: StartPhase; elapsedMs: number }>("cage-start-progress", ({ payload }) =>
    handler(payload.projectId, payload.phase),
  );

/** Each notice comes once, such as the VMs that the app start removed. */
export const takeAppNotices = () => call<string[]>("take_app_notices");
