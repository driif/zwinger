import assert from "node:assert/strict";
import { test } from "node:test";
import { unfinishedEffects } from "./gitOperation.ts";

test("a commit with a failed push says that it is not pushed", () => {
  assert.equal(
    unfinishedEffects({ token: "t", kind: "pr", state: "failed", sha: "abc", step: "push", detail: "git push failed" }),
    "The commit abc is on your local branch. It is not pushed. The PR is not open. The step push failed: git push failed",
  );
});

test("a pushed commit with a failed PR names the branch of origin", () => {
  assert.equal(
    unfinishedEffects({ token: "t", kind: "pr", state: "failed", sha: "abc", ref: "zwinger/fix-01", step: "pr", detail: "422" }),
    "The commit abc is on your local branch. It is pushed to zwinger/fix-01 of origin. The PR is not open yet. The step pr failed: 422",
  );
});

test("an operation with no recorded commit has an unknown result", () => {
  assert.equal(
    unfinishedEffects({ token: "t", kind: "pr", state: "planned" }),
    "A PR started, but its result is not known. Continue checks what is already done.",
  );
});
