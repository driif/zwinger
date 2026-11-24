import { Badge, Button, IconButton, IconButtonGroup } from "@driif/motiv";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  Background,
  BackgroundVariant,
  getViewportForBounds,
  Handle,
  Node,
  NodeProps,
  NodeTypes,
  Panel,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useNodesInitialized,
  useNodesState,
  useReactFlow,
  useStoreApi,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { FolderPlus, Maximize, Minus, Network, Plus } from "lucide-react";
import { RefObject, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { NetEvent } from "../network/api";
import { resolveFolder, setLayout, setViewport } from "./api";
import { isRoundFolder } from "../tasks/api";
import { folderDrop, resolveFolderTarget } from "./folderDrop";
import { CageView, Pos, SessionView, Viewport, VmCage } from "../../shared/api";
import { useCatalog } from "../network/network";
import type { Run } from "../../App";
import { CageMark } from "../../shared/CageMark";
import { BoundaryData, ItemData, Jump, MapEdit, MapMetrics, MapNode, mapEdit, mapLayout } from "./mapLayout";
import "./map.css";

const BLOCKED_MS = 60_000;

/** The node positions and the viewport that the user chose. `viewport: null`: the map fits its nodes. */
interface Choice {
  layout: Record<string, Pos>;
  viewport: Viewport | null;
}

const sameView = (a: Viewport | null | undefined, b: Viewport | null | undefined) =>
  a === b || (!!a && !!b && a.x === b.x && a.y === b.y && a.zoom === b.zoom);

const sameLayout = (a: Record<string, Pos>, b: Record<string, Pos>) =>
  Object.keys(a).length === Object.keys(b).length &&
  Object.entries(a).every(([id, pos]) => b[id]?.x === pos.x && b[id]?.y === pos.y);

/**
 * The choice that this window wrote last per project, with its write number. It wins over a session list that does
 * not have it yet, also when the map mounts again. `settled`: the last write is done, so a list that has the choice is
 * current. `confirmed`: the choice that the done writes stored. `last`: the number of the last write of each field.
 */
const written = new Map<
  string,
  Choice & { seq: number; settled: boolean; confirmed: Choice; last: Record<keyof Choice, number> }
>();
/** The last map write per project. A write waits for the one before, so an older write cannot win. */
const queues = new Map<string, Promise<unknown>>();
let writes = 0;
/** Shows the given choice on the mounted map of the project. */
const restorers = new Map<string, (choice: Choice) => void>();

/**
 * Applies `change` to `current`, the choice on the map, and returns the result. `send` stores the change after the
 * earlier writes of the project. If the write fails, each changed field that no later write changed goes back to its
 * confirmed value, also on the mounted map.
 */
function write(sessionId: string, current: Choice, change: Partial<Choice>, send: () => Promise<unknown>, run: Run) {
  const seq = ++writes;
  const before = written.get(sessionId);
  const fields = Object.keys(change) as (keyof Choice)[];
  const last = { layout: 0, viewport: 0, ...before?.last };
  for (const field of fields) last[field] = seq;
  const choice = { ...current, ...change };
  // With no entry, no write is pending, so the choice on the map is stored.
  written.set(sessionId, { ...choice, seq, settled: false, confirmed: before?.confirmed ?? current, last });
  const done = (queues.get(sessionId) ?? Promise.resolve()).then(send);
  queues.set(sessionId, done.catch(() => {}));
  done.then(
    () => {
      const entry = written.get(sessionId);
      if (!entry) return;
      entry.confirmed = { ...entry.confirmed, ...change };
      if (entry.seq === seq) entry.settled = true;
    },
    () => {
      const entry = written.get(sessionId);
      if (!entry) return;
      if (entry.seq === seq) entry.settled = true;
      const failed = fields.filter((field) => entry.last[field] === seq);
      if (failed.length === 0) return;
      Object.assign(entry, Object.fromEntries(failed.map((field) => [field, entry.confirmed[field]])));
      restorers.get(sessionId)?.({ layout: entry.layout, viewport: entry.viewport });
    },
  );
  run(() => done);
  return choice;
}

/** Keeps the end of a path, which is the part that tells folders apart. */
function fit(text: string, width: number, charWidth: number) {
  if (charWidth === 0) return text;
  const max = Math.floor(width / charWidth);
  if (max <= 1) return "…";
  return text.length > max ? `…${text.slice(-(max - 1))}` : text;
}

function ItemNode({ id, data, width = 0 }: NodeProps<Node<ItemData, "item">>) {
  const { icon: Icon } = data;
  return (
    <div className={`map-node ${data.tone ?? ""} ${data.count === undefined ? "" : "metric"}`} title={data.title}>
      <Handle type="target" position={Position.Left} isConnectable={id === "new-host" || !!data.hostTarget} />
      <Icon size={14} />
      {/* 46 px: the icon, the padding and the activity dot in the top-right corner. */}
      <span className="map-label">{fit(data.label, width - 46 - data.badges.length * 44, data.charWidth ?? 0)}</span>
      {data.badges.map((badge) => (
        <span key={badge} className={`map-badge ${badge}`}>
          {badge}
        </span>
      ))}
      {data.active && <span className="map-activity" aria-label="recent traffic" />}
      {data.count !== undefined && (
        <span className="map-metric">
          <Badge>{data.count}</Badge>
          {data.bytes && <Badge>{data.bytes}</Badge>}
        </span>
      )}
      <Handle type="source" position={Position.Right} isConnectable={id === "source:new-folder"} />
    </div>
  );
}

function BoundaryNode({ data }: NodeProps<Node<BoundaryData, "boundary">>) {
  return (
    <div className={`map-boundary ${data.state}`}>
      <div className="map-boundary-header">
        <CageMark size={18} />
        <span className="map-title">{data.name}</span>
        <span className="map-state">microVM · {data.state}</span>
      </div>
      <Handle type="target" position={Position.Left} className="map-mount-target" />
      <Handle type="source" position={Position.Right} className="map-gateway">
        <Network size={14} />
      </Handle>
    </div>
  );
}

const nodeTypes: NodeTypes = {
  item: ItemNode,
  boundary: BoundaryNode,
  caption: ({ data }: NodeProps<Node<{ label: string }>>) => <span className="map-caption">{data.label}</span>,
};

/** `onStart` runs before a control moves the viewport. `onView` gets the viewport when the control has moved it. */
function MapControls({ onStart, onView, onFit }: {
  onStart: () => void;
  onView: (viewport: Viewport) => void;
  onFit: () => Promise<boolean>;
}) {
  const { zoomIn, zoomOut, getViewport } = useReactFlow();
  const then = (move: () => Promise<boolean>) => {
    onStart();
    move().then(() => onView(getViewport()));
  };
  return (
    <IconButtonGroup size="sm" label="Map view">
      <IconButton icon={<Plus size={14} />} label="Zoom in" onClick={() => then(zoomIn)} />
      <IconButton icon={<Minus size={14} />} label="Zoom out" onClick={() => then(zoomOut)} />
      <IconButton icon={<Maximize size={14} />} label="Fit view" onClick={() => then(onFit)} />
    </IconButtonGroup>
  );
}

/** Arrays, such as the badges, are equal when their items are. */
const sameData = (a: Record<string, unknown>, b: Record<string, unknown>) =>
  Object.keys({ ...a, ...b }).every(
    (key) => a[key] === b[key] || (Array.isArray(a[key]) && Array.isArray(b[key]) && `${a[key]}` === `${b[key]}`),
  );

interface Props {
  session: SessionView;
  cage: CageView;
  /** The cage of the session's VM. */
  vmCage: VmCage | null;
  /** `undefined` until the folders of this session are listed, and while its folder cannot be read. */
  folders?: string[];
  /** The cage's network events, oldest first. */
  events: NetEvent[];
  visible: boolean;
  run: Run;
  onJump: (tab: Jump) => void;
  onEdit: (edit: MapEdit) => void;
  /** A folder from Finder was dropped on the project boundary. */
  onDropFolder: (path: string, host: string) => void;
}

/** Host paths on the left, the microVM in the middle, egress on the right. The user can drag the nodes. */
export function ProjectMap(props: Props) {
  return (
    <ReactFlowProvider>
      <MapCanvas {...props} />
    </ReactFlowProvider>
  );
}

/** Measures the motiv tokens in the box. The map is laid out only when it knows them. */
function MapCanvas(props: Props) {
  const container = useRef<HTMLDivElement>(null);
  const [metrics, setMetrics] = useState<MapMetrics>();

  useLayoutEffect(() => {
    if (!props.visible) return;
    const box = container.current!;
    const measure = () => {
      const probe = document.createElement("span");
      probe.style.cssText =
        "position:absolute;visibility:hidden;width:var(--motiv-space);height:var(--motiv-row-height)";
      const character = document.createElement("span");
      character.style.cssText = "position:absolute;visibility:hidden;width:1ch";
      box.append(probe, character);
      const next = {
        rowHeight: parseFloat(getComputedStyle(probe).height),
        space: parseFloat(getComputedStyle(probe).width),
        charWidth: parseFloat(getComputedStyle(character).width),
      };
      probe.remove();
      character.remove();
      if (!(next.rowHeight > 0 && next.space > 0 && next.charWidth > 0)) return;
      setMetrics((current) =>
        current?.rowHeight === next.rowHeight && current.space === next.space && current.charWidth === next.charWidth
          ? current
          : next,
      );
    };
    measure();
    const observer = new MutationObserver(measure);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-density", "data-theme", "style"] });
    return () => observer.disconnect();
  }, [props.visible]);

  return (
    <div ref={container} className="map" data-drop="map" role="group" aria-label={`Map of project ${props.session.name}`}>
      {metrics && <MapFlow {...props} metrics={metrics} container={container} />}
    </div>
  );
}

function MapFlow({
  session,
  cage,
  vmCage,
  folders,
  events,
  visible,
  run,
  onJump,
  onEdit,
  onDropFolder,
  metrics,
  container,
}: Props & { metrics: MapMetrics; container: RefObject<HTMLDivElement | null> }) {
  const { getNodes, getNodesBounds, setViewport: moveTo } = useReactFlow<MapNode>();
  const store = useStoreApi();
  const [now, setNow] = useState(Date.now);
  const latest = events[events.length - 1]?.at ?? 0;
  const catalog = useCatalog();
  // Only this map changes the choice, so after the mount the session list cannot roll it back.
  const [choice, setChoice] = useState<Choice>(
    () => written.get(session.id) ?? { layout: session.layout, viewport: session.viewport },
  );
  const { layout, viewport } = choice;
  const [initial] = useState(viewport);
  // The viewport of a pan, wheel or pinch that has not ended yet.
  const gesture = useRef<Viewport>(undefined);
  // The input event of the last gesture. A wheel gesture ends about 150 ms after its last event. Until then, the moves
  // of a control or a reset have the event of the wheel.
  const input = useRef<unknown>(undefined);
  // The input event of the gesture that a control replaced. Its moves and its end are not the user's.
  const replaced = useRef<unknown>(undefined);
  // A control, a reset or a restore replaces the gesture that has not ended, so the gesture cannot save over it.
  const dropGesture = () => {
    gesture.current = undefined;
    replaced.current = input.current;
  };

  // The written choice is forgotten when the list has it.
  useEffect(() => {
    const entry = written.get(session.id);
    if (entry?.settled && sameLayout(entry.layout, session.layout) && sameView(entry.viewport, session.viewport)) {
      written.delete(session.id);
    }
  }, [session.layout, session.viewport]);

  // Ticks while an event is recent, so the activity dots and blocked hosts expire.
  useEffect(() => {
    const tick = () => {
      const time = Date.now();
      setNow(time);
      if (time > latest + BLOCKED_MS) clearInterval(timer);
    };
    const timer = setInterval(tick, 1000);
    tick();
    return () => clearInterval(timer);
  }, [latest]);

  const map = useMemo(
    () => mapLayout({ ...session, layout }, cage, vmCage, folders ?? [], events, catalog, now, metrics),
    [session, cage, vmCage, layout, folders, events, catalog, now, metrics],
  );
  const [nodes, setNodes, onNodesChange] = useNodesState<MapNode>(map.nodes);
  const nodesInitialized = useNodesInitialized();

  // Panels cover the canvas but are outside the nodes bounds. Reserve the legend's actual bottom area for every fit.
  function fitMap() {
    const { width, height, minZoom, maxZoom } = store.getState();
    const box = container.current!;
    const legend = box.querySelector<HTMLElement>(".map-legend");
    const bottomPadding = legend
      ? Math.max(0, box.getBoundingClientRect().bottom - legend.getBoundingClientRect().top)
      : 0;
    return moveTo(
      getViewportForBounds(getNodesBounds(getNodes()), width, Math.max(1, height - bottomPadding), minZoom, maxZoom, 0.1),
    );
  }

  // Keeps the current node objects, with their measured size, unless something
  // that they show changed. A dragged node stays where the user holds it.
  useEffect(() => {
    setNodes((current) => {
      const byId = new Map(current.map((n) => [n.id, n]));
      return map.nodes.map((next) => {
        const node = byId.get(next.id);
        if (!node || node.type !== next.type) return next;
        if (node.dragging) return node;
        const same =
          node.position.x === next.position.x &&
          node.position.y === next.position.y &&
          node.width === next.width &&
          node.height === next.height &&
          sameData(node.data, next.data);
        if (same) return node;
        const { position, width, height, ariaLabel, data } = next;
        return { ...node, position, width, height, ariaLabel, data } as MapNode;
      });
    });
  }, [map]);

  function choose(change: Partial<Choice>, send: () => Promise<unknown>) {
    setChoice(write(session.id, choice, change, send, run));
  }

  // A failed write shows the stored choice again, so the same control can try again. The map that made the write
  // can be gone.
  useEffect(() => {
    const restore = (stored: Choice) => {
      dropGesture();
      setChoice(stored);
      // After the nodes go back to their stored positions, and after a fit that is still queued, such as the fit of
      // a reset.
      requestAnimationFrame(() => fitMap().then(() => stored.viewport && moveTo(stored.viewport)));
    };
    restorers.set(session.id, restore);
    return () => {
      if (restorers.get(session.id) === restore) restorers.delete(session.id);
    };
  }, []);

  function save(next: Record<string, Pos>) {
    choose({ layout: next }, () => setLayout(session.id, next));
  }

  // Positions of nodes that no longer exist are dropped. Folder nodes are
  // unknown until the folders are listed or while the project folder cannot
  // be read, so their positions stay meanwhile.
  useEffect(() => {
    const ids = new Set(map.nodes.map((n) => n.id));
    const keep = (id: string) => ids.has(id) || (folders === undefined && id.startsWith("folder:"));
    if (Object.keys(layout).every(keep)) return;
    save(Object.fromEntries(Object.entries(layout).filter(([id]) => keep(id))));
  }, [map]);

  function chooseView(next: Viewport) {
    if (sameView(next, viewport)) return;
    choose({ viewport: next }, () => setViewport(session.id, next));
  }

  // A gesture that has not ended when the map closes is saved too, such as a wheel zoom.
  const endGesture = useRef(() => {});
  endGesture.current = () => {
    const end = gesture.current;
    gesture.current = undefined;
    if (end) chooseView(end);
  };
  useEffect(() => () => endGesture.current(), []);

  function reset() {
    dropGesture();
    choose({ layout: {}, viewport: null }, () => setLayout(session.id, null));
    // After the nodes go back to their auto positions.
    requestAnimationFrame(() => fitMap());
  }

  // With no chosen viewport, the map fits the box when it shows and when the box
  // resizes. A chosen viewport stays, also when nodes are outside the box.
  // The observer does not depend on node readiness: `useNodesInitialized` changes inside React Flow's own node
  // ResizeObserver delivery, and an observer created there gives a "ResizeObserver loop" error in WebKit.
  const fits = viewport === null;
  const nodesInitializedRef = useRef(nodesInitialized);
  nodesInitializedRef.current = nodesInitialized;
  const check = () => {
    const box = container.current;
    if (!nodesInitializedRef.current || !box || box.clientWidth === 0 || box.clientHeight === 0) return;
    fitMap();
  };
  useEffect(() => {
    if (!visible || !fits) return;
    const box = container.current!;
    let frame = 0;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(check);
    });
    observer.observe(box);
    const legend = box.querySelector(".map-legend");
    if (legend) observer.observe(legend);
    return () => {
      observer.disconnect();
      cancelAnimationFrame(frame);
    };
  }, [visible, fits, map.hasSecrets]);
  useEffect(() => {
    if (!visible || !fits || !nodesInitialized) return;
    const frame = requestAnimationFrame(check);
    return () => cancelAnimationFrame(frame);
  }, [visible, fits, nodesInitialized]);

  const onDrop = useRef(onDropFolder);
  onDrop.current = onDropFolder;
  const runRef = useRef(run);
  runRef.current = run;
  useEffect(() => {
    // The map remounts for each project and cage, so a drop that resolves after unmount is stale.
    let live = true;
    const stop = getCurrentWebview().onDragDropEvent(async ({ payload }) => {
      if (payload.type !== "drop" || payload.paths.length === 0) return;
      const target = resolveFolderTarget(payload.position.toLogical(window.devicePixelRatio));
      if (target?.kind !== "map" || !container.current?.contains(target.element)) return;
      const dropped = payload.paths[0];
      const drop = onDrop.current;
      // The App listener opens the room of a round folder.
      if (await isRoundFolder(dropped)) return;
      if (!live) return;
      runRef.current(async () => {
        let host: string;
        try {
          host = await resolveFolder(dropped);
        } catch (e) {
          if (!live) return;
          throw e;
        }
        if (!live) return;
        drop(dropped, host);
      });
    });
    return () => {
      live = false;
      stop.then((unlisten) => unlisten());
    };
  }, []);

  const jump = (node?: MapNode) => node?.type === "item" && node.data.jump && onJump(node.data.jump);

  return (
    <>
      <div className="row end map-actions">
        <Button size="sm" variant="outline" iconLeft={<FolderPlus size={14} />} onClick={() => onEdit({ kind: "mount" })}>
          Add mount
        </Button>
        <Button size="sm" variant="outline" iconLeft={<Network size={14} />} onClick={() => onEdit({ kind: "addRule" })}>
          Allow host
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={Object.keys(layout).length === 0 && viewport === null}
          onClick={reset}
        >
          Reset layout
        </Button>
      </div>
      <ReactFlow<MapNode>
        nodes={nodes}
        edges={map.edges}
        nodeTypes={nodeTypes}
        onNodesChange={onNodesChange}
        onNodeDragStop={(_, __, dragged) =>
          save({ ...layout, ...Object.fromEntries(dragged.map((node) => [node.id, node.position])) })
        }
        onNodeClick={(_, node) => jump(node)}
        isValidConnection={(connection) => !!mapEdit(connection.source, connection.target, map.nodes)}
        onConnect={(connection) => {
          const edit = mapEdit(connection.source, connection.target, map.nodes);
          if (edit) onEdit(edit);
        }}
        onConnectEnd={(event, state) => {
          if (state.fromNode?.id !== "boundary" || state.fromHandle?.type !== "source" || state.toNode || !state.from || !state.to) return;
          if (Math.hypot(state.to.x - state.from.x, state.to.y - state.from.y) < 24) return;
          const point = event instanceof MouseEvent ? event : event.changedTouches.item(0);
          if (!point) return;
          const under = document.elementFromPoint(point.clientX, point.clientY);
          if (!under || !container.current?.contains(under) || !under.closest(".react-flow__pane") ||
            under.closest(".react-flow__node, .react-flow__panel, .react-flow__edge")) return;
          onEdit({ kind: "addRule" });
        }}
        // Only a pan, wheel or pinch has an event. A click on the pane does not move the map.
        onMove={(event, next) => {
          folderDrop.refresh();
          if (!event || event === replaced.current) return;
          input.current = event;
          gesture.current = next;
        }}
        onMoveEnd={(event, end) => {
          if (!event || event === replaced.current || !gesture.current) return;
          gesture.current = end;
          endGesture.current();
        }}
        onKeyDown={(event) => {
          if (event.key !== "Enter") return;
          const id = (event.target as HTMLElement).closest(".react-flow__node")?.getAttribute("data-id");
          jump(nodes.find((n) => n.id === id));
        }}
        nodesConnectable
        connectOnClick={false}
        elementsSelectable={false}
        deleteKeyCode={null}
        minZoom={0.3}
        defaultViewport={initial ?? undefined}
        fitView={initial === null}
      >
        <Background variant={BackgroundVariant.Dots} />
        <Panel position="bottom-left">
          <MapControls onStart={dropGesture} onView={chooseView} onFit={fitMap} />
        </Panel>
        {map.hasSecrets && (
          <Panel position="bottom-right" className="map-legend">
            <p className="row">
              <span className="map-badge key">key</span> gets a credential
              <span className="map-node warn map-swatch" /> login host that the rules block
            </p>
            <p>The agent sees $MSB_…, the proxy adds the real value.</p>
          </Panel>
        )}
      </ReactFlow>
    </>
  );
}
