import { call } from "../../shared/api";

export interface UpdateInfo {
  version: string;
  notes: string | null;
}

/** `null`: this is the newest version. */
export const checkUpdate = () => call<UpdateInfo | null>("check_update");

/**
 * Installs the update and restarts. Fails with `CagesRunning` while a cage is live, unless `stopCages`.
 * With `stopCages` it stops them first; if one does not stop, the update applies at the next start.
 */
export const installUpdate = (stopCages: boolean) =>
  call<void>("install_update", { stopCages });
