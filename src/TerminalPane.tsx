import { useEffect, useRef } from "react";
import type { FitAddon, Terminal } from "ghostty-web";
import { ackSession, attachSession, detachSession, resizeSession, SessionFrame, writeSession } from "./api";

let ghostty: Promise<typeof import("ghostty-web")> | undefined;

/** Loads ghostty-web and its WASM once, on the first terminal. */
function loadGhostty() {
  ghostty ??= import("ghostty-web").then(async (module) => {
    await module.init();
    return module;
  });
  return ghostty;
}

let fitted = { rows: 24, cols: 80 };

/** The size of the last terminal that fitted itself. A new session opens at this size. */
export const fittedSize = () => fitted;

/** The keys that Option sends with ESC first, besides letters and digits. */
const META_KEYS: Record<string, string> = {
  Backspace: "\x7f",
  Enter: "\r",
  Space: " ",
  Period: ".",
  Comma: ",",
  Slash: "/",
  Backslash: "\\",
  Minus: "-",
  Equal: "=",
  Semicolon: ";",
  Quote: "'",
  Backquote: "`",
  BracketLeft: "[",
  BracketRight: "]",
};

/**
 * Paste with bracketed-paste markers, and mouse reports to the PTY while the TUI asks for them. ghostty-web 0.4 sends
 * pasted text raw and handles the mouse only locally. Returns the cleanup.
 */
function tuiInput(t: Terminal, el: HTMLElement, send: (data: string) => void) {
  const surface = () => el.querySelector("canvas") ?? el;
  const cell = (e: MouseEvent) => {
    const r = surface().getBoundingClientRect();
    const clamp = (n: number, max: number) => Math.min(max, Math.max(1, n));
    return [
      clamp(Math.floor(((e.clientX - r.left) * t.cols) / r.width) + 1, t.cols),
      clamp(Math.floor(((e.clientY - r.top) * t.rows) / r.height) + 1, t.rows),
    ];
  };
  const report = (button: number, e: MouseEvent, release: boolean) => {
    const [x, y] = cell(e);
    const code = button | (e.shiftKey ? 4 : 0) | (e.altKey ? 8 : 0) | (e.ctrlKey ? 16 : 0);
    if (t.getMode(1006)) send(`\x1b[<${code};${x};${y}${release ? "m" : "M"}`);
    else if (x <= 223 && y <= 223) send(`\x1b[M${String.fromCharCode(32 + (release ? 3 : code), 32 + x, 32 + y)}`);
  };
  const take = (e: Event) => {
    e.preventDefault();
    e.stopImmediatePropagation();
  };
  let held = -1;
  const onPaste = (e: ClipboardEvent) => {
    const text = e.clipboardData?.getData("text/plain");
    if (!text) return;
    take(e);
    t.paste(text);
  };
  const onDown = (e: MouseEvent) => {
    if (!t.hasMouseTracking() || e.button > 2) return;
    take(e);
    held = e.button;
    report(held, e, false);
  };
  const onUp = (e: MouseEvent) => {
    if (held < 0) return;
    take(e);
    report(held, e, true);
    held = -1;
  };
  const onMove = (e: MouseEvent) => {
    if (!t.hasMouseTracking()) return;
    if (held >= 0 && (t.getMode(1002) || t.getMode(1003))) report(held + 32, e, false);
    else if (held < 0 && t.getMode(1003)) report(35, e, false);
  };
  const onWheel = (e: WheelEvent) => {
    if (!t.hasMouseTracking() || e.deltaY === 0) return;
    take(e);
    report(e.deltaY < 0 ? 64 : 65, e, false);
  };
  el.addEventListener("paste", onPaste, true);
  el.addEventListener("mousedown", onDown, true);
  window.addEventListener("mouseup", onUp, true);
  el.addEventListener("mousemove", onMove, true);
  el.addEventListener("wheel", onWheel, { capture: true, passive: false });
  return () => {
    el.removeEventListener("paste", onPaste, true);
    el.removeEventListener("mousedown", onDown, true);
    window.removeEventListener("mouseup", onUp, true);
    el.removeEventListener("mousemove", onMove, true);
    el.removeEventListener("wheel", onWheel, true);
  };
}

