import assert from "node:assert/strict";
import { test } from "node:test";
import type { Theme } from "../appearance/api.ts";
import { ghosttyTheme } from "./settings.ts";

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
