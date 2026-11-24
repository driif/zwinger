import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import test from "node:test";

const calls = {
  options: {} as Record<string, unknown>,
  osc: [] as number[],
  writes: [] as (string | Uint8Array)[],
  input: 0,
  links: 0,
  resize: 0,
  disposed: false,
  terminals: [] as FakeTerminal[],
};

class FakeTerminal {
  options: Record<string, unknown>;
  get unicode() {
    if (!this.options.allowProposedApi) throw Error("Unicode API is proposed");
    return { activeVersion: "" };
  }
  get parser() {
    if (!this.options.allowProposedApi) throw Error("Parser API is proposed");
    return { registerOscHandler: (code: number) => {
      calls.osc.push(code);
      return { dispose() {} };
    } };
  }
  buffer = { active: { viewportY: 0, baseY: 0 } };
  rows = 24;
  cols = 80;
  selection?: { start: { x: number; y: number }; end: { x: number; y: number } };
  scrolledTo?: number;
  selected?: [number, number, number];

  writes: (string | Uint8Array)[] = [];
  constructor(options: Record<string, unknown>) { this.options = options; calls.options = options; calls.terminals.push(this); }
  loadAddon() {}
  open() {}
  write(bytes: string | Uint8Array, done?: () => void) { calls.writes.push(bytes); this.writes.push(bytes); done?.(); }
  scrollToLine(line: number) { this.scrolledTo = line; }
  select(column: number, row: number, length: number) { this.selected = [column, row, length]; }
  getSelectionPosition() { return this.selection; }
  onData() { calls.input++; }
  onResize() { calls.resize++; }
  registerLinkProvider() { calls.links++; }
  dispose() { calls.disposed = true; }
}

Object.assign(globalThis, {
  historyTest: { Terminal: FakeTerminal, FitAddon: class { fit() {} }, Unicode11Addon: class {} },
  ResizeObserver: class { observe() {} disconnect() {} },
});

const xterm = "export const { Terminal, FitAddon, Unicode11Addon } = globalThis.historyTest;";
const api = "export const openTerminalLocation = () => {}; export const terminalLocation = () => {};";
const links = "export const fileLinkProvider = () => {};";
const opener = "export const openUrl = () => {};";
const virtual = (source: string) => `data:text/javascript,${encodeURIComponent(source)}`;

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (context.parentURL?.endsWith("/engine.ts")) {
      const source = specifier === "./xterm" ? xterm
        : specifier === "./api" ? api
        : specifier === "./fileLinks" ? links
        : specifier === "@tauri-apps/plugin-opener" ? opener
        : undefined;
      if (source) return { url: virtual(source), shortCircuit: true };
    }
    return nextResolve(specifier, context);
  },
});

test("saved output has no input, reply or link path", async () => {
  const { openHistory, setLook } = await import("./engine.ts");
  setLook({
    fontFamily: "monospace", ligatures: false, fontSize: 12, lineHeight: 1,
    cursorStyle: "block", cursorBlink: false, padding: 0, scrollback: 100,
    optionAsMeta: false, themeId: null,
  }, {
    background: "#000", foreground: "#fff", cursor: "#fff", selection: "#888",
    ansi: Array(16).fill("#fff"),
  });
  const element = {
    offsetParent: null,
    style: {},
    addEventListener() {},
    removeEventListener() {},
  } as unknown as HTMLElement;
  const output = Array(200).fill(65);
  const close = await openHistory(element, output);
  assert.equal(calls.options.allowProposedApi, true);
  assert.equal(calls.options.disableStdin, true);
  assert.equal(calls.options.scrollback, 100);
  assert.deepEqual(calls.osc, [0, 1, 2, 8, 52]);
  assert.deepEqual(calls.writes[0], Uint8Array.from(output));
  assert.equal(calls.writes[1], "\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?25l");
  assert.deepEqual([calls.input, calls.resize, calls.links], [0, 0, 0]);
  setLook({
    fontFamily: "monospace", ligatures: false, fontSize: 12, lineHeight: 1,
    cursorStyle: "block", cursorBlink: false, padding: 0, scrollback: 5,
    optionAsMeta: false, themeId: null,
  }, {
    background: "#111", foreground: "#fff", cursor: "#fff", selection: "#888",
    ansi: Array(16).fill("#fff"),
  });
  assert.equal(calls.options.scrollback, 100);
  close();
  assert.equal(calls.disposed, true);
});

