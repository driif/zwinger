import assert from "node:assert/strict";
import test from "node:test";
import { stylesheetsSettled } from "./reveal.ts";

type Listener = () => void;

class FakeLink {
  rel = "";
  href = "";
  crossOrigin: string | null = "";
  sheet: object | null = null;
  removed = false;
  listeners = new Map<string, Listener[]>();
  onListen?: () => void;

  addEventListener(type: string, listener: Listener) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
    this.onListen?.();
  }

  fire(type: "load" | "error") {
    if (type === "load") this.sheet = {};
    for (const listener of this.listeners.get(type) ?? []) listener();
  }

  remove() {
    this.removed = true;
  }
}

function fakeDocument(links: FakeLink[]) {
  const probes: FakeLink[] = [];
  const doc = {
    head: {
      querySelectorAll: () => links,
      append: (probe: FakeLink) => probes.push(probe),
    },
    createElement: () => new FakeLink(),
  };
  return { doc: doc as unknown as Document, probes };
}

const link = (href: string) => Object.assign(new FakeLink(), { rel: "stylesheet", href, crossOrigin: "anonymous" });

async function isSettled(promise: Promise<void>) {
  let settled = false;
  void promise.then(() => (settled = true));
  await new Promise((resolve) => setImmediate(resolve));
  return settled;
}

test("no stylesheet links resolve at once", async () => {
  const { doc } = fakeDocument([]);
  assert.equal(await isSettled(stylesheetsSettled(doc)), true);
});

test("an already loaded link resolves at once without a probe", async () => {
  const loaded = Object.assign(link("/a.css"), { sheet: {} });
  const { doc, probes } = fakeDocument([loaded]);
  assert.equal(await isSettled(stylesheetsSettled(doc)), true);
  assert.equal(probes.length, 0);
});

test("a pending link keeps the wait open until it loads", async () => {
  const pending = link("/a.css");
  const { doc } = fakeDocument([pending]);
  const wait = stylesheetsSettled(doc);
  assert.equal(await isSettled(wait), false);
  pending.fire("load");
  assert.equal(await isSettled(wait), true);
});

test("a load between the listeners and the second check is not lost", async () => {
  const racing = link("/a.css");
  racing.onListen = () => (racing.sheet = {});
  const { doc, probes } = fakeDocument([racing]);
  assert.equal(await isSettled(stylesheetsSettled(doc)), true);
  assert.equal(probes.length, 0);
});

test("a stylesheet error resolves the wait", async () => {
  const failing = link("/a.css");
  const { doc } = fakeDocument([failing]);
  const wait = stylesheetsSettled(doc);
  failing.fire("error");
  assert.equal(await isSettled(wait), true);
});

test("an earlier failure resolves through a probe with the same request", async () => {
  const failed = link("/a.css");
  const { doc, probes } = fakeDocument([failed]);
  const wait = stylesheetsSettled(doc);
  assert.equal(await isSettled(wait), false);
  assert.equal(probes.length, 1);
  const [probe] = probes;
  assert.deepEqual([probe.rel, probe.href, probe.crossOrigin], ["stylesheet", "/a.css", "anonymous"]);
  assert.equal(probe.removed, false);
  probe.fire("error");
  assert.equal(await isSettled(wait), true);
  assert.equal(probe.removed, true);
});

test("the wait stays open while one of several links is pending", async () => {
  const loaded = Object.assign(link("/a.css"), { sheet: {} });
  const pending = link("/b.css");
  const { doc, probes } = fakeDocument([loaded, pending]);
  const wait = stylesheetsSettled(doc);
  assert.equal(await isSettled(wait), false);
  pending.fire("load");
  assert.equal(await isSettled(wait), true);
  assert.equal(probes[0].removed, false);
  probes[0].fire("load");
  assert.equal(probes[0].removed, true);
});

test("a probe that loads first keeps its rules until the original link settles", async () => {
  const pending = link("/a.css");
  const { doc, probes } = fakeDocument([pending]);
  const wait = stylesheetsSettled(doc);
  const [probe] = probes;
  probe.fire("load");
  assert.equal(await isSettled(wait), true);
  assert.equal(probe.removed, false);
  pending.fire("load");
  assert.equal(probe.removed, true);
});
