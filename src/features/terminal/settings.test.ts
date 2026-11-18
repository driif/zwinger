import assert from "node:assert/strict";
import { test } from "node:test";
import type { Theme } from "../appearance/api.ts";
import { adjustmentNote, clampNumber, ghosttyTheme, NUMBERS, type NumberSetting } from "./settings.ts";

const ansi = Array.from({ length: 16 }, (_, i) => `#0000${i.toString(16).padStart(2, "0")}`);
const current: Theme = {
  id: "zwinger-dark",
  name: "zwinger dark",
  base: "dark",
  tokens: { "--motiv-color-bg": "#111111" },
  terminal: { background: "#000000", foreground: "#ffffff", cursor: "#ff0000", selection: "#333333", ansi },
};
const palette = { background: "#1e1e2e", foreground: null, cursor: null, selection: "#585b70", ansi: ["#45475a", ...Array(15).fill(null)] };

test("the Ghostty theme takes the palette and fills the rest from the current theme", () => {
  const theme = ghosttyTheme(current, palette, [current]);
  assert.equal(theme.id, "ghostty");
  assert.equal(theme.base, "dark");
  assert.deepEqual(theme.tokens, current.tokens);
  assert.deepEqual(theme.terminal, {
    background: "#1e1e2e",
    foreground: "#ffffff",
    cursor: "#ff0000",
    selection: "#585b70",
    ansi: ["#45475a", ...ansi.slice(1)],
  });
});

test("a new import replaces the Ghostty theme", () => {
  const earlier = { ...current, id: "custom-1", name: "Ghostty" };
  assert.equal(ghosttyTheme(current, palette, [current, earlier]).id, "custom-1");
});

test("a number out of range is clamped with a note, at both ends of each setting", () => {
  assert.deepEqual(clampNumber("fontSize", 64), { value: 32, note: "Font size: 64 is more than the maximum. Set to 32." });
  assert.deepEqual(clampNumber("fontSize", 4), { value: 8, note: "Font size: 4 is less than the minimum. Set to 8." });
  assert.deepEqual(clampNumber("lineHeight", 3), { value: 2, note: "Line height: 3 is more than the maximum. Set to 2." });
  assert.deepEqual(clampNumber("lineHeight", 0.5), { value: 1, note: "Line height: 0.5 is less than the minimum. Set to 1." });
  assert.deepEqual(clampNumber("padding", 80), { value: 32, note: "Padding: 80 is more than the maximum. Set to 32." });
  assert.deepEqual(clampNumber("padding", -4), { value: 0, note: "Padding: -4 is less than the minimum. Set to 0." });
  assert.deepEqual(clampNumber("scrollback", 1e6), {
    value: 100000,
    note: "Scrollback lines: 1000000 is more than the maximum. Set to 100000.",
  });
  assert.deepEqual(clampNumber("scrollback", 10), {
    value: 1000,
    note: "Scrollback lines: 10 is less than the minimum. Set to 1000.",
  });
});

test("a boundary or in-range number has no note", () => {
  for (const [setting, { min, max }] of Object.entries(NUMBERS) as [NumberSetting, (typeof NUMBERS)[NumberSetting]][]) {
    assert.deepEqual(clampNumber(setting, min), { value: min, note: undefined });
    assert.deepEqual(clampNumber(setting, max), { value: max, note: undefined });
  }
  assert.deepEqual(clampNumber("lineHeight", 1.2), { value: 1.2, note: undefined });
});

test("an import note names the Ghostty line apart from the applied setting", () => {
  assert.equal(
    adjustmentNote("scrollback", "Ghostty scrollback-limit = 50000000", 100000),
    "Scrollback lines: Ghostty scrollback-limit = 50000000 is more than the maximum. Set to 100000.",
  );
  assert.equal(
    adjustmentNote("lineHeight", "Ghostty adjust-cell-height = -40%", 1),
    "Line height: Ghostty adjust-cell-height = -40% is less than the minimum. Set to 1.",
  );
});
