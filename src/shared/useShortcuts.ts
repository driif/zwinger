import { useEffect } from "react";

interface Shortcut {
  /** `KeyboardEvent.key` in lowercase. */
  key: string;
  label: string;
  action: string;
  shift?: boolean;
  alt?: boolean;
  /** Works only while a terminal has the focus. */
  terminal?: boolean;
}

export const SHORTCUTS = [
  { key: "k", label: "⌘K", action: "commands" },
  { key: "n", label: "⌘N", action: "newProject" },
  { key: "t", label: "⌘T", action: "newShell" },
  { key: "t", label: "⌘⇧T", action: "newVmShell", shift: true },
  { key: "e", label: "⌘⇧E", action: "openEditor", shift: true },
  { key: "p", label: "⌘P", action: "newAgent" },
  { key: "w", label: "⌘W", action: "close" },
  { key: "]", label: "⌘]", action: "next" },
  { key: "[", label: "⌘[", action: "previous" },
  { key: "1", label: "⌘1", action: "overview" },
  { key: "2", label: "⌘2", action: "agents" },
  { key: "3", label: "⌘3", action: "terminal" },
  { key: "4", label: "⌘4", action: "network" },
  { key: "5", label: "⌘5", action: "settings" },
  { key: ",", label: "⌘,", action: "appSettings" },
  { key: "d", label: "⌘D", action: "splitRight", terminal: true },
  { key: "d", label: "⌘⇧D", action: "splitDown", shift: true, terminal: true },
  { key: "arrowleft", label: "⌘⌥←", action: "focusLeft", alt: true, terminal: true },
  { key: "arrowright", label: "⌘⌥→", action: "focusRight", alt: true, terminal: true },
  { key: "arrowup", label: "⌘⌥↑", action: "focusUp", alt: true, terminal: true },
  { key: "arrowdown", label: "⌘⌥↓", action: "focusDown", alt: true, terminal: true },
] as const satisfies readonly Shortcut[];

export type ShortcutAction = (typeof SHORTCUTS)[number]["action"];

export const shortcutLabel = (action: ShortcutAction) => SHORTCUTS.find((s) => s.action === action)?.label;

export function useShortcuts(handlers: Record<ShortcutAction, () => void>) {
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      // Only ⌘ chords: Ctrl keys and the other Option and Shift chords go to the terminal and its agent.
      if (!event.metaKey || event.ctrlKey) return;
      const inTerminal = (event.target as Element).closest?.(".terminal") != null;
      const shortcut = (SHORTCUTS as readonly Shortcut[]).find(
        (s) =>
          s.key === event.key.toLowerCase() &&
          !!s.shift === event.shiftKey &&
          !!s.alt === event.altKey &&
          (!s.terminal || inTerminal),
      );
      if (!shortcut) return;
      event.preventDefault();
      event.stopPropagation();
      handlers[shortcut.action as ShortcutAction]();
    }
    // Capture phase, so the shortcuts work while the terminal has focus.
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  });
}
