import { Button, IconButton, IconButtonGroup } from "@driif/motiv";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  Background,
  BackgroundVariant,
  Handle,
  Node,
  NodeProps,
  NodeTypes,
  Panel,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useNodesState,
  useReactFlow,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { Maximize, Minus, Network, Plus, Shield } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { isRoundFolder, NetEvent, Pos, setLayout, ShieldView } from "./api";
import type { Run } from "./App";
import { BoundaryData, ItemData, Jump, MapNode, mapLayout } from "./mapLayout";

const CHAR = 7.2;
const BLOCKED_MS = 60_000;

/** Keeps the end of a path, which is the part that tells folders apart. */
function fit(text: string, width: number) {
  const max = Math.floor(width / CHAR);
  return text.length > max ? `…${text.slice(-(max - 1))}` : text;
}

function ItemNode({ data, width = 0 }: NodeProps<Node<ItemData, "item">>) {
  const { icon: Icon } = data;
  return (
    <div className={`map-node ${data.tone ?? ""}`} title={data.title}>
      <Handle type="target" position={Position.Left} isConnectable={false} />
      <Icon size={14} />
      <span className="map-label">{fit(data.label, width - 36 - data.badges.length * 44)}</span>
      {data.badges.map((badge) => (
        <span key={badge} className={`map-badge ${badge}`}>
          {badge}
        </span>
      ))}
      {data.active && <span className="map-activity" aria-label="recent traffic" />}
      <Handle type="source" position={Position.Right} isConnectable={false} />
    </div>
  );
}

