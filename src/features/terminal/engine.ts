import type { ITheme, Terminal } from "@xterm/xterm";
import type { ISearchOptions, SearchAddon } from "@xterm/addon-search";
import type { WebglAddon } from "@xterm/addon-webgl";
import type { LigaturesAddon } from "@xterm/addon-ligatures";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { TerminalColors } from "../appearance/api";
import type { TerminalPrefs } from "../settings/api";
import { openTerminalLocation, terminalLocation, type OpenedLocation } from "./api";
import { fileLinkProvider } from "./fileLinks";
import { NUMBERS } from "./settings.ts";

/** The terminal settings and colors that all terminals use. */
interface Look {
  prefs: TerminalPrefs;
  colors: TerminalColors;
}

/** The process whose paths in the output become file links, and where an open reports. */
export interface FileLinks {
  processId: string;
  onOpened: (opened: OpenedLocation) => void;
  onError: (error: unknown) => void;
}

/** An xterm in its element. A visible one draws with WebGL. */
export interface Engine {
  term: Terminal;
  search: SearchAddon;
  /** Fits the terminal to its element. A hidden element keeps the size. */
  fit: () => void;
  /** Resolves at the first fit, when the element is visible. */
  sized: Promise<void>;
  /** Keeps the size from now on. A process that has stopped cannot draw its screen again at a new size. */
  freeze: () => void;
  /** Only a visible terminal holds a WebGL context: a webview has about 16. */
  setVisible: (visible: boolean) => void;
  dispose: () => void;
}

interface Live {
  term: Terminal;
  el: HTMLElement;
  fit: () => void;
  historyScrollback?: number;
  ligatures?: LigaturesAddon;
  ligaturesGeneration: number;
}

let look: Look | undefined;
let lookKey = "";
const live = new Set<Live>();

let fitted = { rows: 24, cols: 80 };

/** The size of the last visible terminal that fitted itself. A new session opens at this size. */
export const fittedSize = () => fitted;

const ANSI_NAMES = [
  "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
  "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue", "brightMagenta", "brightCyan", "brightWhite",
] as const;

function options({ prefs, colors }: Look) {
  const theme: ITheme = {
    background: colors.background,
    foreground: colors.foreground,
    cursor: colors.cursor,
    cursorAccent: colors.background,
    selectionBackground: colors.selection,
  };
  ANSI_NAMES.forEach((name, i) => (theme[name] = colors.ansi[i]));
  return {
    fontFamily: prefs.fontFamily,
    fontSize: prefs.fontSize,
    // xterm throws for a line height below 1.
    lineHeight: Math.max(1, prefs.lineHeight),
    cursorStyle: prefs.cursorStyle,
    cursorBlink: prefs.cursorBlink,
    scrollback: prefs.scrollback,
    macOptionIsMeta: prefs.optionAsMeta,
    theme,
  };
}

function style({ el }: Live, { prefs, colors }: Look) {
  el.style.padding = `${prefs.padding}px`;
  el.style.background = colors.background;
}

async function setLigatures(entry: Live, enabled: boolean) {
  const generation = ++entry.ligaturesGeneration;
  entry.ligatures?.dispose();
  entry.ligatures = undefined;
  entry.term.refresh(0, entry.term.rows - 1);
  if (!enabled) return;

  try {
    const { LigaturesAddon } = await import("@xterm/addon-ligatures");
    if (generation !== entry.ligaturesGeneration || !look?.prefs.ligatures || !live.has(entry)) return;
    const addon = new LigaturesAddon();
    entry.term.loadAddon(addon);
    entry.ligatures = addon;
    entry.term.refresh(0, entry.term.rows - 1);
  } catch {
    // A failed addon load must not prevent the terminal from drawing text.
  }
}

/** Restyles all open terminals. A new terminal takes the look too. */
export function setLook(prefs: TerminalPrefs, colors: TerminalColors) {
  const key = JSON.stringify([prefs, colors]);
  if (key === lookKey) return;
  const ligaturesChanged = look?.prefs.ligatures !== prefs.ligatures;
  lookKey = key;
  look = { prefs, colors };
  for (const entry of live) {
    // This runs during render: a value that xterm refuses must not blank the app.
    try {
      const next = options(look);
      if (entry.historyScrollback) next.scrollback = Math.max(next.scrollback, entry.historyScrollback);
      Object.assign(entry.term.options, next);
    } catch {
      // The terminal keeps its old options.
    }
    style(entry, look);
    if (ligaturesChanged) void setLigatures(entry, prefs.ligatures);
    entry.fit();
  }
}