/**
 * The input of a key that agent TUIs need and ghostty-web does not send: Shift+Enter is ESC CR (a new line in the
 * prompt), and Option acts as Meta (ESC, then the key; macOS would type a special character). `undefined`: ghostty-web
 * handles the key.
 */
function agentKey(event: KeyboardEvent): string | undefined {
  if (event.metaKey || event.ctrlKey) return;
  if (event.key === "Enter" && event.shiftKey && !event.altKey) return "\x1b\r";
  if (!event.altKey) return;
  const { code } = event;
  if (code.startsWith("Key")) {
    const letter = code.slice(3).toLowerCase();
    return `\x1b${event.shiftKey ? letter.toUpperCase() : letter}`;
  }
  if (code.startsWith("Digit")) return `\x1b${code.slice(5)}`;
  if (code in META_KEYS) return `\x1b${META_KEYS[code]}`;
}

interface Props {
  sessionId: string;
  active: boolean;
  onExit: () => void;
  onError: (error: unknown) => void;
}

/** Attaches to the session while it is mounted. An unmount detaches; the session keeps running. */
export function TerminalPane({ sessionId, active, onExit, onError }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal>(null);
  const fitter = useRef<FitAddon>(null);

  useEffect(() => {
    let disposed = false;
    let term: Terminal | undefined;
    let stopInput: (() => void) | undefined;
    let token: number | undefined;
    /** The frames written before the attach resolved to its token. */
    let unacked = 0;
    const send = (data: string) => writeSession(sessionId, data).catch(() => {});
    const ack = () => {
      if (disposed) return;
      if (token === undefined) unacked++;
      else ackSession(sessionId, token).catch(() => {});
    };
    loadGhostty()
      .then(async ({ FitAddon, Terminal }) => {
        if (disposed) return;
        term = new Terminal({
          fontSize: 13,
          fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
          theme: { background: "#0b1120", foreground: "#e5e7eb" },
        });
        const fit = new FitAddon();
        term.loadAddon(fit);
        term.open(container.current!);
        fit.fit();
        fit.observeResize();
        terminal.current = term;
        fitter.current = fit;
        const t = term;
        // Returns true when the pane sent the key itself.
        t.attachCustomKeyEventHandler((event) => {
          const text = agentKey(event);
          if (text === undefined) return false;
          send(text);
          return true;
        });
        t.onData(send);
        stopInput = tuiInput(t, container.current!, send);
        t.onResize(({ rows, cols }) => {
          if (!container.current?.hidden) fitted = { rows, cols };
          resizeSession(sessionId, rows, cols).catch(() => {});
        });
        const onFrame = (frame: SessionFrame) => {
          if (disposed) return;
          if (!(frame instanceof ArrayBuffer || Array.isArray(frame))) return onExit();
          // The parse is synchronous. The write callback waits for an animation frame, which a hidden window does not get.
          t.write(frame instanceof ArrayBuffer ? new Uint8Array(frame) : Uint8Array.from(frame));
          ack();
        };
        token = await attachSession(sessionId, onFrame);
        if (disposed) return detachSession(sessionId, token).catch(() => {});
        for (; unacked > 0; unacked--) ackSession(sessionId, token).catch(() => {});
        // After the replay, so the TUI draws itself again at this size. An exited session takes no input.
        fit.fit();
        await resizeSession(sessionId, t.rows, t.cols).catch(() => {});
        if (!disposed && active) {
          fitted = { rows: t.rows, cols: t.cols };
          t.focus();
        }
      })
      .catch(onError);
    return () => {
      disposed = true;
      stopInput?.();
      if (token !== undefined) detachSession(sessionId, token).catch(() => {});
      term?.dispose();
    };
  }, []);

  // A hidden pane cannot measure itself, so it fits when it shows.
  useEffect(() => {
    if (!active || !terminal.current) return;
    fitter.current?.fit();
    fitted = { rows: terminal.current.rows, cols: terminal.current.cols };
    terminal.current.focus();
  }, [active]);

  return <div ref={container} className="terminal" hidden={!active} />;
}
