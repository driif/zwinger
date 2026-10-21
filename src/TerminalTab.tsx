import { Button, MenuButton } from "@driif/motiv";
import { Bot, SquareTerminal, X } from "lucide-react";
import { useState } from "react";
import { SessionKind, ShieldView } from "./api";
import type { Pane } from "./App";
import { TerminalPane } from "./TerminalPane";

interface Props {
  shield: ShieldView;
  panes: Pane[];
  activePane?: string;
  visible: boolean;
  onShowPane: (key: string) => void;
  onNewSession: (kind: SessionKind) => void;
  onClosePane: (key: string) => void;
  onPaneOpen: (key: string, sessionId: string) => void;
  onPaneExit: () => void;
  onPaneError: (key: string, error: unknown) => void;
}

const KIND_ICON = { shell: <SquareTerminal size={14} />, pi: <Bot size={14} /> };

export function TerminalTab({
  shield,
  panes,
  activePane,
  visible,
  onShowPane,
  onNewSession,
  onClosePane,
  onPaneOpen,
  onPaneExit,
  onPaneError,
}: Props) {
  const [menuOpen, setMenuOpen] = useState(false);
  const own = panes.filter((p) => p.shieldId === shield.id);

  function create(kind: SessionKind) {
    setMenuOpen(false);
    onNewSession(kind);
  }

  return (
    <div className="terminal-tab">
      <div className="session-strip row" role="toolbar" aria-label="Sessions">
        {own.map((pane, index) => {
          const session = shield.sessions.find((s) => s.id === pane.sessionId);
          const state =
            pane.sessionId === undefined
              ? "opening"
              : session === undefined
                ? "ended"
                : typeof session.exitCode === "number"
                  ? `exited ${session.exitCode}`
                  : undefined;
          return (
            <div key={pane.key} className="row session-chip">
              <Button
                size="sm"
                variant={pane.key === activePane ? "secondary" : "ghost"}
                iconLeft={KIND_ICON[pane.kind]}
                aria-pressed={pane.key === activePane}
                onClick={() => onShowPane(pane.key)}
              >
                {pane.kind} {index + 1}
                {state && <span className="muted"> · {state}</span>}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                aria-label={`Close ${pane.kind} ${index + 1}`}
                iconLeft={<X size={12} />}
                onClick={() => onClosePane(pane.key)}
              />
            </div>
          );
        })}
        <div className="session-menu">
          <MenuButton open={menuOpen} onToggle={() => setMenuOpen(!menuOpen)} label="New session" />
          {menuOpen && (
            <div className="session-menu-list" role="menu">
              <Button size="sm" variant="ghost" role="menuitem" iconLeft={KIND_ICON.shell} onClick={() => create("shell")}>
                New shell <kbd className="muted">⌘T</kbd>
              </Button>
              <Button size="sm" variant="ghost" role="menuitem" iconLeft={KIND_ICON.pi} onClick={() => create("pi")}>
                New pi terminal
              </Button>
            </div>
          )}
        </div>
      </div>
      {own.length === 0 && (
        <div className="empty">
          <p className="muted">
            No sessions. A shell starts in /workspace. pi starts in {shield.harness ?? "/workspace"}.
          </p>
          <div className="row">
            <Button variant="outline" iconLeft={KIND_ICON.shell} onClick={() => onNewSession("shell")}>
              New shell
            </Button>
            <Button variant="outline" iconLeft={KIND_ICON.pi} onClick={() => onNewSession("pi")}>
              New pi terminal
            </Button>
          </div>
        </div>
      )}
      {panes.map((pane) => (
        <TerminalPane
          key={pane.key}
          shieldId={pane.shieldId}
          kind={pane.kind}
          active={visible && pane.shieldId === shield.id && pane.key === activePane}
          onOpen={(sessionId) => onPaneOpen(pane.key, sessionId)}
          onExit={onPaneExit}
          onError={(e) => onPaneError(pane.key, e)}
        />
      ))}
    </div>
  );
}
