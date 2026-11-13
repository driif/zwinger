import { Button } from "@driif/motiv";
import { Bot, Laptop } from "lucide-react";
import { CageView, PaneNode, SessionView, TerminalKind } from "../../shared/api";
import type { Pane } from "../../App";
import { PaneLayout } from "./PaneLayout";
import { StartProgress } from "../projects/StartProgress";

interface Props {
  session: SessionView;
  cage: CageView;
  /** The terminals of all projects. */
  panes: Pane[];
  /** The focused pane of the shown project. */
  focused?: string;
  visible: boolean;
  /** `agent` opens the project's first agent. */
  onNewSession: (kind: TerminalKind | "agent") => void;
  onFocus: (key: string) => void;
  onLayout: (layout: PaneNode) => void;
  /** Opens a dormant or exited terminal again. */
  onReopen: (pane: Pane) => void;
  onPaneExit: () => void;
  onPaneError: (error: unknown) => void;
  /** The name of the default editor. */
  editor?: string;
  onSplit: (key: string, direction: "row" | "column") => void;
  onOpenEditor: (key: string) => void;
  onHide: (key: string) => void;
}

/** The pane layout of the shown project. The terminals of every project stay attached. */
export function TerminalTab({ session, cage, onNewSession, ...layout }: Props) {
  return (
    <div className="terminal-tab">
      {!session.paneLayout && cage.status.state === "starting" && (
        <div className="empty">
          <StartProgress phase={cage.status.phase} />
        </div>
      )}
      {!session.paneLayout && cage.status.state !== "starting" && (
        <div className="empty">
          <p className="muted">
            No pane is open. A Mac shell starts in {session.folder} and needs no cage start. An agent starts in the cage,
            in {session.harness ?? "/workspace"}.
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
      <PaneLayout layout={session.paneLayout} status={cage.status} {...layout} />
    </div>
  );
}
