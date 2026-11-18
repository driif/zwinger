import type { ITheme, Terminal } from "@xterm/xterm";
import type { ISearchOptions, SearchAddon } from "@xterm/addon-search";
import type { WebglAddon } from "@xterm/addon-webgl";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { TerminalColors } from "../appearance/api";
import type { TerminalPrefs } from "../settings/api";

/** The terminal settings and colors that all terminals use. */
interface Look {
  prefs: TerminalPrefs;
  colors: TerminalColors;
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

/** Restyles all open terminals. A new terminal takes the look too. */
export function setLook(prefs: TerminalPrefs, colors: TerminalColors) {
  const key = JSON.stringify([prefs, colors]);
  if (key === lookKey) return;
  lookKey = key;
  look = { prefs, colors };
  for (const entry of live) {
    // This runs during render: a value that xterm refuses must not blank the app.
    try {
      Object.assign(entry.term.options, options(look));
    } catch {
      // The terminal keeps its old options.
    }
    style(entry, look);
    entry.fit();
  }
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

/**
 * Opens an xterm in `el`. It fits itself when `el` changes size. xterm.js loads on the first terminal.
 * ⌘F in the terminal calls `onFind` and does not go to the shell.
 */
export async function openTerminal(el: HTMLElement, onFind: () => void): Promise<Engine> {
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
    if (!frozen) return;
    const { deltaX, deltaY } = event;
    const down = deltaY > 0 && el.scrollTop + el.clientHeight < el.scrollHeight;
    const up = deltaY < 0 && el.scrollTop > 0;
    const right = deltaX > 0 && el.scrollLeft + el.clientWidth < el.scrollWidth;
    const left = deltaX < 0 && el.scrollLeft > 0;
    if (!(down || up || right || left)) return;
    // A line or page delta is rare. A cell is about 16 pixels.
    const unit = event.deltaMode === WheelEvent.DOM_DELTA_PIXEL ? 1 : event.deltaMode === WheelEvent.DOM_DELTA_LINE ? 16 : el.clientHeight;
    el.scrollBy(deltaX * unit, deltaY * unit);
    event.preventDefault();
    event.stopPropagation();
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
  const entry: Live = { term, el, fit: fitNow };
  style(entry, look!);
  live.add(entry);
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
      live.delete(entry);
      releaseWebgl();
      term.dispose();
    },
  };
}
