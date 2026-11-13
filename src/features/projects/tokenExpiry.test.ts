import assert from "node:assert/strict";
import { test } from "node:test";
import { expiresSoon } from "./tokenExpiry.ts";

test("Claude token warns from 30 days before expiry", () => {
  const now = 1_800_000_000_000;
  const day = 24 * 60 * 60 * 1000;
  assert.equal(expiresSoon(now + 31 * day, now), false);
  assert.equal(expiresSoon(now + 30 * day, now), true);
  assert.equal(expiresSoon(now + day, now), true);
});
