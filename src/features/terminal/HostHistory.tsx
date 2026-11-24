import { useEffect, useRef, useState } from "react";
import { HistoryView, openHistory } from "./engine";

/**
 * A passive view of saved Mac shell output. It has no process connection. Its xterm exists only while the run is near
 * the visible part of the pane; the run keeps its height, scroll line and selection meanwhile.
 */
export function HostHistory({ bytes }: { bytes: number[] }) {
  const container = useRef<HTMLDivElement>(null);
  const [near, setNear] = useState(false);
  const saved = useRef<{ bytes: number[]; view: HistoryView }>(undefined);

  useEffect(() => {
    const el = container.current!;
    const observer = new IntersectionObserver(([entry]) => setNear(entry.isIntersecting), {
      root: el.parentElement,
      rootMargin: "50% 0px",
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (!near) return;
    let disposed = false;
    let close: (() => HistoryView) | undefined;
    const view = saved.current?.bytes === bytes ? saved.current.view : undefined;
    try {
      openHistory(container.current!, bytes, view).then((opened) => {
        if (disposed) opened();
        else close = opened;
      }).catch(() => {});
    } catch {
      // A history render failure must not affect the live terminal.
    }
    return () => {
      disposed = true;
      if (close) saved.current = { bytes, view: close() };
    };
  }, [bytes, near]);

  return <div ref={container} className="terminal host-history" aria-label="Previous Mac shell output" />;
}
