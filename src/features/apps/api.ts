import { App, call } from "../../shared/api";

/** No `id` adds a new app. Resolves to the app id. */
export const putApp = (id: string, app: Omit<App, "id"> & { id?: string }) =>
  call<string>("put_app", { id, app });

/** Also stops the app. */
export const removeApp = (id: string, appId: string) =>
  call<void>("remove_app", { id, appId });

export const reorderApps = (id: string, appIds: string[]) =>
  call<void>("reorder_apps", { id, appIds });

/** Resolves to the session id of the run. */
export const startApp = (id: string, appId: string) =>
  call<string>("start_app", { id, appId });

export const stopApp = (id: string, appId: string) =>
  call<void>("stop_app", { id, appId });

export const restartApp = (id: string, appId: string) =>
  call<string>("restart_app", { id, appId });
