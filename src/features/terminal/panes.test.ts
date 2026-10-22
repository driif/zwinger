import assert from "node:assert/strict";
import { test } from "node:test";
import type { PaneNode } from "../../shared/api.ts";
import { leaves, neighbor, place, remove, replace, setRatio, split, tooDeep } from "./panes.ts";

const leaf = (terminal: string): PaneNode => ({ type: "leaf", terminal });

test("a split puts the new pane right or below its pane", () => {
  let layout = split(null, undefined, "a", "row");
  assert.deepEqual(layout, leaf("a"));
  layout = split(layout, "a", "b", "row");
  layout = split(layout, "b", "c", "column");
  assert.deepEqual(leaves(layout), ["a", "b", "c"]);
  const rects = Object.fromEntries(place(layout).panes.map((p) => [p.terminal, p.rect]));
  assert.deepEqual(rects.a, { left: 0, top: 0, width: 0.5, height: 1 });
  assert.deepEqual(rects.b, { left: 0.5, top: 0, width: 0.5, height: 0.5 });
  assert.deepEqual(rects.c, { left: 0.5, top: 0.5, width: 0.5, height: 0.5 });
  assert.equal(place(layout).splits.length, 2);
});

test("a removed pane gives its place to its sibling", () => {
  const layout = split(split(leaf("a"), "a", "b", "row"), "b", "c", "column");
  assert.deepEqual(remove(layout, "b"), { type: "split", direction: "row", ratio: 0.5, a: leaf("a"), b: leaf("c") });
  assert.deepEqual(remove(remove(layout, "b"), "a"), leaf("c"));
  assert.equal(remove(leaf("a"), "a"), null);
});

test("replace shows another terminal in a pane", () => {
  const layout = split(leaf("a"), "a", "b", "row");
  assert.deepEqual(leaves(replace(layout, "b", "x")), ["a", "x"]);
  assert.deepEqual(replace(null, undefined, "x"), leaf("x"));
});

test("a ratio change follows the path", () => {
  const layout = split(split(leaf("a"), "a", "b", "row"), "b", "c", "column");
  const changed = setRatio(layout, ["b"], 0.3);
  assert.equal(place(changed).splits[1].ratio, 0.3);
  assert.equal(place(changed).splits[0].ratio, 0.5);
});

test("focus moves to the nearest pane in the direction", () => {
  const layout = split(split(leaf("a"), "a", "b", "row"), "b", "c", "column");
  assert.equal(neighbor(layout, "a", "right"), "b");
  assert.equal(neighbor(layout, "c", "left"), "a");
  assert.equal(neighbor(layout, "b", "down"), "c");
  assert.equal(neighbor(layout, "c", "up"), "b");
  assert.equal(neighbor(layout, "a", "left"), undefined);
});

test("a pane at depth 8 cannot split", () => {
  let layout: PaneNode = leaf("t0");
  for (let i = 1; i <= 8; i++) layout = split(layout, `t${i - 1}`, `t${i}`, "row");
  assert.equal(tooDeep(layout, "t8"), true);
  assert.equal(tooDeep(layout, "t0"), false);
});
