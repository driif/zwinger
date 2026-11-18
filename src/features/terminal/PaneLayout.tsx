import { Badge, Button, Dialog, IconButton, IconButtonGroup } from "@driif/motiv";
import { Code, Columns2, Ellipsis, Play, Rows2, X } from "lucide-react";
import { CSSProperties, PointerEvent, useRef, useState } from "react";
import type { PaneNode, CageStatus } from "../../shared/api";
import type { Pane } from "../../App";
import { KindIcon, paneName } from "../projects/ProjectRail";
import { StartProgress } from "../projects/StartProgress";
import { Path, place, Rect, setRatio } from "./panes";
import { TerminalView } from "./TerminalView";

interface Props {
  /** The layout of the shown project. */
  layout: PaneNode | null;
  /** The status of the shown project. A VM pane shows the phase of its start. */
  status: CageStatus;
  /** The terminals of all projects. Each one with a session stays attached, also while it is not shown. */
  panes: Pane[];
  focused?: string;
  visible: boolean;
  onFocus: (key: string) => void;
  /** The end of a divider drag. */
  onLayout: (layout: PaneNode) => void;
  onReopen: (pane: Pane) => void;
  onPaneExit: () => void;
  onPaneError: (error: unknown) => void;
  /** The name of the default editor. None: no editor is on. */
  editor?: string;
  onSplit: (key: string, direction: "row" | "column") => void;
  onOpenEditor: (key: string) => void;
  /** Hides the pane. The terminal keeps running. */
  onHide: (key: string) => void;
}

const at = (r: Rect): CSSProperties => ({
  left: `${r.left * 100}%`,
  top: `${r.top * 100}%`,
  width: `${r.width * 100}%`,
  height: `${r.height * 100}%`,
});

/** The part of the pane below its header. */
const body = (r: Rect): CSSProperties => ({
  ...at(r),
  top: `calc(${r.top * 100}% + var(--zw-pane-header))`,
  height: `calc(${r.height * 100}% - var(--zw-pane-header))`,
});

