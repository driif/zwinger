import assert from "node:assert/strict";
import { test } from "node:test";
import { direction, duration, trendSummary } from "./startTrend.ts";

const starts = (...totals: number[]) => totals.map((totalMs, i) => ({ at: i, totalMs }));

test("the direction compares a total with the start before", () => {
  assert.equal(direction(3600, 4200), "slower");
  assert.equal(direction(4200, 3600), "faster");
  assert.equal(direction(4200, 4200), "same");
  assert.equal(direction(0, 0), "same");
});

test("a duration shows its unit", () => {
  assert.equal(duration(0), "0 ms");
  assert.equal(duration(999), "999 ms");
  assert.equal(duration(4200), "4.2 s");
  assert.equal(duration(4011), "4.011 s");
  assert.equal(duration(1000), "1 s");
});

test("the summary names the latest total and its change", () => {
  assert.equal(trendSummary(starts(4200)), "Latest 4.2 s. There is no start before it.");
  assert.equal(trendSummary(starts(3600, 4200)), "Latest 4.2 s, 600 ms slower than the start before");
  assert.equal(trendSummary(starts(4200, 4160)), "Latest 4.16 s, 40 ms faster than the start before");
  assert.equal(trendSummary(starts(2900, 4200)), "Latest 4.2 s, 1.3 s slower than the start before");
  assert.equal(trendSummary(starts(4011, 4012)), "Latest 4.012 s, 1 ms slower than the start before");
  assert.equal(trendSummary(starts(5000, 4200, 4200)), "Latest 4.2 s, the same as the start before");
});
