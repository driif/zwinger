import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { BUILT_INS } from "./builtIns.ts";
import { parseCachedTheme } from "./themeCache.ts";

test("the hidden native window starts with the default dark background", () => {
  const config = JSON.parse(readFileSync("src-tauri/tauri.conf.json", "utf8"));
  assert.equal(config.app.windows[0].backgroundColor, BUILT_INS[0].tokens["--motiv-color-bg"]);
});

test("a cached theme cannot pass malformed fields to the active theme", () => {
  const tokens = { "--motiv-color-bg": "#21c881" };
  const theme = parseCachedTheme(JSON.stringify({
    id: 42,
    name: null,
    base: "light",
    tokens,
    terminal: null,
  }));
  assert.deepEqual(theme, {
    id: BUILT_INS[0].id,
    name: BUILT_INS[0].name,
    base: "light",
    tokens,
    terminal: BUILT_INS[0].terminal,
  });
});

test("an unsupported cached base uses the default theme", () => {
  assert.deepEqual(parseCachedTheme(JSON.stringify({ base: "unsupported", tokens: {} })), BUILT_INS[0]);
});

test("a valid cached theme keeps its terminal colors and identity", () => {
  const theme = { ...BUILT_INS[1], id: "custom-light", name: "Custom light" };
  assert.deepEqual(parseCachedTheme(JSON.stringify(theme)), theme);
});