test("each saved run has its own parser and screen", async () => {
  const { openHistory } = await import("./engine.ts");
  const element = {
    offsetParent: null, style: {}, addEventListener() {}, removeEventListener() {},
  } as unknown as HTMLElement;
  const first = await openHistory(element, Array.from(Buffer.from("first\x1b]52;unfinished")));
  const second = await openHistory(element, Array.from(Buffer.from("\x1b[3Jsecond")));
  const [a, b] = calls.terminals.slice(-2);
  assert.notEqual(a, b);
  assert.deepEqual(a.writes[0], Uint8Array.from(Buffer.from("first\x1b]52;unfinished")));
  assert.deepEqual(b.writes[0], Uint8Array.from(Buffer.from("\x1b[3Jsecond")));
  first();
  second();
});

test("restored history keeps at least the configured scrollback", async () => {
  const { openHistory, setLook } = await import("./engine.ts");
  setLook({
    fontFamily: "monospace", ligatures: false, fontSize: 12, lineHeight: 1,
    cursorStyle: "block", cursorBlink: false, padding: 0, scrollback: 40_000,
    optionAsMeta: false, themeId: null,
  }, {
    background: "#000", foreground: "#fff", cursor: "#fff", selection: "#888",
    ansi: Array(16).fill("#fff"),
  });
  const element = {
    offsetParent: null, style: {}, addEventListener() {}, removeEventListener() {},
  } as unknown as HTMLElement;
  const lines = Array.from({ length: 25_000 }, (_, i) => `line ${i}`);
  lines[0] = "FIRST-MARKER";
  lines[lines.length - 1] = "LAST-MARKER";
  const text = lines.join("\r\n") + "\r\n";
  assert.ok(text.length > 250_000);
  const close = await openHistory(element, Array.from(Buffer.from(text)));
  const terminal = calls.terminals.at(-1)!;
  assert.ok((terminal.options.scrollback as number) >= 40_000);
  const written = Buffer.from(terminal.writes[0] as Uint8Array).toString();
  assert.ok(written.startsWith("FIRST-MARKER"));
  assert.ok(written.endsWith("LAST-MARKER\r\n"));
  close();
});

test("a closed run gives its scroll line and selection, and opens at them again", async () => {
  const { openHistory } = await import("./engine.ts");
  const element = {
    offsetParent: null, style: {}, addEventListener() {}, removeEventListener() {},
  } as unknown as HTMLElement;
  const close = await openHistory(element, Array.from(Buffer.from("output")));
  const first = calls.terminals.at(-1)!;
  first.buffer.active = { viewportY: 12, baseY: 40 };
  first.selection = { start: { x: 70, y: 13 }, end: { x: 5, y: 15 } };
  const view = close();
  assert.deepEqual(view, { viewportY: 12, selection: { column: 70, row: 13, length: 2 * 80 + 5 - 70, cols: 80 } });

  const again = await openHistory(element, Array.from(Buffer.from("output")), view);
  const second = calls.terminals.at(-1)!;
  assert.equal(second.scrolledTo, 12);
  assert.deepEqual(second.selected, [70, 13, 95]);
  second.buffer.active = { viewportY: 40, baseY: 40 };
  second.cols = 100;
  second.selection = first.selection;
  const atBottom = again();
  assert.deepEqual(atBottom, { viewportY: null, selection: { column: 70, row: 13, length: 2 * 100 + 5 - 70, cols: 100 } });

  // A different width drops the selection, and the bottom stays the bottom.
  const third = await openHistory(element, Array.from(Buffer.from("output")), atBottom);
  const terminal = calls.terminals.at(-1)!;
  assert.equal(terminal.scrolledTo, undefined);
  assert.equal(terminal.selected, undefined);
  third();
});
