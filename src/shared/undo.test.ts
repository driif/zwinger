import assert from "node:assert/strict";
import { afterEach, beforeEach, mock, test } from "node:test";
import type { ToastOptions } from "@driif/motiv";

const shown: ToastOptions[] = [];
const dismissed: string[] = [];
mock.module("@driif/motiv", {
  exports: {
    toast: (options: ToastOptions) => String(shown.push(options)),
    dismissToast: (id: string) => dismissed.push(id),
  },
});
const invoked: [string, unknown][] = [];
let fail: string | null = null;
/** The stubbed `performance.now()`. The fake invoke advances it by `delay` before a remove_* reply. */
let clock = 0;
let delay = 0;
Object.assign(globalThis, {
  window: {
    dispatchEvent: () => true,
    __TAURI_INTERNALS__: {
      invoke: async (command: string, args: unknown) => {
        invoked.push([command, args]);
        if (command === fail) throw { code: "notFound", message: "gone" };
        if (command.startsWith("remove_")) {
          clock += delay;
          return { token: `t-${command}`, expiresInMs: 10_000 };
        }
      },
    },
  },
});
const { offerUndo, removeMountWithUndo, removeRuleWithUndo } = await import("./undo.ts");

/** A `run` like the app's: it awaits the action and keeps its error. */
const errors: unknown[] = [];
const runs: Promise<unknown>[] = [];
const run = (action: () => Promise<unknown>) => {
  const done = action().catch((e) => errors.push(e));
  runs.push(done);
  return done;
};

beforeEach(() => {
  shown.length = dismissed.length = invoked.length = errors.length = runs.length = 0;
  fail = null;
  clock = delay = 0;
  mock.method(performance, "now", () => clock);
});

afterEach(() => {
  mock.restoreAll();
  mock.timers.reset();
});

test("Undo restores the receipt in its own cage through run, and the toast goes at the deadline", async () => {
  mock.timers.enable({ apis: ["setTimeout"] });
  offerUndo(run, "cage-a", "a.io", { token: "t1", expiresInMs: 10_000 }, 0);

  const options = shown.at(-1)!;
  assert.equal(options.title, "Removed a.io");
  assert.equal(options.duration, 10_000);
  await options.action!.onClick();
  await Promise.all(runs);
  assert.deepEqual(invoked, [["undo_removal", { cageId: "cage-a", token: "t1" }]]);

  // Hover or focus can pause the toast: the timer dismisses it anyway.
  mock.timers.tick(9_999);
  assert.deepEqual(dismissed, []);
  mock.timers.tick(1);
  assert.deepEqual(dismissed, [String(shown.length)]);
});

// NetworkTab and CommandBar remove a rule, CageSettings and CommandBar remove a mount, through these entry points.
for (const entry of [
  {
    name: "rule",
    remove: () => removeRuleWithUndo(run, "cage-a", "r1", "a.io"),
    command: ["remove_rule", { cageId: "cage-a", ruleId: "r1" }],
    title: "Removed a.io",
  },
  {
    name: "mount",
    remove: () => removeMountWithUndo(run, "cage-a", "/data"),
    command: ["remove_mount", { cageId: "cage-a", guest: "/data" }],
    title: "Removed /data",
  },
] as const) {
  test(`a ${entry.name} removal offers Undo, and Undo restores its receipt in the same cage`, async () => {
    mock.timers.enable({ apis: ["setTimeout"] });
    await entry.remove();
    assert.deepEqual(errors, []);
    assert.equal(shown.length, 1);
    assert.equal(shown[0].title, entry.title);
    assert.equal(shown[0].action!.label, "Undo");

    await shown[0].action!.onClick();
    await Promise.all(runs);
    assert.deepEqual(invoked, [
      entry.command,
      ["undo_removal", { cageId: "cage-a", token: `t-${entry.command[0]}` }],
    ]);
  });

  test(`a ${entry.name} reply delayed to the deadline or later shows no toast and no error`, async () => {
    mock.timers.enable({ apis: ["setTimeout"] });
    delay = 10_000;
    await entry.remove();
    assert.deepEqual(errors, []);
    assert.deepEqual(shown, []);
    mock.timers.tick(10_000);
    assert.deepEqual(dismissed, []);
  });

  test(`a ${entry.name} reply delayed by 4 s gets a toast for the 6 s that are left`, async () => {
    mock.timers.enable({ apis: ["setTimeout"] });
    delay = 4_000;
    await entry.remove();
    assert.deepEqual(errors, []);
    assert.equal(shown.length, 1);
    assert.equal(shown[0].duration, 6_000);
    mock.timers.tick(5_999);
    assert.deepEqual(dismissed, []);
    mock.timers.tick(1);
    assert.deepEqual(dismissed, ["1"]);
  });

  test(`a failed ${entry.name} removal shows the run error and no toast`, async () => {
    fail = entry.command[0];
    await entry.remove();
    assert.deepEqual(errors, [{ code: "notFound", message: "gone" }]);
    assert.deepEqual(shown, []);
  });
}
