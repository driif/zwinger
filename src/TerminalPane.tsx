import { useEffect, useRef } from "react";
import { FitAddon, init, Terminal } from "ghostty-web";
import { closeSession, openSession, resizeSession, SessionKind, writeSession } from "./api";

const ready = init();

interface Props {
  shieldId: string;
  kind: SessionKind;
  active: boolean;
  onOpen: (sessionId: string) => void;
  onExit: () => void;
  onError: (error: unknown) => void;
}

export function TerminalPane({ shieldId, kind, active, onOpen, onExit, onError }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal>(null);

  // Runs once per pane: the pane owns its terminal and its session for its whole life.
  useEffect(() => {
    let disposed = false;
    let term: Terminal | undefined;
    ready
      .then(async () => {
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
        const t = term;
        const id = await openSession(shieldId, kind, t.rows, t.cols, (event) =>
          event.event === "output" ? t.write(new Uint8Array(event.data)) : onExit(),
        );
        if (disposed) return closeSession(id);
        t.onData((data) => writeSession(id, data).catch(() => {}));
        t.onResize(({ rows, cols }) => resizeSession(id, rows, cols).catch(() => {}));
        t.focus();
        onOpen(id);
      })
      .catch(onError);
    return () => {
      disposed = true;
      term?.dispose();
    };
  }, []);

  useEffect(() => {
    if (active) terminal.current?.focus();
  }, [active]);

  return <div ref={container} className="terminal" hidden={!active} />;
}
