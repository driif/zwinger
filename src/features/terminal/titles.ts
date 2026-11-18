import { useSyncExternalStore } from "react";
import type { Terminal } from "@xterm/xterm";

/** The window title (OSC 0 or OSC 2) of each running process, by process id. */
let titles: Record<string, string> = {};
/** The binding that may set the title of each process. A newer binding of the same process replaces an older one. */
const owners = new Map<string, object>();
const listeners = new Set<() => void>();

function setTitle(processId: string, title: string) {
  if (titles[processId] === (title || undefined)) return;
  const { [processId]: _, ...rest } = titles;
  titles = title ? { ...rest, [processId]: title } : rest;
  listeners.forEach((listener) => listener());
}

/**
 * Keeps the title that the process sets in `term`. The returned function ends the title, for example at the exit of the process.
 * It ignores later titles, can run again, and keeps the title of a newer binding of the same process.
 * A function of its own, so the test drives it with a real xterm.
 */
export function followTitle(term: Pick<Terminal, "onTitleChange">, processId: string) {
  const binding = {};
  owners.set(processId, binding);
  const listener = term.onTitleChange((title) => owners.get(processId) === binding && setTitle(processId, title.trim()));
  return () => {
    listener.dispose();
    if (owners.get(processId) !== binding) return;
    owners.delete(processId);
    setTitle(processId, "");
  };
}

export const terminalTitles = () => titles;

/** The titles of the running processes. A new title renders again. */
export function useTerminalTitles() {
  return useSyncExternalStore((listener) => {
    listeners.add(listener);
    return () => listeners.delete(listener);
  }, terminalTitles);
}
