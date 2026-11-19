import assert from "node:assert/strict";
import { test } from "node:test";
import xterm from "@xterm/xterm";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { cellRange, fileLinkProvider, findLocations, logicalLine, type Location } from "./fileLinks.ts";

type Term = InstanceType<typeof xterm.Terminal>;

const write = (term: Term, data: string) => new Promise<void>((resolve) => term.write(data, resolve));

function terminal(cols: number) {
  const term = new xterm.Terminal({ allowProposedApi: true, cols, rows: 10 });
  term.loadAddon(new Unicode11Addon());
  term.unicode.activeVersion = "11";
  return term;
}

const found = (text: string) => findLocations(text).map(({ path, line, col }) => ({ path, line, col }));
const at = (path: string, line: number, col?: number) => ({ path, line, col });

test("finds path:line, path:line:col, path(line,col) and the Rust arrow", () => {
  assert.deepEqual(found("src/main.ts:12"), [at("src/main.ts", 12)]);
  assert.deepEqual(found("error at ./a/b.rs:3:9: bad"), [at("./a/b.rs", 3, 9)]);
  assert.deepEqual(found("src/app.ts(14,5): error TS2322"), [at("src/app.ts", 14, 5)]);
  assert.deepEqual(found("src/app.ts(14): x"), [at("src/app.ts", 14)]);
  assert.deepEqual(found("  --> src/terminal/mod.rs:120:17"), [at("src/terminal/mod.rs", 120, 17)]);
  assert.deepEqual(found("Makefile:7: *** missing separator"), [at("Makefile", 7)]);
  assert.deepEqual(found("/abs/x.go:1 and y.py:2"), [at("/abs/x.go", 1), at("y.py", 2)]);
});

test("quotes, brackets and trailing punctuation are not in the link", () => {
  for (const text of ['"src/a.ts:3"', "'src/a.ts:3'", "(src/a.ts:3)", "[src/a.ts:3]", "<src/a.ts:3>", "see src/a.ts:3.", "src/a.ts:3,", "src/a.ts:3;", "`src/a.ts:3`"]) {
    const [location] = findLocations(text);
    assert.deepEqual(at(location.path, location.line, location.col), at("src/a.ts", 3), text);
    assert.equal(text.slice(location.index, location.index + location.length), "src/a.ts:3", text);
  }
});

test("a column is kept only when it is unambiguous", () => {
  const [location] = findLocations("app.log:12:30:45 started");
  assert.deepEqual(at(location.path, location.line, location.col), at("app.log", 12));
  assert.equal(location.length, "app.log:12".length);
  assert.deepEqual(found("a.rs:12.5"), []);
  assert.deepEqual(found("a.rs:0"), []);
  assert.deepEqual(found("a.rs:4:0"), [at("a.rs", 4)]);
});

test("URLs, times, plain numbers and host:port give no candidate", () => {
  assert.deepEqual(found("open http://localhost:3000/src/a.ts:3 now"), []);
  assert.deepEqual(found("file:///Users/me/a.rs:3"), []);
  assert.deepEqual(found("at 12:30 and 12:30:45 and 1.5:3"), []);
  assert.deepEqual(found("listening on localhost:3000, LOCALHOST:8080 and 127.0.0.1:5173"), []);
  assert.deepEqual(found("https://x.io/a and b.rs:2"), [at("b.rs", 2)]);
});

test("a quoted path with spaces links whole or not at all, never a suffix", () => {
  for (const [text, link] of [
    ['"src/My File.ts:43"', "src/My File.ts:43"],
    ["'a b.rs:12'", "a b.rs:12"],
    ['"src/My File.ts":42', '"src/My File.ts":42'],
  ]) {
    const [location, ...rest] = findLocations(text);
    assert.equal(rest.length, 0, text);
    assert.equal(text.slice(location.index, location.index + location.length), link, text);
  }
  assert.deepEqual(found('"src/My File.ts:43"'), [at("src/My File.ts", 43)]);
  assert.deepEqual(found('"src/My File.ts":42:7'), [at("src/My File.ts", 42, 7)]);
  assert.deepEqual(found("'see a b.rs:12 now'"), []);
});

