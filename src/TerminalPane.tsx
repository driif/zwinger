import { useEffect, useRef } from "react";
import { FitAddon, init, Terminal } from "ghostty-web";
import { attachApp, closeSession, detachApp, openSession, PtyEvent, resizeSession, SessionKind, writeSession } from "./api";

const ready = init();

interface Props {
  /** A new session that the pane owns, or the run of an app, which the backend owns. */
  session: { shieldId: string; kind: SessionKind } | { attach: string };
  active: boolean;
  onOpen?: (sessionId: string) => void;
  onExit: () => void;
  onError: (error: unknown) => void;
}

export function TerminalPane({ session, active, onOpen, onExit, onError }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal>(null);

  // Runs once per pane: the pane owns its terminal, and its session unless it attaches, for its whole life.
  useEffect(() => {
    let disposed = false;
    let term: Terminal | undefined;
    let detach: (() => void) | undefined;
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
        const onEvent = (event: PtyEvent) => {
          if (disposed) return;
          if (event.event === "output") t.write(new Uint8Array(event.data));
          else onExit();
        };
        let id: string;
        if ("attach" in session) {
          id = session.attach;
          const token = await attachApp(id, onEvent);
          detach = () => detachApp(id, token).catch(() => {});
          if (disposed) return detach();
          // An exited run takes no input.
          await resizeSession(id, t.rows, t.cols).catch(() => {});
          if (disposed) return;
        } else {
          id = await openSession(session.shieldId, session.kind, t.rows, t.cols, onEvent);
          if (disposed) return closeSession(id);
        }
        t.onData((data) => writeSession(id, data).catch(() => {}));
        t.onResize(({ rows, cols }) => resizeSession(id, rows, cols).catch(() => {}));
        t.focus();
        onOpen?.(id);
      })
      .catch(onError);
    return () => {
      disposed = true;
      detach?.();
      term?.dispose();
    };
  }, []);

  useEffect(() => {
    if (active) terminal.current?.focus();
  }, [active]);

  return <div ref={container} className="terminal" hidden={!active} />;
}
