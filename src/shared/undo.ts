import { dismissToast, toast } from "@driif/motiv";
import type { Run } from "../App.tsx";
import { removeRule } from "../features/network/api.ts";
import { removeMount } from "../features/settings/api.ts";
import { type Removal, undoRemoval } from "./api.ts";

/**
 * Shows a toast with Undo for a removal in cage `cageId`. Undo restores the removal through `run`, so an error shows
 * as each other error. The window starts at `sent`, when the removal request was sent: a late reply gets a shorter
 * toast, or no toast when the receipt has expired. The toast goes at the deadline, also while hover or focus pauses it.
 */
export function offerUndo(run: Run, cageId: string, label: string, removal: Removal, sent: number) {
  const left = removal.expiresInMs - (performance.now() - sent);
  if (left <= 0) return;
  const id = toast({
    title: `Removed ${label}`,
    duration: left,
    action: { label: "Undo", onClick: () => void run(() => undoRemoval(cageId, removal.token)) },
  });
  setTimeout(() => dismissToast(id), left);
}

/** Removes the rule through `run` and offers Undo. A failed removal shows the `run` error and no toast. */
export const removeRuleWithUndo = (run: Run, cageId: string, ruleId: string, label: string) =>
  run(() => {
    const sent = performance.now();
    return removeRule(cageId, ruleId).then((removal) => offerUndo(run, cageId, label, removal, sent));
  });

/** Removes the mount through `run` and offers Undo. A failed removal shows the `run` error and no toast. */
export const removeMountWithUndo = (run: Run, cageId: string, guest: string) =>
  run(() => {
    const sent = performance.now();
    return removeMount(cageId, guest).then((removal) => offerUndo(run, cageId, guest, removal, sent));
  });
