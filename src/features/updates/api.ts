import { call } from "../../shared/api";

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
export const installUpdate = (stopShields: boolean) =>
  call<void>("install_update", { stopShields });
