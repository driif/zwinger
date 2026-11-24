import { Button, IconButton, TextField } from "@driif/motiv";
import { ChevronDown, ChevronUp, Info, Play, X } from "lucide-react";
import { CSSProperties, useEffect, useRef, useState } from "react";
import { ackSession, attachSession, detachSession, OpenedLocation, resizeSession, SessionFrame, writeSession } from "./api";
import { Engine, openTerminal, searchOptions } from "./engine";
import { followTitle } from "./titles";
import { Notice } from "../../shared/Notice";
import "./terminal.css";

/** Turns off the VT mouse reports, so a drag selects text again. The screen and its styles stay. */
const RELEASE_MOUSE = "\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l";

interface Props {
  processId: string;
  /** The exit code that the listing knows. `null`: the process runs, or the listing does not show its exit yet. */
  exitCode: number | null;
  visible: boolean;
  /** Takes the keyboard focus while it is visible. */
  focused: boolean;
  /** Each change gives the keyboard focus to the focused terminal again. */
  focusRequest: number;
  /** The place in the pane layout. No style fills the parent. */
  style?: CSSProperties;
  onFocus?: () => void;
  onExit: (processId: string, code: number) => void;
  /** Starts a new process in place of the exited one. */
  onReopen: () => void;
  onError: (error: unknown) => void;
}

/**
 * Attaches to the terminal process while it is mounted. An unmount detaches; the process keeps running.
 * After the exit it shows the last screen read-only, with the exit code.
 */
export function TerminalView({ processId, exitCode, visible, focused, focusRequest, style, onFocus, onExit, onReopen, onError }: Props) {
  const pane = useRef<HTMLDivElement>(null);
  const container = useRef<HTMLDivElement>(null);
  const field = useRef<HTMLInputElement>(null);
  const [engine, setEngine] = useState<Engine>();
  /** 0: the search is closed. Each ⌘F counts up and focuses the field again. */
  const [finding, setFinding] = useState(0);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<{ resultIndex: number; resultCount: number }>();
  const [exitFrame, setExitFrame] = useState<number>();
  /** Set when a file link opened in an editor that cannot go to the line. */
  const [lineIgnored, setLineIgnored] = useState<string>();
  const exit = exitFrame ?? exitCode;
  /** Read in the xterm listeners: an exited process takes no input, resize or reply, also before a refresh. */
  const exited = useRef(exit !== null);
  /** Makes the open terminal a read-only record of the exited process. It can run again. */
  const settle = useRef<() => void>(undefined);

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
      if (reported || disposed || exited.current) return;
      reported = true;
      onError(e);
    };
    const ack = () => {
      if (disposed) return;
      if (token === undefined) unacked++;
      else ackSession(processId, token).catch(fail);
    };
    const links = {
      processId,
      onOpened: ({ editor, lineIgnored }: OpenedLocation) => setLineIgnored(lineIgnored ? editor : undefined),
      onError,
    };
    openTerminal(container.current!, () => setFinding((n) => n + 1), links)
      .then(async (created) => {
        if (disposed) return created.dispose();
        opened = created;
        setEngine(created);
        const { term } = created;
        // A process that the listing shows as exited can replay its old title. Only a live process sets one.
        if (!exited.current) unfollow = followTitle(term, processId);
        settle.current = () => {
          // The listing can arrive later. Retire the live title as soon as the process exits.
          unfollow?.();
          exited.current = true;
          created.freeze();
          term.write(RELEASE_MOUSE);
        };
        term.onData((data) => {
          if (!disposed && !exited.current) writeSession(processId, data).catch(fail);
        });
        term.onResize(({ rows, cols }) => {
          if (!disposed && !exited.current) resizeSession(processId, rows, cols).catch(() => {});
        });
        const onFrame = (frame: SessionFrame) => {
          if (disposed) return;
          if (!(frame instanceof ArrayBuffer || Array.isArray(frame))) {
            settle.current!();
            setExitFrame(frame.exit);
            return onExit(processId, frame.exit);
          }
          // No write callback: it waits for an animation frame, which a hidden window does not get.
          term.write(frame instanceof ArrayBuffer ? new Uint8Array(frame) : Uint8Array.from(frame));
          // In order after the output: the last output of the process can turn the mouse reports on again.
          if (exited.current) term.write(RELEASE_MOUSE);
          ack();
        };
        if (exited.current) {
          // The replay waits for the size of the pane: a smaller grid would drop the top rows of a full-screen program.
          created.fit();
          await created.sized;
          if (disposed) return;
          settle.current();
        }
        token = await attachSession(processId, onFrame);
        if (disposed) return detachSession(processId, token).catch(() => {});
        for (; unacked > 0; unacked--) ackSession(processId, token).catch(fail);
        // After the replay, so the TUI draws itself again at this size.
        created.fit();
        if (!exited.current) await resizeSession(processId, term.rows, term.cols).catch(() => {});
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
    if (exit === null) return;
    exited.current = true;
    settle.current?.();
  }, [exit]);

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
      {lineIgnored && (
        <div className="terminal-link-notice">
          <Notice icon={<Info size={14} />} onDismiss={() => setLineIgnored(undefined)}>
            {lineIgnored} cannot go to a line, so it opened the file only.
          </Notice>
        </div>
      )}
      {exit !== null && (
        <div className="terminal-exit">
          <span className="muted">Exited with code {exit}</span>
          <Button size="sm" variant="outline" iconLeft={<Play size={14} />} onClick={onReopen}>
            Open
          </Button>
        </div>
      )}
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
          <IconButton size="sm" label="Previous match" icon={<ChevronUp size={14} />} disabled={!query} onClick={() => find(query, true)} />
          <IconButton size="sm" label="Next match" icon={<ChevronDown size={14} />} disabled={!query} onClick={() => find(query)} />
          <IconButton size="sm" label="Close search" icon={<X size={14} />} onClick={close} />
        </div>
      )}
    </div>
  );
}
