import assert from "node:assert/strict";
import { test } from "node:test";

/** The saved prefs of the fake backend, as plain fields. */
let saved: Record<string, unknown> = { density: "default", railWidth: 256 };
/** The replies that wait. A test releases them in the order it wants. */
const pending: (() => void)[] = [];
let failNext = false;
Object.assign(globalThis, {
  window: {
    dispatchEvent: () => true,
    __TAURI_INTERNALS__: {
      invoke: (command: string, args: { patch?: Record<string, unknown> }) => {
        if (command === "get_preferences") return Promise.resolve({ ...saved });
        const fail = failNext;
        failNext = false;
        if (!fail) saved = { ...saved, ...args.patch };
        const reply = { ...saved };
        return new Promise((resolve, reject) =>
          pending.push(() => (fail ? reject({ code: "Io", message: "disk full" }) : resolve(reply))),
        );
      },
    },
  },
});
const { prefsWriter } = await import("./prefsWriter.ts");

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

test("two overlapping saves of different fields show both", async () => {
  saved = { density: "default", railWidth: 256 };
  const shown: unknown[] = [];
  const write = prefsWriter((prefs) => shown.push(prefs));
  const first = write({ density: "compact" });
  const second = write({ railWidth: 300 });
  // The older reply arrives last: it must not replace the newer prefs.
  pending.splice(1, 1)[0]();
  pending.splice(0, 1)[0]();
  await Promise.all([first, second]);
  assert.deepEqual(shown, [{ density: "compact", railWidth: 300 }]);
});

test("a failed latest save reads the saved prefs, so an earlier save still shows", async () => {
  saved = { density: "default", railWidth: 256 };
  const shown: unknown[] = [];
  const write = prefsWriter((prefs) => shown.push(prefs));
  const first = write({ density: "compact" });
  failNext = true;
  const second = write({ railWidth: 300 });
  pending.splice(0, 1)[0]();
  pending.splice(0, 1)[0]();
  await first;
  await assert.rejects(second, { message: "disk full" });
  await settle();
  assert.deepEqual(shown, [{ density: "compact", railWidth: 256 }]);
});

test("a failed older save keeps the newer prefs", async () => {
  saved = { density: "default", railWidth: 256 };
  const shown: unknown[] = [];
  const write = prefsWriter((prefs) => shown.push(prefs));
  failNext = true;
  const first = write({ density: "compact" });
  const second = write({ railWidth: 300 });
  pending.splice(0, 1)[0]();
  pending.splice(0, 1)[0]();
  await assert.rejects(first, { message: "disk full" });
  await second;
  await settle();
  assert.deepEqual(shown, [{ density: "default", railWidth: 300 }]);
});
