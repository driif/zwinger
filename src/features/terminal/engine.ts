import type { ITheme, Terminal } from "@xterm/xterm";
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
  /** Fits the terminal to its element. A hidden element keeps the size. */
  fit: () => void;
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

let xterm: Promise<typeof import("./xterm")> | undefined;

/** Opens an xterm in `el`. It fits itself when `el` changes size. xterm.js loads on the first terminal. */
export async function openTerminal(el: HTMLElement): Promise<Engine> {
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
  term.loadAddon(new SearchAddon());
  // The webview does not open windows; the system browser opens the link.
  term.loadAddon(new WebLinksAddon((_, uri) => openUrl(uri)));
  // Shift+Enter is ESC CR: a new line in the prompt of an agent TUI.
  term.attachCustomKeyEventHandler((event) => {
    if (event.key !== "Enter" || !event.shiftKey || event.altKey || event.ctrlKey || event.metaKey) return true;
    if (event.type === "keydown") term.input("\x1b\r");
    return false;
  });
  term.open(el);
  const fitNow = () => {
    if (el.offsetParent === null) return;
    fit.fit();
    fitted = { rows: term.rows, cols: term.cols };
  };
  const entry: Live = { term, el, fit: fitNow };
  style(entry, look!);
  live.add(entry);
  const observer = new ResizeObserver(fitNow);
  observer.observe(el);

  let webgl: WebglAddon | undefined;
  return {
    term,
    fit: fitNow,
    setVisible(visible) {
      if (visible && !webgl) {
        try {
          const addon = new WebglAddon();
          // A lost context goes back to the DOM renderer.
          addon.onContextLoss(() => {
            addon.dispose();
            if (webgl === addon) webgl = undefined;
          });
          term.loadAddon(addon);
          webgl = addon;
        } catch {
          // No WebGL: the DOM renderer draws.
        }
      } else if (!visible && webgl) {
        webgl.dispose();
        webgl = undefined;
      }
    },
    dispose() {
      observer.disconnect();
      live.delete(entry);
      term.dispose();
    },
  };
}