test("brackets and parentheses in a path stay in the path", () => {
  assert.deepEqual(found("app/(group)/page.tsx:12"), [at("app/(group)/page.tsx", 12)]);
  assert.deepEqual(found("app/[id]/page.tsx:3:1"), [at("app/[id]/page.tsx", 3, 1)]);
  assert.deepEqual(found("(group)/page.tsx:3"), [at("(group)/page.tsx", 3)]);
  assert.deepEqual(found("at fn (app/[id]/page.tsx:3:1)"), [at("app/[id]/page.tsx", 3, 1)]);
  assert.deepEqual(found("src/app.cs(14, 5): error"), [at("src/app.cs", 14, 5)]);
});

test("a long line without locations is scanned in linear time", () => {
  const base64 = "QUJD".repeat(4109) + ":1";
  const start = performance.now();
  for (let i = 0; i < 20; i++) findLocations(`${base64} ${base64}`);
  assert.ok(performance.now() - start < 200, "20 scans of 32 KB take less than 200 ms");
});

test("the logical line of a colored row maps each character to its cell", async () => {
  const term = terminal(40);
  await write(term, "\x1b[31merror\x1b[0m: \x1b[1;34msrc/a.rs:3:4\x1b[0m\r\n");
  const line = logicalLine(term.buffer.active, 0);
  assert.equal(line.text, "error: src/a.rs:3:4");
  const [location] = findLocations(line.text);
  assert.deepEqual(cellRange(line, location.index, location.length), { start: { x: 8, y: 1 }, end: { x: 19, y: 1 } });
  term.dispose();
});

test("a soft-wrapped line joins its rows, and the range spans them", async () => {
  const term = terminal(10);
  await write(term, "at src/long/b.rs:42 x\r\n");
  for (const row of [0, 1, 2]) assert.equal(logicalLine(term.buffer.active, row).text, "at src/long/b.rs:42 x");
  const line = logicalLine(term.buffer.active, 2);
  const [location] = findLocations(line.text);
  // `src/long/b.rs:42` starts at column 4 of row 1 and ends at column 9 of row 2.
  assert.deepEqual(cellRange(line, location.index, location.length), { start: { x: 4, y: 1 }, end: { x: 9, y: 2 } });
  // A hard line break ends the logical line.
  assert.equal(logicalLine(term.buffer.active, 3).text, "");
  term.dispose();
});

test("wide characters take two cells, also at a wrap", async () => {
  const term = terminal(10);
  await write(term, "日本 a.rs:5\r\n");
  let line = logicalLine(term.buffer.active, 0);
  assert.equal(line.text, "日本 a.rs:5");
  let [location] = findLocations(line.text);
  // The two wide characters take cells 1 to 4, so the last `5` wraps to row 2.
  assert.deepEqual(cellRange(line, location.index, location.length), { start: { x: 6, y: 1 }, end: { x: 1, y: 2 } });
  term.dispose();

  const narrow = terminal(10);
  // The emoji does not fit in the last cell of row 0, so it wraps and leaves that cell empty.
  await write(narrow, "abcdefghi😀 ü.rs:2\r\n");
  line = logicalLine(narrow.buffer.active, 1);
  assert.equal(line.text, "abcdefghi😀 ü.rs:2");
  [location] = findLocations(line.text);
  assert.deepEqual(at(location.path, location.line), at("ü.rs", 2));
  assert.deepEqual(cellRange(line, location.index, location.length), { start: { x: 4, y: 2 }, end: { x: 9, y: 2 } });
  const emoji = line.text.indexOf("😀");
  assert.deepEqual(cellRange(line, emoji, 2), { start: { x: 1, y: 2 }, end: { x: 2, y: 2 } });
  narrow.dispose();
});

test("the provider links only confirmed files, opens only on ⌘-click, and checks again on each request", async () => {
  const term = terminal(40);
  await write(term, "a.rs:1 b.rs:2 a.rs:3 localhost:3000\r\n");
  const checked: string[] = [];
  const opened: Location[] = [];
  const files = new Set(["a.rs", "localhost"]);
  const provider = fileLinkProvider(
    term,
    async (path) => {
      checked.push(path);
      if (!files.has(path)) throw new Error("no file");
    },
    (location) => opened.push(location),
  );
  const links = () =>
    new Promise<{ text: string; activate: (event: MouseEvent, text: string) => void }[] | undefined>((resolve) =>
      provider.provideLinks(1, resolve),
    );
  const first = await links();
  // A file named `localhost` does not make `localhost:3000` a link.
  assert.deepEqual(first?.map((l) => l.text), ["a.rs:1", "a.rs:3"]);
  assert.deepEqual(checked, ["a.rs", "b.rs"], "one check per path");

  first![0].activate({ metaKey: false } as MouseEvent, "a.rs:1");
  assert.equal(opened.length, 0, "a plain click opens nothing");
  first![1].activate({ metaKey: true } as MouseEvent, "a.rs:3");
  assert.deepEqual(opened.map((l) => [l.path, l.line]), [["a.rs", 3]]);

  // A folder change with no output: the next request sees the new files.
  files.add("b.rs");
  assert.deepEqual((await links())?.map((l) => l.text), ["a.rs:1", "b.rs:2", "a.rs:3"]);
  assert.equal(checked.length, 4);
  files.clear();
  assert.equal(await links(), undefined);
  term.dispose();
});

