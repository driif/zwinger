import assert from "node:assert/strict";
import { test } from "node:test";
import type { Prefs } from "../settings/api.ts";
import { BUILT_INS, resolveTheme } from "./themes.ts";

const custom = { ...BUILT_INS[0], id: "custom" };
const themes = [...BUILT_INS, custom];
const prefs = { theme: custom.id, followSystem: false, lightTheme: "zwinger-light", darkTheme: "zwinger-contrast" } as Prefs;

test("the selected user theme stays active across system appearance changes", () => {
  assert.equal(resolveTheme(prefs, themes, true), custom);
  assert.equal(resolveTheme(prefs, themes, false), custom);
});

test("following the system selects the configured dark or light theme", () => {
  const following = { ...prefs, followSystem: true };
  assert.equal(resolveTheme(following, themes, true).id, "zwinger-contrast");
  assert.equal(resolveTheme(following, themes, false).id, "zwinger-light");
});

test("a missing theme falls back to the built-in dark theme", () => {
  assert.equal(resolveTheme(prefs, BUILT_INS, false).id, "zwinger-dark");
});