/** Fits all visible terminals to their elements, for example after a zoom change. */
export function fitAll() {
  for (const entry of live) entry.fit();
}

/** Outlines the matches in the terminal colors. The search addon reports its match count only with decorations. */
export function searchOptions(): ISearchOptions {
  const { foreground, cursor } = look!.colors;
  return {
    decorations: {
      matchBorder: foreground,
      matchOverviewRuler: foreground,
      activeMatchBorder: cursor,
      activeMatchColorOverviewRuler: cursor,
    },
  };
}

let xterm: Promise<typeof import("./xterm")> | undefined;

function scrollByWheel(el: HTMLElement, event: WheelEvent, deltaX: number) {
  const unit = event.deltaMode === WheelEvent.DOM_DELTA_PIXEL ? 1
    : event.deltaMode === WheelEvent.DOM_DELTA_LINE ? 16 : el.clientHeight;
  el.scrollBy(deltaX * unit, event.deltaY * unit);
  event.preventDefault();
  event.stopPropagation();
}

/** A run consumes its own scrollback before the wheel moves to the adjacent run. */
function scrollAdjacentRun(term: Terminal, pane: HTMLElement | null, event: WheelEvent) {
  if (!pane) return;
  const buffer = term.buffer.active;
  const down = event.deltaY > 0 && buffer.viewportY >= buffer.baseY
    && pane.scrollTop + pane.clientHeight < pane.scrollHeight;
  const up = event.deltaY < 0 && buffer.viewportY === 0 && pane.scrollTop > 0;
  if (down || up) scrollByWheel(pane, event, 0);
}

/**
 * Opens an xterm in `el`. It fits itself when `el` changes size. xterm.js loads on the first terminal.
 * ⌘F in the terminal calls `onFind` and does not go to the shell. ⌘-click on a `path:line` opens the file.
 */
