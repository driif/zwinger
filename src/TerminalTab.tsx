import { Button } from "@driif/motiv";
import { Bot, SquareTerminal } from "lucide-react";
import { SessionKind, ShieldView } from "./api";
import type { Pane } from "./App";
import { TerminalPane } from "./TerminalPane";

interface Props {
  shield: ShieldView;
  panes: Pane[];
  activePane?: string;
  visible: boolean;
  onNewSession: (kind: SessionKind) => void;
  onPaneOpen: (key: string, sessionId: string) => void;
  onPaneExit: () => void;
  onPaneError: (key: string, error: unknown) => void;
}

/** Keeps the panes of every shield mounted; the tree picks the pane that shows. */
export function TerminalTab({ shield, panes, activePane, visible, onNewSession, onPaneOpen, onPaneExit, onPaneError }: Props) {
  const shown = panes.some((p) => p.shieldId === shield.id && p.key === activePane);
  return (
    <div className="terminal-tab">
      {!shown && (
        <div className="empty">
          <p className="muted">
            No terminal. A shell starts in /workspace. pi starts in {shield.harness ?? "/workspace"}.
          </p>
          <div className="row">
            <Button variant="outline" iconLeft={<SquareTerminal size={14} />} onClick={() => onNewSession("shell")}>
              New shell <kbd className="muted">⌘T</kbd>
            </Button>
            <Button variant="outline" iconLeft={<Bot size={14} />} onClick={() => onNewSession("pi")}>
              New pi terminal
            </Button>
          </div>
        </div>
      )}
      {panes.map((pane) => (
        <TerminalPane
          key={pane.key}
          session={{ shieldId: pane.shieldId, kind: pane.kind }}
          active={visible && pane.shieldId === shield.id && pane.key === activePane}
          onOpen={(sessionId) => onPaneOpen(pane.key, sessionId)}
          onExit={onPaneExit}
          onError={(e) => onPaneError(pane.key, e)}
        />
      ))}
    </div>
  );
}
