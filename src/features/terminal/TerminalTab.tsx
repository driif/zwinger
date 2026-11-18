import { Button } from "@driif/motiv";
import { Bot, Laptop } from "lucide-react";
import { CageStatus, PaneNode, SessionView, TerminalKind } from "../../shared/api";
import type { Pane } from "../../App";
import { PaneLayout } from "./PaneLayout";
import { StartProgress } from "../projects/StartProgress";

interface Props {
  session: SessionView;
  /** The status of the session's cage. */
  status: CageStatus;
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
  onPaneExit: (processId: string, code: number) => void;
  onPaneError: (error: unknown) => void;
  /** The name of the default editor. */
  editor?: string;
  /** The home folder, shown as `~`. */
  home?: string;
  onSplit: (key: string, direction: "row" | "column") => void;
  onOpenEditor: (key: string) => void;
  onHide: (key: string) => void;
}

/** The pane layout of the shown project. The terminals of every project stay attached. */
export function TerminalTab({ session, status, onNewSession, ...layout }: Props) {
  return (
    <div className="terminal-tab">
      {!session.paneLayout && status.state === "starting" && (
        <div className="empty">
          <StartProgress phase={status.phase} />
        </div>
      )}
      {!session.paneLayout && status.state !== "starting" && (
        <div className="empty">
          <p className="muted">
            No pane is open. A Mac shell starts in {session.folder} and needs no cage start.{" "}
            {session.effectiveCage
              ? `An agent starts in the cage, in ${session.harness ?? "/workspace"}.`
              : `This session has no cage, so an agent runs on this Mac in ${session.folder}, and nothing isolates it.`}
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
      <PaneLayout layout={session.paneLayout} status={status} {...layout} />
    </div>
  );
}