/** The panes of the shown project side by side, with dividers that drag. */
export function PaneLayout({
  layout,
  status,
  panes,
  focused,
  visible,
  onFocus,
  onLayout,
  onReopen,
  onPaneExit,
  onPaneError,
  editor,
  onSplit,
  onOpenEditor,
  onHide,
}: Props) {
  const area = useRef<HTMLDivElement>(null);
  const [actionsFor, setActionsFor] = useState<string>();
  const [drag, setDrag] = useState<{ path: Path; ratio: number }>();
  const shown = layout && drag ? setRatio(layout, drag.path, drag.ratio) : layout;
  const { panes: places, splits } = shown ? place(shown) : { panes: [], splits: [] };
  const rects = new Map(places.map((p) => [p.terminal, p.rect]));

  function ratioAt(event: PointerEvent, rect: Rect, row: boolean) {
    const box = area.current!.getBoundingClientRect();
    const share = row
      ? ((event.clientX - box.left) / box.width - rect.left) / rect.width
      : ((event.clientY - box.top) / box.height - rect.top) / rect.height;
    return Math.min(0.9, Math.max(0.1, share));
  }

  return (
    <div ref={area} className="pane-layout" hidden={!layout}>
      {panes.map((pane) => {
        const rect = rects.get(pane.key);
        return (
          pane.processId && (
            <TerminalView
              key={pane.processId}
              processId={pane.processId}
              visible={visible && rect !== undefined}
              focused={pane.key === focused}
              style={rect && body(rect)}
              onFocus={() => onFocus(pane.key)}
              onExit={onPaneExit}
              onError={onPaneError}
            />
          )
        );
      })}
      {places.map(({ terminal, rect }) => {
        const pane = panes.find((p) => p.key === terminal);
        return (
          pane &&
          !pane.processId && (
            <div key={terminal} className="pane-dormant" style={body(rect)}>
              {status.state === "starting" && pane.kind.type !== "host" && pane.kind.type !== "hostAgent" ? (
                <StartProgress phase={status.phase} />
              ) : (
                <>
                  <p className="muted">{paneName(pane, panes)} is not running.</p>
                  <Button variant="outline" iconLeft={<Play size={14} />} onClick={() => onReopen(pane)}>
                    Open
                  </Button>
                </>
              )}
            </div>
          )
        );
      })}
      {places.map(({ terminal, rect }) => {
        const pane = panes.find((p) => p.key === terminal);
        if (!pane) return null;
        const name = paneName(pane, panes);
        const actions = [
          { label: "Split right with a Mac shell", icon: <Columns2 size={12} />, run: () => onSplit(terminal, "row") },
          { label: "Split down with a Mac shell", icon: <Rows2 size={12} />, run: () => onSplit(terminal, "column") },
          ...(editor ? [{ label: `Open the folder of ${name} in ${editor}`, icon: <Code size={12} />, run: () => onOpenEditor(terminal) }] : []),
          { label: "Hide pane", icon: <X size={12} />, run: () => onHide(terminal) },
        ];
        return (
          <div
            key={`header-${terminal}`}
            className={terminal === focused ? "pane-header focused" : "pane-header"}
            style={{ ...at(rect), height: "var(--zw-pane-header)" }}
          >
            <KindIcon kind={pane.kind} />
            <Badge>{pane.kind.type === "host" || pane.kind.type === "hostAgent" ? "Mac" : "VM"}</Badge>
            <span className="grow pane-name">{name}</span>
            <IconButtonGroup className="pane-actions" label={`Actions for ${name}`}>
              {actions.map((action) => <IconButton key={action.label} label={action.label} icon={action.icon} onClick={action.run} />)}
            </IconButtonGroup>
            <IconButton
              className="pane-actions-trigger"
              label={`Actions for ${name}`}
              icon={<Ellipsis size={14} />}
              aria-haspopup="dialog"
              onClick={() => setActionsFor(terminal)}
            />
            {visible && actionsFor === terminal && (
              <Dialog open title={`Actions for ${name}`} onClose={() => setActionsFor(undefined)}>
                <div className="stack">
                  {actions.map((action) => (
                    <Button key={action.label} iconLeft={action.icon} variant="ghost" onClick={() => {
                      setActionsFor(undefined);
                      action.run();
                    }}>
                      {action.label}
                    </Button>
                  ))}
                </div>
              </Dialog>
            )}
          </div>
        );
      })}
      {splits.map(({ path, direction, ratio, rect }) => {
        const row = direction === "row";
        const line = row
          ? { left: `${(rect.left + rect.width * ratio) * 100}%`, top: `${rect.top * 100}%`, height: `${rect.height * 100}%` }
          : { top: `${(rect.top + rect.height * ratio) * 100}%`, left: `${rect.left * 100}%`, width: `${rect.width * 100}%` };
        const dragging = drag?.path.join() === path.join();
        return (
          <div
            key={path.join("") || "root"}
            className={`pane-divider ${direction} ${dragging ? "dragging" : ""}`}
            style={line}
            role="separator"
            aria-orientation={row ? "vertical" : "horizontal"}
            aria-label="Resize the panes"
            aria-valuenow={Math.round(ratio * 100)}
            onPointerDown={(event) => {
              event.currentTarget.setPointerCapture(event.pointerId);
              setDrag({ path, ratio });
            }}
            onPointerMove={(event) => dragging && setDrag({ path, ratio: ratioAt(event, rect, row) })}
            onPointerUp={(event) => {
              if (!dragging) return;
              setDrag(undefined);
              onLayout(setRatio(layout!, path, ratioAt(event, rect, row)));
            }}
            onPointerCancel={() => setDrag(undefined)}
          />
        );
      })}
    </div>
  );
}
