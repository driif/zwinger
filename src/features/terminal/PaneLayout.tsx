import { Badge, Button, Dialog, IconButton, Splitter } from "@driif/motiv";
import { Code, Columns2, Ellipsis, Play, Rows2, X } from "lucide-react";
import { CSSProperties, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { PaneNode, CageStatus } from "../../shared/api";
import type { Pane } from "../../App";
import { paneName } from "./labels";
import { StartProgress } from "../projects/StartProgress";
import { place, Rect, setRatio } from "./panes";
import { folderLabel } from "./folders";
import { useTerminalTitles } from "./titles";
import { TerminalView } from "./TerminalView";
import { HostHistory } from "./HostHistory";
import { terminalHistory } from "./api";

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
  /** A committed divider position. */
  onLayout: (layout: PaneNode) => void;
  onReopen: (pane: Pane) => void;
  onPaneExit: (processId: string, code: number) => void;
  onPaneError: (error: unknown) => void;
  /** The name of the default editor. None: no editor is on. */
  editor?: string;
  /** The home folder, shown as `~`. */
  home?: string;
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

interface HostPaneProps {
  pane: Pane;
  name: string;
  rect?: Rect;
  visible: boolean;
  focused: boolean;
  focusRequest: number;
  onFocus: () => void;
  onPaneExit: (processId: string, code: number) => void;
  onPaneError: (error: unknown) => void;
  onReopen: () => void;
}

function HostPane(props: HostPaneProps) {
  const [runs, setRuns] = useState<number[][] | null>();
  const { pane } = props;
  const shown = props.visible && !!props.rect;
  useEffect(() => {
    if (!shown) {
      setRuns(undefined);
      return;
    }
    let active = true;
    terminalHistory(pane.sessionId, pane.key)
      .then((history) => { if (active) setRuns(history?.length ? history : null); })
      .catch(() => { if (active) setRuns(null); });
    return () => { active = false; };
  }, [pane.sessionId, pane.key, shown]);

  return <HostPaneBody {...props} runs={runs} />;
}

export function HostPaneBody({ pane, name, rect, visible, focused, focusRequest, onFocus, onPaneExit, onPaneError, onReopen, runs }: HostPaneProps & { runs?: number[][] | null }) {
  const outer = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (pane.processId && runs && outer.current) outer.current.scrollTop = outer.current.scrollHeight;
  }, [pane.processId, runs]);

  if (!pane.processId && !runs) {
    return rect && (
      <div className="pane-dormant" style={body(rect)}>
        <p className="muted">{name} is not running.</p>
        <Button variant="outline" iconLeft={<Play size={14} />} onClick={onReopen}>Open</Button>
      </div>
    );
  }

  return (
    <div ref={outer} className={pane.processId ? "host-pane live" : "host-pane dormant"} style={rect && body(rect)} hidden={!visible || !rect}>
      {runs?.map((run, index) => <HostHistory key={index} bytes={run} />)}
      {pane.processId ? (
        <div className="host-live">
          <TerminalView
            key={pane.processId}
            processId={pane.processId}
            exitCode={pane.exitCode}
            visible={visible && !!rect}
            focused={focused}
            focusRequest={focusRequest}
            onFocus={onFocus}
            onExit={onPaneExit}
            onReopen={onReopen}
            onError={onPaneError}
          />
        </div>
      ) : (
        <div className="terminal-exit">
          <span className="muted">Not running</span>
          <Button size="sm" variant="outline" iconLeft={<Play size={14} />} onClick={onReopen}>Open</Button>
        </div>
      )}
    </div>
  );
}

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
  home,
  onSplit,
  onOpenEditor,
  onHide,
}: Props) {
  const area = useRef<HTMLDivElement>(null);
  const titles = useTerminalTitles();
  const [actionsFor, setActionsFor] = useState<string>();
  const [focusRequest, setFocusRequest] = useState(0);
  const [preview, setPreview] = useState<{ saved: PaneNode; layout: PaneNode }>();
  const shown = preview && preview.saved === layout ? preview.layout : layout;
  const { panes: places, splits } = shown ? place(shown) : { panes: [], splits: [] };
  const rects = new Map(places.map((p) => [p.terminal, p.rect]));

  return (
    <div ref={area} className="pane-layout" hidden={!layout}>
      {layout && splits.map(({ path, direction, ratio, rect }) => {
        const row = direction === "row";
        const line = row
          ? {
              left: `${(rect.left + rect.width * ratio) * 100}%`,
              top: `${rect.top * 100}%`,
              height: `${rect.height * 100}%`,
              transform: "translateX(-50%)",
            }
          : {
              top: `${(rect.top + rect.height * ratio) * 100}%`,
              left: `${rect.left * 100}%`,
              width: `${rect.width * 100}%`,
              transform: "translateY(-50%)",
            };
        return (
          <Splitter
            key={path.join("") || "root"}
            className="pane-divider"
            style={line}
            orientation={row ? "vertical" : "horizontal"}
            label="Resize the panes"
            value={ratio}
            min={0.1}
            max={0.9}
            step={0.01}
            aria-valuetext={`${Math.round(ratio * 100)}%`}
            getValueFromDelta={(delta, start) => {
              const box = area.current!.getBoundingClientRect();
              return start + delta / (row ? box.width * rect.width : box.height * rect.height);
            }}
            onChange={(value) => setPreview({ saved: layout, layout: setRatio(layout, path, value) })}
            onCommit={(value) => {
              setPreview(undefined);
              onLayout(setRatio(layout, path, value));
            }}
          />
        );
      })}
      {panes.map((pane) => {
        const rect = rects.get(pane.key);
        if (pane.kind.type === "host") return (
          <HostPane
            key={pane.key}
            pane={pane}
            name={paneName(pane, panes)}
            rect={rect}
            visible={visible}
            focused={pane.key === focused}
            focusRequest={focusRequest}
            onFocus={() => onFocus(pane.key)}
            onPaneExit={onPaneExit}
            onPaneError={onPaneError}
            onReopen={() => onReopen(pane)}
          />
        );
        return (
          pane.processId && (
            <TerminalView
              key={pane.processId}
              processId={pane.processId}
              exitCode={pane.exitCode}
              visible={visible && rect !== undefined}
              focused={pane.key === focused}
              focusRequest={focusRequest}
              style={rect && body(rect)}
              onFocus={() => onFocus(pane.key)}
              onExit={onPaneExit}
              onReopen={() => onReopen(pane)}
              onError={onPaneError}
            />
          )
        );
      })}
      {places.map(({ terminal, rect }) => {
        const pane = panes.find((p) => p.key === terminal);
        return (
          pane &&
          pane.kind.type !== "host" &&
          !pane.processId && (
            <div key={terminal} className="pane-dormant" style={body(rect)}>
              {status.state === "starting" && pane.kind.type !== "hostAgent" ? (
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
        const title = pane.processId && titles[pane.processId];
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
            onClick={(event) => {
              // The buttons and the action dialog in the header do their own work.
              if ((event.target as Element).closest("button, dialog")) return;
              onFocus(terminal);
              setFocusRequest((n) => n + 1);
            }}
          >
            <Badge size="sm">{pane.kind.type === "host" || pane.kind.type === "hostAgent" ? "Mac" : "VM"}</Badge>
            <span className="grow pane-label">
              {/* The tab already names a single pane. A split names each pane. */}
              <span className={places.length > 1 ? "pane-name" : "sr-only"} title={pane.folder ? `${title || name}\n${pane.folder.path}` : title}>
                {title || name}
              </span>
              {pane.folder && (
                <span className="pane-folder mono" title={pane.folder.path}>
                  <span aria-hidden="true">{folderLabel(pane.folder, home)}</span>
                  <span className="sr-only">Folder {pane.folder.path}</span>
                </span>
              )}
            </span>
            <div role="group" className="pane-actions" aria-label={`Actions for ${name}`}>
              {actions.map((action) => <IconButton key={action.label} size="sm" label={action.label} icon={action.icon} onClick={action.run} />)}
            </div>
            <IconButton
              size="sm"
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
    </div>
  );
}
