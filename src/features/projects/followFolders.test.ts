import assert from "node:assert/strict";
import { test } from "node:test";
import { type FolderApi, folderKey, followFolders } from "./followFolders.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const flush = () => new Promise((resolve) => setTimeout(resolve));

/** A mocked backend: each watch gets a channel and a token that the test resolves. */
function backend() {
  const watches: { id: string; send: (folders: string[] | undefined) => void; token: ReturnType<typeof deferred<number>> }[] = [];
  const reads: { id: string; result: ReturnType<typeof deferred<string[]>> }[] = [];
  const unwatched: number[] = [];
  const rescans: { token: number; result: ReturnType<typeof deferred<void>> }[] = [];
  const api: FolderApi = {
    watch: (id, send) => {
      const token = deferred<number>();
      watches.push({ id, send, token });
      return token.promise;
    },
    unwatch: (token) => unwatched.push(token),
    rescan: (token) => {
      const result = deferred<void>();
      rescans.push({ token, result });
      return result.promise;
    },
    list: (id) => {
      const result = deferred<string[]>();
      reads.push({ id, result });
      return result.promise;
    },
  };
  return { api, watches, reads, unwatched, rescans };
}

test("a folder change from the channel sets the list", async () => {
  const { api, watches } = backend();
  const seen: string[][] = [];
  followFolders("a", (f) => seen.push(f), api);
  watches[0].send(["web"]);
  watches[0].token.resolve(1);
  await flush();
  watches[0].send(["api", "web"]);
  assert.deepEqual(seen, [["web"], ["api", "web"]]);
});

test("a session switch closes the old watch and ignores its late lists", async () => {
  const { api, watches, unwatched } = backend();
  const seen: [string, string[]][] = [];
  const a = followFolders("a", (f) => seen.push(["a", f]), api);
  watches[0].token.resolve(1);
  await flush();
  a.close();
  followFolders("b", (f) => seen.push(["b", f]), api);
  watches[0].send(["old"]);
  watches[1].send(["new"]);
  assert.deepEqual(unwatched, [1]);
  assert.deepEqual(seen, [["b", ["new"]]]);
  assert.equal(watches[1].id, "b");
});

test("unmount closes the watch, and no list arrives after it", async () => {
  const { api, watches, unwatched } = backend();
  const seen: string[][] = [];
  const follow = followFolders("a", (f) => seen.push(f), api);
  watches[0].token.resolve(7);
  await flush();
  follow.close();
  watches[0].send(["web"]);
  assert.deepEqual(unwatched, [7]);
  assert.deepEqual(seen, []);
});

test("a token that arrives after unmount is unwatched", async () => {
  const { api, watches, unwatched } = backend();
  followFolders("a", () => {}, api).close();
  watches[0].token.resolve(3);
  await flush();
  assert.deepEqual(unwatched, [3]);
});

test("a failed watch falls back to a read now and on each reload", async () => {
  const { api, watches, reads } = backend();
  const seen: string[][] = [];
  const follow = followFolders("a", (f) => seen.push(f), api);
  watches[0].token.reject(new Error("no watch"));
  await flush();
  assert.equal(reads.length, 1);
  reads[0].result.resolve(["web"]);
  await flush();
  follow.reload();
  reads[1].result.resolve(["api", "web"]);
  await flush();
  assert.deepEqual(seen, [["web"], ["api", "web"]]);
});

test("a reload tells a working watch to read again", async () => {
  const { api, watches, reads, rescans } = backend();
  const follow = followFolders("a", () => {}, api);
  watches[0].token.resolve(4);
  await flush();
  follow.reload();
  assert.deepEqual(
    rescans.map((r) => r.token),
    [4],
  );
  assert.equal(reads.length, 0);
});

test("a watch that has ended falls back to a direct read", async () => {
  const { api, watches, reads, rescans } = backend();
  const seen: string[][] = [];
  const follow = followFolders("a", (f) => seen.push(f), api);
  watches[0].token.resolve(4);
  await flush();
  follow.reload();
  rescans[0].result.reject(new Error("the watch has ended"));
  await flush();
  reads[0].result.resolve(["web"]);
  await flush();
  watches[0].send(["stale"]);
  follow.reload();
  assert.equal(rescans.length, 1);
  assert.equal(reads.length, 2);
  assert.deepEqual(seen, [["web"]]);
});

test("a list of one folder does not show for an other folder of the same project", () => {
  assert.notEqual(folderKey({ id: "a", folder: "/one" }), folderKey({ id: "a", folder: "/two" }));
  assert.notEqual(folderKey({ id: "a", folder: "/one" }), folderKey({ id: "b", folder: "/one" }));
});

test("an older fallback read does not replace a newer list", async () => {
  const { api, watches, reads } = backend();
  const seen: string[][] = [];
  const follow = followFolders("a", (f) => seen.push(f), api);
  watches[0].token.reject(new Error("no watch"));
  await flush();
  follow.reload();
  reads[1].result.resolve(["new"]);
  await flush();
  reads[0].result.resolve(["old"]);
  await flush();
  assert.deepEqual(seen, [["new"]]);
});

test("a fallback read that ends after unmount sets nothing", async () => {
  const { api, watches, reads } = backend();
  const seen: string[][] = [];
  const follow = followFolders("a", (f) => seen.push(f), api);
  watches[0].token.reject(new Error("no watch"));
  await flush();
  follow.close();
  reads[0].result.resolve(["web"]);
  await flush();
  assert.deepEqual(seen, []);
});

test("an unreadable root from the channel is undefined, and an empty list stays empty", async () => {
  const { api, watches } = backend();
  const seen: (string[] | undefined)[] = [];
  followFolders("a", (f) => seen.push(f), api);
  watches[0].send(undefined);
  watches[0].send([]);
  assert.deepEqual(seen, [undefined, []]);
});

test("the newest fallback read that fails sets undefined", async () => {
  const { api, watches, reads } = backend();
  const seen: (string[] | undefined)[] = [];
  const follow = followFolders("a", (f) => seen.push(f), api);
  watches[0].token.reject(new Error("no watch"));
  await flush();
  reads[0].result.resolve([]);
  await flush();
  follow.reload();
  reads[1].result.reject(new Error("missing root"));
  await flush();
  assert.deepEqual(seen, [[], undefined]);
});

test("a failed fallback read that is superseded or closed sets nothing", async () => {
  const { api, watches, reads } = backend();
  const seen: (string[] | undefined)[] = [];
  const follow = followFolders("a", (f) => seen.push(f), api);
  watches[0].token.reject(new Error("no watch"));
  await flush();
  follow.reload();
  reads[1].result.resolve(["new"]);
  await flush();
  reads[0].result.reject(new Error("missing root"));
  await flush();
  follow.reload();
  follow.close();
  reads[2].result.reject(new Error("missing root"));
  await flush();
  assert.deepEqual(seen, [["new"]]);
});
