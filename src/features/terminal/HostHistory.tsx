import { useEffect, useRef } from "react";
import { openHistory } from "./engine";

/** A passive view of saved Mac shell output. It has no process connection. */
export function HostHistory({ bytes }: { bytes: number[] }) {
  const container = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let disposed = false;
    let close: (() => void) | undefined;
    try {
      openHistory(container.current!, bytes).then((opened) => {
        if (disposed) opened();
        else close = opened;
      }).catch(() => {});
    } catch {
      // A history render failure must not affect the live terminal.
    }
    return () => {
      disposed = true;
      close?.();
    };
  }, [bytes]);

  return <div ref={container} className="terminal host-history" aria-label="Previous Mac shell output" />;
}
