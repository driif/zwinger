import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { zoom } from "../settings/api.ts";
import { fitAll } from "../terminal/engine.ts";

/** The View menu has ⌘=, ⌘− and ⌘0. ⌘+ is ⌘⇧= and has no menu item, so it is handled here. */
export const isZoomInKey = (event: KeyboardEvent) =>
  event.metaKey && !event.ctrlKey && !event.altKey && event.key === "+";

/** Handles ⌘+, and fits all terminals again after each zoom change. */
export function useZoom() {
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (!isZoomInKey(event)) return;
      event.preventDefault();
      event.stopPropagation();
      zoom("in").catch(() => {});
    }
    // Capture phase, so the key works while a terminal has the focus.
    window.addEventListener("keydown", onKeyDown, true);
    // The new cell size in device pixels is known only after the next frame.
    const stop = listen<number>("zoom-changed", () => requestAnimationFrame(fitAll));
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
      void stop.then((unlisten) => unlisten());
    };
  }, []);
}
