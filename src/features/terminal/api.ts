import { Channel } from "@tauri-apps/api/core";
import { call, PaneNode, TerminalKind } from "../../shared/api";
import type { CursorStyle } from "../settings/api";

/** Saves the split tree of the session's terminal view. */
export const setPaneLayout = (sessionId: string, layout: PaneNode | null) =>
  call<void>("set_pane_layout", { sessionId, layout });

/** The colors of a Ghostty config as `#rrggbb`. `ansi` has 16 entries, `null` where the config sets none. */
export interface GhosttyPalette {
  background: string | null;
  foreground: string | null;
  cursor: string | null;
  selection: string | null;
  ansi: (string | null)[];
}

/** The terminal settings that the config sets, its colors, and the keys that zwinger does not use. */
export interface GhosttyImport {
  settings: Partial<{
    fontFamily: string;
    fontSize: number;
    lineHeight: number;
    cursorStyle: CursorStyle;
    cursorBlink: boolean;
    padding: number;
    scrollback: number;
    optionAsMeta: boolean;
  }>;
  palette: GhosttyPalette | null;
  ignored: string[];
}

/** Reads the user's Ghostty config files. Fails if there is none. */
export const importGhosttyConfig = () => call<GhosttyImport>("import_ghostty_config");

/** Output bytes, or the exit. Tauri can send small raw bodies as a number array. */
export type SessionFrame = ArrayBuffer | number[] | { exit: number };

export const reorderTerminals = (sessionId: string, terminalIds: string[]) =>
  call<void>("reorder_terminals", { sessionId, terminalIds });

/**
 * Sends the kept output of the process, and its exit if it has exited, then new output. A later attach takes over.
 * Resolves to the token for `detachSession` and `ackSession`.
 */
export function attachSession(
  id: string,
  onFrame: (frame: SessionFrame) => void,
) {
  return call<number>("attach_session", {
    id,
    onEvent: new Channel<SessionFrame>(onFrame),
  });
}

/** Stops the output to the attach of `token`. A later attach keeps its output. */
export const detachSession = (id: string, token: number) =>
  call<void>("detach_session", { id, token });

/** The attach of `token` has written one more output frame. The terminal holds its process while frames wait. */
export const ackSession = (id: string, token: number) =>
  call<void>("ack_session", { id, token });

/** Adds a terminal to the end of the session's list and opens it. Resolves to the process id. */
export const openSession = (
  sessionId: string,
  kind: TerminalKind,
  rows: number,
  cols: number,
) => call<string>("open_session", { sessionId, kind, rows, cols });

/** Opens a dormant or exited terminal again, under the same id. Resolves to the process id. */
export const reopenTerminal = (
  sessionId: string,
  terminalId: string,
  rows: number,
  cols: number,
) => call<string>("reopen_terminal", { sessionId, terminalId, rows, cols });

export const writeSession = (id: string, data: string) =>
  call<void>("write_session", { id, data });

export const resizeSession = (id: string, rows: number, cols: number) =>
  call<void>("resize_session", { id, rows, cols });

/** Removes the terminal and ends its process. */
export const closeTerminal = (sessionId: string, terminalId: string) =>
  call<void>("close_terminal", { sessionId, terminalId });
