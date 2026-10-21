import { useEffect } from "react";

export const SHORTCUTS = [
  { key: "k", label: "⌘K", action: "commands" },
  { key: "n", label: "⌘N", action: "newShield" },
  { key: "t", label: "⌘T", action: "newShell" },
  { key: "p", label: "⌘P", action: "newPi" },
  { key: "w", label: "⌘W", action: "close" },
  { key: "]", label: "⌘]", action: "next" },
  { key: "[", label: "⌘[", action: "previous" },
  { key: "1", label: "⌘1", action: "overview" },
  { key: "2", label: "⌘2", action: "terminal" },
  { key: "3", label: "⌘3", action: "network" },
  { key: "4", label: "⌘4", action: "settings" },
] as const;

export type ShortcutAction = (typeof SHORTCUTS)[number]["action"];

export const shortcutLabel = (action: ShortcutAction) => SHORTCUTS.find((s) => s.action === action)?.label;

export function useShortcuts(handlers: Record<ShortcutAction, () => void>) {
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      const shortcut = SHORTCUTS.find((s) => s.key === event.key);
      if (!event.metaKey || event.shiftKey || event.altKey || !shortcut) return;
      event.preventDefault();
      event.stopPropagation();
      handlers[shortcut.action]();
    }
    // Capture phase, so the shortcuts work while the terminal has focus.
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  });
}
