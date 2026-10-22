import { useEffect } from "react";

export const SHORTCUTS = [
  { key: "k", label: "⌘K", action: "commands" },
  { key: "n", label: "⌘N", action: "newShield" },
  { key: "t", label: "⌘T", action: "newShell" },
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
] as const;

export type ShortcutAction = (typeof SHORTCUTS)[number]["action"];

export const shortcutLabel = (action: ShortcutAction) => SHORTCUTS.find((s) => s.action === action)?.label;

export function useShortcuts(handlers: Record<ShortcutAction, () => void>) {
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      // Only plain ⌘ chords: Ctrl, Option and Shift keys go to the terminal and its agent.
      if (!event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
      const shortcut = SHORTCUTS.find((s) => s.key === event.key);
      if (!shortcut) return;
      event.preventDefault();
      event.stopPropagation();
      handlers[shortcut.action]();
    }
    // Capture phase, so the shortcuts work while the terminal has focus.
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  });
}
