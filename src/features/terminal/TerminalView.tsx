import { IconButton, TextField } from "@driif/motiv";
import { ChevronDown, ChevronUp, X } from "lucide-react";
import { CSSProperties, useEffect, useRef, useState } from "react";
import { ackSession, attachSession, detachSession, resizeSession, SessionFrame, writeSession } from "./api";
import { Engine, openTerminal, searchOptions } from "./engine";
import { followTitle } from "./titles";
import "./terminal.css";

interface Props {
  processId: string;
  visible: boolean;
  /** Takes the keyboard focus while it is visible. */
  focused: boolean;
  /** Each change gives the keyboard focus to the focused terminal again. */
  focusRequest: number;
  /** The place in the pane layout. No style fills the parent. */
  style?: CSSProperties;
  onFocus?: () => void;
  onExit: (processId: string, code: number) => void;
  onError: (error: unknown) => void;
}

/** Attaches to the terminal process while it is mounted. An unmount detaches; the process keeps running. */
export function TerminalView({ processId, visible, focused, focusRequest, style, onFocus, onExit, onError }: Props) {
  const pane = useRef<HTMLDivElement>(null);
  const container = useRef<HTMLDivElement>(null);
  const field = useRef<HTMLInputElement>(null);
  const [engine, setEngine] = useState<Engine>();
  /** 0: the search is closed. Each ⌘F counts up and focuses the field again. */
  const [finding, setFinding] = useState(0);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<{ resultIndex: number; resultCount: number }>();

  useEffect(() => {
    let disposed = false;
    let opened: Engine | undefined;
    let unfollow: (() => void) | undefined;
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
      else ackSession(processId, token).catch(fail);
    };
    openTerminal(container.current!, () => setFinding((n) => n + 1))
      .then(async (created) => {
        if (disposed) return created.dispose();
        opened = created;
        setEngine(created);
        const { term } = created;
        const endTitle = followTitle(term, processId);
        unfollow = endTitle;
        term.onData((data) => writeSession(processId, data).catch(fail));
        term.onResize(({ rows, cols }) => resizeSession(processId, rows, cols).catch(() => {}));
        const onFrame = (frame: SessionFrame) => {
          if (disposed) return;
          if (!(frame instanceof ArrayBuffer || Array.isArray(frame))) {
            // The refresh that removes the pane can come late or fail. Late output must not name the exited process.
            endTitle();
            return onExit(processId, frame.exit);
          }
          // The parse is synchronous. The write callback waits for an animation frame, which a hidden window does not get.
          term.write(frame instanceof ArrayBuffer ? new Uint8Array(frame) : Uint8Array.from(frame));
          ack();
        };
        token = await attachSession(processId, onFrame);
        if (disposed) return detachSession(processId, token).catch(() => {});
        for (; unacked > 0; unacked--) ackSession(processId, token).catch(fail);
        // After the replay, so the TUI draws itself again at this size. An exited session takes no input.
        created.fit();
        await resizeSession(processId, term.rows, term.cols).catch(() => {});
      })
      .catch(onError);
    return () => {
      disposed = true;
      if (token !== undefined) detachSession(processId, token).catch(() => {});
      unfollow?.();
      opened?.dispose();
    };
  }, []);

  useEffect(() => {
    engine?.setVisible(visible);
  }, [engine, visible]);

  useEffect(() => {
    // A click in the search field of this pane focuses the pane: the field keeps the focus.
    if (visible && focused && !pane.current!.contains(document.activeElement)) engine?.term.focus();
  }, [engine, visible, focused, focusRequest]);

  useEffect(() => {
    if (!engine) return;
    const listener = engine.search.onDidChangeResults(setResults);
    return () => listener.dispose();
  }, [engine]);

  useEffect(() => {
    if (!finding) return;
    field.current!.focus();
    field.current!.select();
  }, [finding]);

  function find(next: string, previous = false) {
    setQuery(next);
    if (!next) {
      // The addon selects the active match. An empty search removes it with the decorations.
      engine!.search.clearDecorations();
      engine!.term.clearSelection();
    } else if (previous) engine!.search.findPrevious(next, searchOptions());
    else engine!.search.findNext(next, { ...searchOptions(), incremental: next !== query });
  }

  function close() {
    find("");
    setFinding(0);
    engine!.term.focus();
  }

  // The addon counts at most 1000 matches.
  const count = results && results.resultCount >= 1000 ? "1000+" : results?.resultCount;
  return (
    <div ref={pane} className="terminal-pane" hidden={!visible} style={style} onFocus={onFocus}>
      <div ref={container} className="terminal" />
      {finding > 0 && (
        <div
          className="terminal-search"
          role="search"
          onKeyDown={(event) => {
            if (event.key !== "Escape") return;
            event.preventDefault();
            close();
          }}
        >
          <TextField
            ref={field}
            label={<span className="sr-only">Search the terminal</span>}
            value={query}
            onChange={(event) => find(event.target.value)}
            onKeyDown={(event) => {
              if (event.key !== "Enter") return;
              event.preventDefault();
              if (query) find(query, event.shiftKey);
            }}
          />
          <span className="terminal-search-count" aria-live="polite">
            {/* A refresh for new output can come after the field is cleared. */}
            {query &&
              results &&
              (results.resultCount === 0
                ? "No matches"
                : results.resultIndex >= 0
                  ? `${results.resultIndex + 1} of ${count}`
                  : `${count} matches`)}
          </span>
          <IconButton label="Previous match" icon={<ChevronUp size={14} />} disabled={!query} onClick={() => find(query, true)} />
          <IconButton label="Next match" icon={<ChevronDown size={14} />} disabled={!query} onClick={() => find(query)} />
          <IconButton label="Close search" icon={<X size={14} />} onClick={close} />
        </div>
      )}
    </div>
  );
}
