import { Button } from "@driif/motiv";
import { Bot, Play, SquareTerminal } from "lucide-react";
import { SessionKind, ShieldView } from "./api";
import type { Pane } from "./App";
import { paneName } from "./ShieldRail";
import { TerminalPane } from "./TerminalPane";

interface Props {
  shield: ShieldView;
  panes: Pane[];
  activePane?: string;
  visible: boolean;
  /** `agent` opens the shield's first agent. */
  onNewSession: (kind: SessionKind | "agent") => void;
  /** Opens a dormant or exited terminal again. */
  onReopen: (pane: Pane) => void;
  onPaneExit: () => void;
  onPaneError: (error: unknown) => void;
}

/** Keeps the panes of every shield attached; the tree picks the pane that shows. */
export function TerminalTab({ shield, panes, activePane, visible, onNewSession, onReopen, onPaneExit, onPaneError }: Props) {
  const shown = panes.find((p) => p.shieldId === shield.id && p.key === activePane);
  return (
    <div className="terminal-tab">
      {!shown && (
        <div className="empty">
          <p className="muted">
            No terminal. A shell starts in /workspace. An agent starts in {shield.harness ?? "/workspace"}.
            {shield.status.state !== "running" && " The shield starts first."}
          </p>
          <div className="row">
            <Button variant="outline" iconLeft={<SquareTerminal size={14} />} onClick={() => onNewSession({ type: "shell" })}>
              New shell <kbd className="muted">⌘T</kbd>
            </Button>
            <Button variant="outline" iconLeft={<Bot size={14} />} onClick={() => onNewSession("agent")}>
              New agent <kbd className="muted">⌘P</kbd>
            </Button>
          </div>
        </div>
      )}
      {shown && !shown.sessionId && (
        <div className="empty">
          <p className="muted">{paneName(shown, panes)} is not running. Open it to start a new process in its place.</p>
          <Button variant="outline" iconLeft={<Play size={14} />} onClick={() => onReopen(shown)}>
            Open
          </Button>
        </div>
      )}
      {panes.map(
        (pane) =>
          pane.sessionId && (
            <TerminalPane
              key={pane.sessionId}
              sessionId={pane.sessionId}
              active={visible && pane.key === shown?.key}
              onExit={onPaneExit}
              onError={onPaneError}
            />
          ),
      )}
    </div>
  );
}
