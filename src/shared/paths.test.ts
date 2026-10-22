import assert from "node:assert/strict";
import { test } from "node:test";
import { tildePath, truncateMiddle } from "./paths.ts";

test("the home folder becomes ~", () => {
  assert.equal(tildePath("/Users/me/dev/app", "/Users/me"), "~/dev/app");
  assert.equal(tildePath("/Users/me", "/Users/me/"), "~");
  assert.equal(tildePath("/Users/meg/app", "/Users/me"), "/Users/meg/app");
  assert.equal(tildePath("/opt/app", undefined), "/opt/app");
});

test("a long text keeps its start and end", () => {
  assert.equal(truncateMiddle("abcdef", 6), "abcdef");
  assert.equal(truncateMiddle("~/dev/my/project/zwinger", 11), "~/dev…inger");
  assert.equal(truncateMiddle("~/dev/my/project/zwinger", 11).length, 11);
});
