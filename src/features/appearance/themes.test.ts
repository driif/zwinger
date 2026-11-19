import assert from "node:assert/strict";
import { test } from "node:test";
import type { Prefs } from "../settings/api.ts";
import { importAndReload, importedMessage, type ImportResult } from "./api.ts";
import { BUILT_INS, isBuiltIn, resolveTheme } from "./themes.ts";

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

test("the import notice names the theme and a new name", () => {
  const theme = { ...custom, name: "Sea 2" };
  assert.equal(importedMessage({ theme, originalName: "Sea", renamed: true }), "Imported 'Sea' as 'Sea 2'");
  assert.equal(importedMessage({ theme: custom, originalName: custom.name, renamed: false }), `Imported '${custom.name}'`);
});

test("an import reloads the themes and returns the notice", async () => {
  let reloads = 0;
  const result: ImportResult = { theme: custom, originalName: custom.name, renamed: false };
  const message = await importAndReload(async () => void reloads++, async () => result);
  assert.equal(message, `Imported '${custom.name}'`);
  assert.equal(reloads, 1);
});

test("a cancelled import does not reload and gives no notice", async () => {
  let reloads = 0;
  assert.equal(await importAndReload(async () => void reloads++, async () => null), undefined);
  assert.equal(reloads, 0);
});

test("a failed import rejects and does not reload", async () => {
  let reloads = 0;
  const failing = async (): Promise<ImportResult | null> => {
    throw new Error("bad file");
  };
  await assert.rejects(importAndReload(async () => void reloads++, failing), /bad file/);
  assert.equal(reloads, 0);
});

test("Export is only for custom themes", () => {
  assert.ok(BUILT_INS.every((theme) => isBuiltIn(theme.id)));
  assert.equal(isBuiltIn(custom.id), false);
});
