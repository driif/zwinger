import { getPreferences, updatePreferences, type Prefs, type PrefsPatch } from "./api.ts";

/**
 * Saves patches. Only the reply to the latest save sets the prefs. If the latest save fails, the saved prefs are read
 * again, because an earlier save that succeeded did not set them.
 */
export function prefsWriter(apply: (prefs: Prefs) => void) {
  let latest = 0;
  return async (patch: PrefsPatch) => {
    const write = ++latest;
    try {
      const saved = await updatePreferences(patch);
      if (write === latest) apply(saved);
    } catch (error) {
      if (write === latest) getPreferences().then((saved) => write === latest && apply(saved), () => {});
      throw error;
    }
  };
}
