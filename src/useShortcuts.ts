import { useEffect } from "react";

export const SHORTCUTS = [
  { key: "n", label: "⌘N", action: "newShield", text: "New shield" },
  { key: "t", label: "⌘T", action: "newShell", text: "New shell" },
  { key: "p", label: "⌘P", action: "newPi", text: "New pi agent" },
  { key: "w", label: "⌘W", action: "close", text: "Close session" },
  { key: "]", label: "⌘]", action: "next", text: "Next session" },
  { key: "[", label: "⌘[", action: "previous", text: "Previous session" },
] as const;

export type ShortcutAction = (typeof SHORTCUTS)[number]["action"];

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
