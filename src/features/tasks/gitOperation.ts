import type { GitOperation } from "./api.ts";

/** What an unfinished Git operation already did, from its journal. */
export function unfinishedEffects(op: GitOperation) {
  const what = op.kind === "pr" ? "PR" : "commit";
  if (!op.sha) return `A ${what} started, but its result is not known. Continue checks what is already done.`;
  const pushed = op.ref
    ? `It is pushed to ${op.ref} of origin. The PR is not open yet.`
    : "It is not pushed. The PR is not open.";
  const failed = op.state === "failed" ? ` The step ${op.step} failed: ${op.detail}` : "";
  return `The commit ${op.sha} is on your local branch. ${op.kind === "pr" ? pushed : ""}${failed}`.trim();
}