function BoundaryNode({ data }: NodeProps<Node<BoundaryData, "boundary">>) {
  return (
    <div className={`map-boundary ${data.state}`}>
      <div className="map-boundary-header">
        <Shield size={18} />
        <span className="map-title">{data.name}</span>
        <span className="map-state">microVM · {data.state}</span>
      </div>
      <Handle type="source" position={Position.Right} isConnectable={false} className="map-gateway">
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

function MapControls() {
  const { zoomIn, zoomOut, fitView } = useReactFlow();
  return (
    <IconButtonGroup label="Map view">
      <IconButton icon={<Plus size={14} />} label="Zoom in" onClick={() => zoomIn()} />
      <IconButton icon={<Minus size={14} />} label="Zoom out" onClick={() => zoomOut()} />
      <IconButton icon={<Maximize size={14} />} label="Fit view" onClick={() => fitView()} />
    </IconButtonGroup>
  );
}

/** Arrays, such as the badges, are equal when their items are. */
const sameData = (a: Record<string, unknown>, b: Record<string, unknown>) =>
  Object.keys({ ...a, ...b }).every(
    (key) => a[key] === b[key] || (Array.isArray(a[key]) && Array.isArray(b[key]) && `${a[key]}` === `${b[key]}`),
  );

interface Props {
  shield: ShieldView;
  /** `undefined` until the folders of this shield are listed. */
  folders?: string[];
  /** The shield's network events, oldest first. */
  events: NetEvent[];
  visible: boolean;
  run: Run;
  onJump: (tab: Jump) => void;
  /** A folder from Finder was dropped on the shield boundary. */
  onDropFolder: (path: string) => void;
}

/** Host paths on the left, the microVM in the middle, egress on the right. The user can drag the nodes. */
export function ShieldMap(props: Props) {
  return (
    <ReactFlowProvider>
      <MapCanvas {...props} />
    </ReactFlowProvider>
  );
}

function MapCanvas({ shield, folders, events, visible, run, onJump, onDropFolder }: Props) {
  const { fitView, getNode, screenToFlowPosition } = useReactFlow<MapNode>();
  const container = useRef<HTMLDivElement>(null);
  const [now, setNow] = useState(Date.now);
  const latest = events[events.length - 1]?.at ?? 0;
  // The layout that the last save sent, until the shield list has it.
  const [saved, setSaved] = useState<{ base: Record<string, Pos>; layout: Record<string, Pos> }>();
  const layout = saved?.base === shield.layout ? saved.layout : shield.layout;

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
    () => mapLayout({ ...shield, layout }, folders ?? [], events, now),
    [shield, layout, folders, events, now],
  );
  const [nodes, setNodes, onNodesChange] = useNodesState<MapNode>(map.nodes);

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

  function save(next: Record<string, Pos> | null) {
    setSaved({ base: shield.layout, layout: next ?? {} });
    run(() => setLayout(shield.id, next));
  }

  // Positions of nodes that no longer exist are dropped. Folder nodes are
  // unknown until the folders are listed, so their positions stay until then.
  useEffect(() => {
    const ids = new Set(map.nodes.map((n) => n.id));
    const keep = (id: string) => ids.has(id) || (folders === undefined && id.startsWith("folder:"));
    if (Object.keys(layout).every(keep)) return;
    save(Object.fromEntries(Object.entries(layout).filter(([id]) => keep(id))));
  }, [map]);

  useEffect(() => {
    if (visible) requestAnimationFrame(() => fitView());
  }, [visible]);

  // Child nodes cover the boundary, so the drop point is tested against its rect.
  const onDrop = useRef(onDropFolder);
  onDrop.current = onDropFolder;
  useEffect(() => {
    const stop = getCurrentWebview().onDragDropEvent(async ({ payload }) => {
      if (payload.type !== "drop" || payload.paths.length === 0) return;
      const point = payload.position.toLogical(window.devicePixelRatio);
      if (!container.current?.contains(document.elementFromPoint(point.x, point.y))) return;
      // The App listener opens the room of a round folder.
      if (await isRoundFolder(payload.paths[0])) return;
      const { x, y } = screenToFlowPosition(point);
      const box = getNode("boundary");
      const width = box?.measured?.width ?? box?.width;
      const height = box?.measured?.height ?? box?.height;
      if (!box || width === undefined || height === undefined) return;
      const { x: left, y: top } = box.position;
      if (x >= left && x <= left + width && y >= top && y <= top + height) onDrop.current(payload.paths[0]);
    });
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  const jump = (node?: MapNode) => node?.type === "item" && node.data.jump && onJump(node.data.jump);

  return (
    <div ref={container} className="map" role="group" aria-label={`Map of shield ${shield.name}`}>
      <ReactFlow<MapNode>
        nodes={nodes}
        edges={map.edges}
        nodeTypes={nodeTypes}
        onNodesChange={onNodesChange}
        onNodeDragStop={(_, __, dragged) =>
          save({ ...layout, ...Object.fromEntries(dragged.map((node) => [node.id, node.position])) })
        }
        onNodeClick={(_, node) => jump(node)}
        onKeyDown={(event) => {
          if (event.key !== "Enter") return;
          const id = (event.target as HTMLElement).closest(".react-flow__node")?.getAttribute("data-id");
          jump(nodes.find((n) => n.id === id));
        }}
        nodesConnectable={false}
        elementsSelectable={false}
        deleteKeyCode={null}
        minZoom={0.3}
        fitView
      >
        <Background variant={BackgroundVariant.Dots} />
        <Panel position="bottom-left">
          <MapControls />
        </Panel>
        <Panel position="top-right">
          <Button size="sm" variant="outline" disabled={Object.keys(layout).length === 0} onClick={() => save(null)}>
            Reset layout
          </Button>
        </Panel>
        {map.hasSecrets && (
          <Panel position="bottom-right" className="map-legend">
            <p className="row">
              <span className="map-badge key">key</span> gets a secret
              <span className="map-node warn map-swatch" /> secret host that the network policy blocks
            </p>
            <p>Agent sees $MSB_…, the shield injects the real value.</p>
          </Panel>
        )}
      </ReactFlow>
    </div>
  );
}
