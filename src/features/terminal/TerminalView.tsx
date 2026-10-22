import { CSSProperties, useEffect, useRef, useState } from "react";
import { ackSession, attachSession, detachSession, resizeSession, SessionFrame, writeSession } from "./api";
import { Engine, openTerminal } from "./engine";
import "./terminal.css";

interface Props {
  sessionId: string;
  visible: boolean;
  /** Takes the keyboard focus while it is visible. */
  focused: boolean;
  /** The place in the pane layout. No style fills the parent. */
  style?: CSSProperties;
  onFocus?: () => void;
  onExit: () => void;
  onError: (error: unknown) => void;
}

/** Attaches to the session while it is mounted. An unmount detaches; the session keeps running. */
export function TerminalView({ sessionId, visible, focused, style, onFocus, onExit, onError }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const [engine, setEngine] = useState<Engine>();

  useEffect(() => {
    let disposed = false;
    let opened: Engine | undefined;
    let token: number | undefined;
    /** The frames written before the attach resolved to its token. */
    let unacked = 0;
    // Input that does not arrive is reported once, not at each key.
    let reported = false;
    const fail = (e: unknown) => {
      if (reported || disposed) return;
      reported = true;
      onError(e);
    };
    const ack = () => {
      if (disposed) return;
      if (token === undefined) unacked++;
      else ackSession(sessionId, token).catch(fail);
    };
    openTerminal(container.current!)
      .then(async (created) => {
        if (disposed) return created.dispose();
        opened = created;
        setEngine(created);
        const { term } = created;
        term.onData((data) => writeSession(sessionId, data).catch(fail));
        term.onResize(({ rows, cols }) => resizeSession(sessionId, rows, cols).catch(() => {}));
        const onFrame = (frame: SessionFrame) => {
          if (disposed) return;
          if (!(frame instanceof ArrayBuffer || Array.isArray(frame))) return onExit();
          // The parse is synchronous. The write callback waits for an animation frame, which a hidden window does not get.
          term.write(frame instanceof ArrayBuffer ? new Uint8Array(frame) : Uint8Array.from(frame));
          ack();
        };
        token = await attachSession(sessionId, onFrame);
        if (disposed) return detachSession(sessionId, token).catch(() => {});
        for (; unacked > 0; unacked--) ackSession(sessionId, token).catch(fail);
        // After the replay, so the TUI draws itself again at this size. An exited session takes no input.
        created.fit();
        await resizeSession(sessionId, term.rows, term.cols).catch(() => {});
      })
      .catch(onError);
    return () => {
      disposed = true;
      if (token !== undefined) detachSession(sessionId, token).catch(() => {});
      opened?.dispose();
    };
  }, []);

  useEffect(() => {
    engine?.setVisible(visible);
  }, [engine, visible]);

  useEffect(() => {
    if (visible && focused) engine?.term.focus();
  }, [engine, visible, focused]);

  return <div ref={container} className="terminal" hidden={!visible} style={style} onFocus={onFocus} />;
}
