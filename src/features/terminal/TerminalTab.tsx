import { Button } from "@driif/motiv";
import { Bot, Laptop } from "lucide-react";
import { PaneNode, SessionKind, ShieldView } from "../../shared/api";
import type { Pane } from "../../App";
import { PaneLayout } from "./PaneLayout";

interface Props {
  shield: ShieldView;
  /** The terminals of all shields. */
  panes: Pane[];
  /** The focused pane of the shown shield. */
  focused?: string;
  visible: boolean;
  /** `agent` opens the shield's first agent. */
  onNewSession: (kind: SessionKind | "agent") => void;
  onFocus: (key: string) => void;
  onLayout: (layout: PaneNode) => void;
  /** Opens a dormant or exited terminal again. */
  onReopen: (pane: Pane) => void;
  onPaneExit: () => void;
  onPaneError: (error: unknown) => void;
}

/** The pane layout of the shown shield. The terminals of every shield stay attached. */
export function TerminalTab({ shield, onNewSession, ...layout }: Props) {
  return (
    <div className="terminal-tab">
      {!shield.paneLayout && (
        <div className="empty">
          <p className="muted">
            No pane is open. A Mac shell starts in {shield.folder} and needs no shield start. An agent starts in the shield,
            in {shield.harness ?? "/workspace"}.
          </p>
          <div className="row">
            <Button variant="outline" iconLeft={<Laptop size={14} />} onClick={() => onNewSession({ type: "host" })}>
              New Mac shell <kbd className="muted">⌘T</kbd>
            </Button>
            <Button variant="outline" iconLeft={<Bot size={14} />} onClick={() => onNewSession("agent")}>
              New agent <kbd className="muted">⌘P</kbd>
            </Button>
          </div>
        </div>
      )}
      <PaneLayout layout={shield.paneLayout} {...layout} />
    </div>
  );
}
