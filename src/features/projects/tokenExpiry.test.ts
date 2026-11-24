import assert from "node:assert/strict";
import { test } from "node:test";
import { dayEnd, expiresSoon, expiryLabel } from "./tokenExpiry.ts";

test("Claude token warns from 30 days before expiry", () => {
  const now = 1_800_000_000_000;
  const day = 24 * 60 * 60 * 1000;
  assert.equal(expiresSoon(now + 31 * day, now), false);
  assert.equal(expiresSoon(now + 30 * day, now), true);
  assert.equal(expiresSoon(now + day, now), true);
});

test("a token stays valid through its entered expiry day", () => {
  assert.equal(dayEnd(""), null);
  const end = dayEnd("2027-11-24")!;
  assert.equal(end, new Date(2027, 10, 25).getTime());
  // The last moment of the day is still valid, the next midnight is not.
  assert.ok(new Date(2027, 10, 24, 23, 59, 59, 999).getTime() < end);
  assert.ok(!(new Date(2027, 10, 25).getTime() < end));
  // The end of a month and of a year.
  assert.equal(dayEnd("2027-12-31"), new Date(2028, 0, 1).getTime());
});

test("the label shows the entered day, and only the estimate says about", () => {
  const at = dayEnd("2027-11-24")!;
  const day = new Date(2027, 10, 24).toLocaleDateString();
  assert.equal(expiryLabel(at, true), `expires ${day}`);
  assert.equal(expiryLabel(at, false), `expires about ${day}`);
});