test("a window cut never links part of a longer word", async () => {
  const term = terminal(10);
  // 3000 characters of one word, then a location: the window around the last row cuts the word.
  await write(term, "x".repeat(3000) + "/a.rs:1 b.rs:2\r\n");
  const rows = Math.ceil(3015 / 10);
  const line = logicalLine(term.buffer.active, rows - 1);
  assert.ok(line.text.length <= 2048 + 2048 + 10);
  assert.deepEqual(found(line.text), [at("b.rs", 2)]);
  term.dispose();
});

test("a quoted path in brackets links whole, and an unpaired quote links nothing", () => {
  for (const [text, link] of [
    ['("/Users/me/My Project/a.ts:42")', "/Users/me/My Project/a.ts:42"],
    ["at ['src/My File.ts:3'],", "src/My File.ts:3"],
    ['("src/My File.ts":42)', '"src/My File.ts":42'],
  ]) {
    const [location, ...rest] = findLocations(text);
    assert.equal(rest.length, 0, text);
    assert.equal(text.slice(location.index, location.index + location.length), link, text);
  }
  assert.deepEqual(found('("/Users/me/My Project/a.ts:42")'), [at("/Users/me/My Project/a.ts", 42)]);
  for (const text of ['"/Users/me/My Project/a.ts:42', 'My Project/a.ts:42"', 'x"y.rs:3', '"a b.rs":3"']) {
    assert.deepEqual(found(text), [], text);
  }
  // An apostrophe in a different word does not matter.
  assert.deepEqual(found("it's in src/a.rs:3"), [at("src/a.rs", 3)]);
});

/** A provider whose checks wait until the test resolves them. */
function deferredProvider(term: Term) {
  const pending: (() => void)[] = [];
  const opened: Location[] = [];
  const provider = fileLinkProvider(
    term,
    () => new Promise<void>((resolve) => pending.push(resolve)),
    (location) => opened.push(location),
  );
  const links = () =>
    new Promise<{ text: string; activate: (event: MouseEvent, text: string) => void }[] | undefined>((resolve) =>
      provider.provideLinks(1, resolve),
    );
  return { pending, opened, links };
}

test("a redraw while the checks run gives no links", async () => {
  const term = terminal(40);
  await write(term, "a.rs:1\r\n");
  const { pending, links } = deferredProvider(term);
  const result = links();
  await write(term, "\x1b[H\x1b[2Kb.rs:1\r\n");
  for (const resolve of pending) resolve();
  assert.equal(await result, undefined);
  term.dispose();
});

test("a redraw, a reflow or a buffer switch before the click opens nothing", async () => {
  const term = terminal(40);
  await write(term, "x a.rs:1\r\n");
  const { pending, opened, links } = deferredProvider(term);
  const request = async () => {
    const result = links();
    await Promise.resolve();
    for (const resolve of pending.splice(0)) resolve();
    return (await result)!;
  };
  const click = { metaKey: true } as MouseEvent;

  let [link] = await request();
  await write(term, "\x1b[H\x1b[2Kx b.rs:1\r\n");
  link.activate(click, link.text);
  assert.equal(opened.length, 0, "the row has a different text");

  [link] = await request();
  term.resize(4, 10);
  link.activate(click, link.text);
  assert.equal(opened.length, 0, "the location moved to different cells");
  term.resize(40, 10);

  [link] = await request();
  await write(term, "\x1b[?1049h");
  link.activate(click, link.text);
  assert.equal(opened.length, 0, "the alternate screen is active");
  await write(term, "\x1b[?1049l");

  link.activate(click, link.text);
  assert.deepEqual(opened.map((l) => l.path), ["b.rs"], "the unchanged row opens");
  term.dispose();
});
