import assert from "node:assert/strict";
import { test } from "node:test";
import { latestRequest } from "./diffRequest.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

test("a late failure of an older request changes neither the diff nor the error", async () => {
  const requests = latestRequest();
  const state: { diff?: string; error?: string } = {};
  const read = (work: Promise<string>) => {
    const isCurrent = requests.next();
    state.error = undefined;
    return work.then(
      (text) => {
        if (isCurrent()) state.diff = text;
      },
      (e) => {
        if (isCurrent()) state.error = String(e);
      },
    );
  };
  const a = deferred<string>();
  const b = deferred<string>();
  const readA = read(a.promise);
  const readB = read(b.promise);
  b.resolve("diff B");
  await readB;
  a.reject("A failed");
  await readA;
  assert.deepEqual(state, { diff: "diff B", error: undefined });
});

test("drop makes an open request stale", () => {
  const requests = latestRequest();
  const isCurrent = requests.next();
  assert.equal(isCurrent(), true);
  requests.drop();
  assert.equal(isCurrent(), false);
});
