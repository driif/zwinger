import { Card } from "@driif/motiv";
import { KeyboardEvent, useRef, useState } from "react";
import {
  Bot,
  CircleAlert,
  CircleDot,
  CircleStop,
  Globe,
  LoaderCircle,
  Lock,
  Network,
  SquareTerminal,
} from "lucide-react";
import { SessionKind, ShieldView } from "./api";
import { SHORTCUTS } from "./useShortcuts";

export interface Pane {
  key: string;
  shieldId: string;
  kind: SessionKind;
  sessionId?: string;
}

interface Props {
  shields: ShieldView[];
  panes: Pane[];
  selectedShield?: string;
  selectedPane?: string;
  onSelectShield: (id: string) => void;
  onSelectPane: (key: string) => void;
}

const STATUS_ICON = {
  stopped: <CircleStop size={14} />,
  starting: <LoaderCircle size={14} className="spin" />,
  running: <CircleDot size={14} className="ok" />,
  stopping: <LoaderCircle size={14} className="spin" />,
  error: <CircleAlert size={14} className="danger" />,
};

export function Sidebar({
  shields,
  panes,
  selectedShield,
  selectedPane,
  onSelectShield,
  onSelectPane,
}: Props) {
  // Roving tabindex: one row is in the tab order, the arrow keys move between rows.
  const rows = shields.flatMap((shield) => [
    { key: shield.id, select: () => onSelectShield(shield.id) },
    ...panes.filter((p) => p.shieldId === shield.id).map((p) => ({ key: p.key, select: () => onSelectPane(p.key) })),
  ]);
  const elements = useRef(new Map<string, HTMLElement>());
  const [focused, setFocused] = useState<string>();
  const current = rows.find((r) => r.key === focused)?.key ?? selectedPane ?? selectedShield ?? rows[0]?.key;

  function rowProps(key: string) {
    const index = rows.findIndex((r) => r.key === key);
    return {
      role: "button",
      tabIndex: key === current ? 0 : -1,
      ref: (el: HTMLElement | null) => {
        if (el) elements.current.set(key, el);
        else elements.current.delete(key);
      },
      onFocus: () => setFocused(key),
      onClick: rows[index].select,
      onKeyDown: (event: KeyboardEvent) => {
        const step = { ArrowDown: 1, ArrowUp: -1 }[event.key];
        if (step !== undefined) {
          event.preventDefault();
          const next = rows[(index + step + rows.length) % rows.length];
          elements.current.get(next.key)?.focus();
        } else if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          rows[index].select();
        }
      },
    };
  }

  return (
    <aside className="sidebar">
      <ul className="tree">
        {shields.map((shield) => (
          <li key={shield.id}>
            <Card
              variant={shield.id === selectedShield ? "outlined" : "subtle"}
              padding="sm"
              interactive
              className="row"
              title={shield.status.state === "error" ? shield.status.message : shield.folder}
              {...rowProps(shield.id)}
            >
              {STATUS_ICON[shield.status.state]}
              <span className="grow">{shield.name}</span>
              {shield.network.allowAll ? (
                <Globe size={14} className="warn" aria-label="all egress allowed" />
              ) : shield.network.domains.length > 0 ? (
                <Network size={14} aria-label="allow list" />
              ) : (
                <Lock size={14} aria-label="no egress" />
              )}
            </Card>
            <ul className="tree nested">
              {panes
                .filter((pane) => pane.shieldId === shield.id)
                .map((pane) => {
                  const session = shield.sessions.find((s) => s.id === pane.sessionId);
                  const exited = pane.sessionId !== undefined && session?.exitCode !== null;
                  return (
                    <li key={pane.key}>
                      <Card
                        variant={pane.key === selectedPane ? "outlined" : "subtle"}
                        padding="sm"
                        interactive
                        className={exited ? "row muted" : "row"}
                        {...rowProps(pane.key)}
                      >
                        {pane.kind === "pi" ? <Bot size={14} /> : <SquareTerminal size={14} />}
                        <span className="grow">{pane.kind}</span>
                        <span className="mono">
                          {pane.sessionId === undefined
                            ? "opening"
                            : exited
                              ? `exited ${session?.exitCode ?? ""}`
                              : "running"}
                        </span>
                      </Card>
                    </li>
                  );
                })}
            </ul>
          </li>
        ))}
      </ul>
      <dl className="shortcuts">
        {SHORTCUTS.map((s) => (
          <div key={s.key}>
            <dt className="mono">{s.label}</dt>
            <dd>{s.text}</dd>
          </div>
        ))}
      </dl>
    </aside>
  );
}
