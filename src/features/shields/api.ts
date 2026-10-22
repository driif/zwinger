import { listen } from "@tauri-apps/api/event";
import { call, Pos, ShieldView, StartPhase } from "../../shared/api";

export const listShields = () => call<ShieldView[]>("list_shields");

export const createShield = (folder: string) =>
  call<string>("create_shield", { folder });

export const deleteShield = (id: string) => call<void>("delete_shield", { id });

/** `ids` must name each shield once. */
export const reorderShields = (ids: string[]) =>
  call<void>("reorder_shields", { ids });

/** `null` restores the auto layout. */
export const setLayout = (id: string, layout: Record<string, Pos> | null) =>
  call<void>("set_layout", { id, layout });

/** The canonical folder of a host path: a link resolves to its target. */
export const resolveFolder = (path: string) =>
  call<string>("resolve_folder", { path });

export const renameShield = (id: string, name: string) =>
  call<void>("rename_shield", { id, name });

export const startShield = (id: string) => call<void>("start_shield", { id });

export const stopShield = (id: string) => call<void>("stop_shield", { id });

/** The user confirms the folders that are not verified since the update. */
export const confirmPaths = (id: string) => call<void>("confirm_paths", { id });

/** Resolves to the function that stops the listener. */
export const onGuestLogin = (handler: (shieldId: string) => void) =>
  listen<string>("guest-login", ({ payload }) => handler(payload));

/** Each phase change of a shield start. Resolves to the function that stops the listener. */
export const onStartProgress = (handler: (shieldId: string, phase: StartPhase) => void) =>
  listen<{ shieldId: string; phase: StartPhase; elapsedMs: number }>("shield-start-progress", ({ payload }) =>
    handler(payload.shieldId, payload.phase),
  );

/** Each notice comes once, such as the VMs that the app start removed. */
export const takeAppNotices = () => call<string[]>("take_app_notices");