export async function openTerminal(el: HTMLElement, onFind: () => void, links: FileLinks): Promise<Engine> {
  xterm ??= import("./xterm");
  const { Terminal, FitAddon, SearchAddon, Unicode11Addon, WebLinksAddon, WebglAddon } = await xterm;
  // The workspace sets the look as it renders, before its terminals mount.
  let term: Terminal;
  try {
    term = new Terminal({ allowProposedApi: true, ...options(look!) });
  } catch {
    // A value that xterm refuses gives the xterm defaults, not a blank pane.
    term = new Terminal({ allowProposedApi: true, theme: options(look!).theme });
  }
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new Unicode11Addon());
  term.unicode.activeVersion = "11";
  const search = new SearchAddon();
  term.loadAddon(search);
  // The webview does not open windows; the system browser opens the link.
  term.loadAddon(new WebLinksAddon((_, uri) => openUrl(uri)));
  const { processId } = links;
  const files = fileLinkProvider(
    term,
    (path) => terminalLocation(processId, path),
    ({ path, line, col }) => openTerminalLocation(processId, path, line, col).then(links.onOpened, links.onError),
  );
  const fileProvider = term.registerLinkProvider(files);
  // Shift+Enter is ESC CR: a new line in the prompt of an agent TUI.
  term.attachCustomKeyEventHandler((event) => {
    if (event.key.toLowerCase() === "f" && event.metaKey && !event.shiftKey && !event.altKey && !event.ctrlKey) {
      if (event.type === "keydown") {
        event.preventDefault();
        onFind();
      }
      return false;
    }
    if (event.key !== "Enter" || !event.shiftKey || event.altKey || event.ctrlKey || event.metaKey) return true;
    if (event.type === "keydown") term.input("\x1b\r");
    return false;
  });
  term.open(el);
  let frozen = false;
  // A frozen grid can be larger than its pane. The wheel scrolls the pane first, before xterm takes it.
  const scrollPane = (event: WheelEvent) => {
    if (frozen) {
      const { deltaX, deltaY } = event;
      const down = deltaY > 0 && el.scrollTop + el.clientHeight < el.scrollHeight;
      const up = deltaY < 0 && el.scrollTop > 0;
      const right = deltaX > 0 && el.scrollLeft + el.clientWidth < el.scrollWidth;
      const left = deltaX < 0 && el.scrollLeft > 0;
      if (down || up || right || left) {
        scrollByWheel(el, event, deltaX);
        return;
      }
    }
    if (term.buffer.active.type === "normal" && term.modes.mouseTrackingMode === "none") {
      scrollAdjacentRun(term, el.closest<HTMLElement>(".host-pane.live"), event);
    }
  };
  el.addEventListener("wheel", scrollPane, { capture: true, passive: false });
  let markSized!: () => void;
  const sized = new Promise<void>((resolve) => (markSized = resolve));
  const fitNow = () => {
    if (el.offsetParent === null || frozen) return;
    fit.fit();
    markSized();
    fitted = { rows: term.rows, cols: term.cols };
  };
  const entry: Live = { term, el, fit: fitNow, ligaturesGeneration: 0 };
  style(entry, look!);
  live.add(entry);
  if (look!.prefs.ligatures) void setLigatures(entry, true);
  const observer = new ResizeObserver(fitNow);
  observer.observe(el);

  let webgl: WebglAddon | undefined;
  function releaseWebgl() {
    if (!webgl) return;
    // xterm appends its WebGL canvas after the 2D link layer. Its addon detaches
    // that canvas on disposal but leaves the context live until garbage collection.
    const canvas = el.querySelector<HTMLCanvasElement>(".xterm-screen > canvas:last-of-type");
    const gl = canvas?.getContext("webgl2");
    webgl.dispose();
    webgl = undefined;
    if (gl && !gl.isContextLost()) gl.getExtension("WEBGL_lose_context")?.loseContext();
  }
  return {
    term,
    search,
    fit: fitNow,
    sized,
    freeze() {
      frozen = true;
      el.classList.add("frozen");
    },
    setVisible(visible) {
      if (visible && !webgl) {
        try {
          webgl = new WebglAddon();
          webgl.onContextLoss(releaseWebgl);
          term.loadAddon(webgl);
        } catch {
          releaseWebgl();
        }
      } else if (!visible) {
        releaseWebgl();
      }
    },
    dispose() {
      markSized();
      observer.disconnect();
      el.removeEventListener("wheel", scrollPane, { capture: true });
      fileProvider.dispose();
      entry.ligaturesGeneration++;
      entry.ligatures?.dispose();
      live.delete(entry);
      releaseWebgl();
      term.dispose();
    },
  };
}

/** Opens saved output without any input, link, title or clipboard path. */
export async function openHistory(el: HTMLElement, bytes: number[]): Promise<() => void> {
  xterm ??= import("./xterm");
  const { Terminal, FitAddon, Unicode11Addon } = await xterm;
  const lookOptions = options(look!);
  const lines = bytes.reduce((count, byte) => count + Number(byte === 10), 0);
  const estimate = lines + Math.ceil(bytes.length / 80) + 24;
  const historyScrollback = Math.max(lookOptions.scrollback, Math.min(estimate, NUMBERS.scrollback.max));
  const term = new Terminal({ allowProposedApi: true, ...lookOptions, scrollback: historyScrollback, disableStdin: true, cursorBlink: false });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new Unicode11Addon());
  term.unicode.activeVersion = "11";
  const ignored = () => true;
  const handlers = [0, 1, 2, 8, 52].map((code) => term.parser.registerOscHandler(code, ignored));
  term.open(el);
  const fitNow = () => {
    if (el.offsetParent !== null) fit.fit();
  };
  const entry: Live = { term, el, fit: fitNow, historyScrollback, ligaturesGeneration: 0 };
  style(entry, look!);
  live.add(entry);
  if (look!.prefs.ligatures) void setLigatures(entry, true);
  const observer = new ResizeObserver(fitNow);
  observer.observe(el);
  fitNow();
  const leaveHistory = (event: WheelEvent) => scrollAdjacentRun(term, el.parentElement, event);
  el.addEventListener("wheel", leaveHistory, { capture: true, passive: false });
  term.write(Uint8Array.from(bytes));
  term.write("\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?25l");
  return () => {
    observer.disconnect();
    el.removeEventListener("wheel", leaveHistory, { capture: true });
    handlers.forEach((handler) => handler.dispose());
    entry.ligaturesGeneration++;
    entry.ligatures?.dispose();
    live.delete(entry);
    term.dispose();
  };
}
