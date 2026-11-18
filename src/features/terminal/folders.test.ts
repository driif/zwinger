import assert from "node:assert/strict";
import { test } from "node:test";
import type { TerminalView } from "../../shared/api";
import { editorLabel, folderLabel, liveFolder, mergeFolder } from "./folders.ts";

const home = "/Users/me";

test("a Mac folder shows with ~ and a VM folder as it is", () => {
  assert.equal(folderLabel({ path: "/Users/me/dev/app/src", vm: false, opens: "/Users/me/dev/app/src" }, home), "~/dev/app/src");
  assert.equal(folderLabel({ path: "/Users/me/dev", vm: true, opens: null }, home), "/Users/me/dev");
  assert.equal(folderLabel({ path: "/workspace/src", vm: true, opens: "/Users/me/dev/app/src" }, undefined), "/workspace/src");
});

test("a long folder keeps its start and end", () => {
  const label = folderLabel({ path: `/Users/me/${"deep/".repeat(20)}end`, vm: false, opens: null }, home);
  assert.equal(label.length, 40);
  assert.ok(label.startsWith("~/deep/") && label.endsWith("/end") && label.includes("…"), label);
});

test("the editor label names the folder that opens", () => {
  const project = "/Users/me/dev/app";
  assert.equal(
    editorLabel("Zed", { path: "/workspace/src", vm: true, opens: "/Users/me/dev/app/src" }, project, home),
    "Open ~/dev/app/src in Zed",
  );
  assert.equal(
    editorLabel("Zed", { path: "/etc", vm: true, opens: null }, project, home),
    "Open project folder ~/dev/app in Zed (VM folder /etc is not on the Mac)",
  );
  assert.equal(
    editorLabel("Zed", { path: "/Users/me/gone", vm: false, opens: null }, project, home),
    "Open project folder ~/dev/app in Zed (~/gone is gone)",
  );
  assert.equal(editorLabel("Zed", null, project, home), "Open project folder ~/dev/app in Zed (the folder of the terminal is unknown)");
});

const at = (path: string) => ({ path, vm: true, opens: null });

const process = (id: string, seq: number, path: string, exitCode: number | null = null): TerminalView => ({
  id,
  kind: { type: "shell" },
  exitCode,
  terminalId: "t1",
  agent: null,
  folder: at(path),
  folderSeq: seq,
});

test("a late batch reply does not undo a newer event or listing", () => {
  // The batch read /workspace/alpha/old at seq 1 and is held.
  const old = { seq: 1, folder: at("/workspace/alpha/old") };
  // OSC 7 /workspace/alpha/new: the event, then the list refresh.
  let kept = mergeFolder(undefined, { seq: 3, folder: at("/workspace/alpha/new") });
  const listing = process("p1", 3, "/workspace/alpha/new");
  assert.equal(liveFolder(listing, kept, new Set())?.path, "/workspace/alpha/new");
  // The old batch is released.
  kept = mergeFolder(kept, old);
  assert.equal(liveFolder(listing, kept, new Set())?.path, "/workspace/alpha/new");
  // Also with no event kept: the listing is newer.
  assert.equal(liveFolder(listing, old, new Set())?.path, "/workspace/alpha/new");
  // A newer reply wins over an older listing.
  assert.equal(liveFolder(process("p1", 1, "/workspace/alpha/old"), { seq: 4, folder: at("/x") }, new Set())?.path, "/x");
});

test("an exited process shows no folder and its replacement is not affected", () => {
  const exited = new Set(["old"]);
  const late = { seq: 9, folder: at("/workspace/late") };
  assert.equal(liveFolder(process("old", 2, "/workspace"), late, exited), null, "a late listing or reply of the old id");
  assert.equal(liveFolder(process("old", 2, "/workspace", 0), undefined, new Set()), null);
  assert.equal(liveFolder(process("new", 0, "/workspace"), undefined, exited)?.path, "/workspace");
});
