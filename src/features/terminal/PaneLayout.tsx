import { Button } from "@driif/motiv";
import { Play } from "lucide-react";
import { CSSProperties, PointerEvent, useRef, useState } from "react";
import type { PaneNode } from "../../shared/api";
import type { Pane } from "../../App";
import { paneName } from "../shields/ShieldRail";
import { Path, place, Rect, setRatio } from "./panes";
import { TerminalView } from "./TerminalView";

interface Props {
  /** The layout of the shown shield. */
  layout: PaneNode | null;
  /** The terminals of all shields. Each one with a session stays attached, also while it is not shown. */
  panes: Pane[];
  focused?: string;
  visible: boolean;
  onFocus: (key: string) => void;
  /** The end of a divider drag. */
  onLayout: (layout: PaneNode) => void;
  onReopen: (pane: Pane) => void;
  onPaneExit: () => void;
  onPaneError: (error: unknown) => void;
}

const at = (r: Rect): CSSProperties => ({
  left: `${r.left * 100}%`,
  top: `${r.top * 100}%`,
  width: `${r.width * 100}%`,
  height: `${r.height * 100}%`,
});

/** The panes of the shown shield side by side, with dividers that drag. */
export function PaneLayout({ layout, panes, focused, visible, onFocus, onLayout, onReopen, onPaneExit, onPaneError }: Props) {
  const area = useRef<HTMLDivElement>(null);
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
          pane.sessionId && (
            <TerminalView
              key={pane.sessionId}
              sessionId={pane.sessionId}
              visible={visible && rect !== undefined}
              focused={pane.key === focused}
              style={rect && at(rect)}
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
          !pane.sessionId && (
            <div key={terminal} className="pane-dormant" style={at(rect)}>
              <p className="muted">{paneName(pane, panes)} is not running.</p>
              <Button variant="outline" iconLeft={<Play size={14} />} onClick={() => onReopen(pane)}>
                Open
              </Button>
            </div>
          )
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
