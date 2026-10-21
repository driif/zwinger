import { Ban, Bot, Folder, FolderOpen, FolderSymlink, Globe, LucideIcon, Network, Shield } from "lucide-react";
import { KeyboardEvent, ReactNode } from "react";
import { ShieldView } from "./api";
import type { Tab } from "./App";

const WIDTH = 900;
const HOST_X = 16;
const HOST_W = 230;
const BOX_X = 300;
const BOX_W = 300;
const NODE_X = 320;
const NODE_W = 260;
const FOLDER_X = 344;
const FOLDER_W = 236;
const NET_X = 660;
const NET_W = 224;
const TOP = 72;
const ROW = 36;
const GAP = 12;
const H = 28;
const CAP = 6;
const CHAR = 7.2;

/** Keeps the end of a path, which is the part that tells folders apart. */
function fit(text: string, width: number) {
  const max = Math.floor(width / CHAR);
  return text.length > max ? `…${text.slice(-(max - 1))}` : text;
}

const under = (path: string, base: string) => path === base || path.startsWith(`${base}/`);

interface NodeProps {
  x: number;
  y: number;
  width: number;
  icon: LucideIcon;
  label: string;
  title?: string;
  tone?: "warn" | "blocked" | "harness" | "muted";
  badges?: string[];
  onJump?: () => void;
}

function Node({ x, y, width, icon: Icon, label, title, tone, badges = [], onJump }: NodeProps) {
  const textWidth = width - 36 - badges.length * 56;
  const button = onJump && {
    role: "button",
    tabIndex: 0,
    "aria-label": title ?? label,
    onClick: onJump,
    onKeyDown: (event: KeyboardEvent) => {
      if (event.key !== "Enter" && event.key !== " ") return;
      event.preventDefault();
      onJump();
    },
  };
  return (
    <g className={`map-node ${tone ?? ""}`} {...button}>
      <title>{title ?? label}</title>
      <rect x={x} y={y} width={width} height={H} rx={6} />
      <Icon x={x + 8} y={y + 7} size={14} />
      <text x={x + 28} y={y + 18}>
        {fit(label, textWidth)}
      </text>
      {badges.map((badge, i) => {
        const bx = x + width - 56 * (i + 1);
        return (
          <g key={badge} className={`map-badge ${badge}`}>
            <rect x={bx} y={y + 6} width={50} height={16} rx={8} />
            <text x={bx + 25} y={y + 18} textAnchor="middle">
              {badge}
            </text>
          </g>
        );
      })}
    </g>
  );
}

function Line({ from, to, blocked }: { from: [number, number]; to: [number, number]; blocked?: boolean }) {
  const mid = (from[0] + to[0]) / 2;
  return (
    <path
      className={blocked ? "map-line blocked" : "map-line"}
      d={`M${from[0]} ${from[1]} C${mid} ${from[1]} ${mid} ${to[1]} ${to[0]} ${to[1]}`}
    />
  );
}

interface Props {
  shield: ShieldView;
  folders: string[];
  onJump: (tab: Tab) => void;
}

/** Host paths on the left, the microVM in the middle, egress on the right. */
export function ShieldMap({ shield, folders, onJump }: Props) {
  const { harness, network, status } = shield;
  const toSettings = () => onJump("settings");
  const toNetwork = () => onJump("network");
  const center = (y: number) => y + H / 2;

  const allFolderPaths = folders.map((f) => `/workspace/${f}`);
  const guests = shield.mounts.map((m) => m.guest);
  // The deepest mount wins, as in the backend; mounts are never below /workspace.
  const harnessAt =
    harness === null
      ? undefined
      : (guests.filter((g) => under(harness, g)).sort((a, b) => b.length - a.length)[0] ??
        allFolderPaths.find((p) => under(harness, p)));
  const more = (paths: string[], noun: string) =>
    `+${paths.length - CAP} more${noun}${harnessAt && paths.slice(CAP).includes(harnessAt) ? ", 1 with harness" : ""}`;
  const folderPaths = allFolderPaths.slice(0, CAP);
  const mounts = shield.mounts;
  const harnessBadge = (path: string) => (path === harnessAt ? ["harness"] : []);

  const inside: ReactNode[] = [];
  const outside: ReactNode[] = [];
  const lines: ReactNode[] = [];
  let y = TOP;

  inside.push(
    <Node
      key="workspace"
      x={NODE_X}
      y={y}
      width={NODE_W}
      icon={FolderOpen}
      label="/workspace"
      badges={["rw"]}
      onJump={toSettings}
    />,
  );
  outside.push(
    <Node key="folder" x={HOST_X} y={y} width={HOST_W} icon={FolderOpen} label={shield.folder} onJump={toSettings} />,
  );
  lines.push(<Line key="folder" from={[HOST_X + HOST_W, center(y)]} to={[NODE_X, center(y)]} />);

  for (const path of folderPaths) {
    y += ROW;
    inside.push(
      <Node
        key={path}
        x={FOLDER_X}
        y={y}
        width={FOLDER_W}
        icon={path === harnessAt ? Bot : Folder}
        label={path.slice("/workspace/".length)}
        title={path === harnessAt ? `${path}, harness ${harness}` : path}
        tone={path === harnessAt ? "harness" : "muted"}
        badges={harnessBadge(path)}
        onJump={toSettings}
      />,
    );
  }
  if (folders.length > CAP) {
    y += ROW;
    inside.push(
      <text key="folders-more" className="map-more" x={FOLDER_X + 8} y={y + 18}>
        {more(allFolderPaths, "")}
      </text>,
    );
  }

  // A hidden folder, a folder past the listing limit, or folders that are still loading.
  if (harness !== null && harnessAt === undefined) {
    y += ROW;
    inside.push(
      <Node
        key="harness"
        x={FOLDER_X}
        y={y}
        width={FOLDER_W}
        icon={Bot}
        label={harness.slice("/workspace/".length)}
        title={`harness ${harness}`}
        tone="harness"
        badges={["harness"]}
        onJump={toSettings}
      />,
    );
  }

  if (mounts.length > 0) y += GAP;
  for (const mount of mounts) {
    y += ROW;
    const access = mount.readonly ? "ro" : "rw";
    inside.push(
      <Node
        key={mount.guest}
        x={NODE_X}
        y={y}
        width={NODE_W}
        icon={mount.guest === harnessAt ? Bot : FolderSymlink}
        label={mount.guest}
        title={`${mount.guest}, ${mount.readonly ? "read-only" : "read-write"}${mount.guest === harnessAt ? `, harness ${harness}` : ""}`}
        tone={mount.guest === harnessAt ? "harness" : undefined}
        badges={[...harnessBadge(mount.guest), access]}
        onJump={toSettings}
      />,
    );
    outside.push(
      <Node key={mount.guest} x={HOST_X} y={y} width={HOST_W} icon={Folder} label={mount.host} onJump={toSettings} />,
    );
    lines.push(<Line key={mount.guest} from={[HOST_X + HOST_W, center(y)]} to={[NODE_X, center(y)]} />);
  }
  const boxBottom = y + H + 20;

  const gateway: [number, number] = [BOX_X + BOX_W, center(TOP)];
  const net: ReactNode[] = [];
  let ny = TOP;
  if (network.mode === "public") {
    net.push(
      <Node key="internet" x={NET_X} y={ny} width={NET_W} icon={Globe} label="Public internet" tone="warn" onJump={toNetwork} />,
    );
    lines.push(<Line key="internet" from={gateway} to={[NET_X, center(ny)]} />);
    ny += ROW;
  } else if (network.mode === "deny" || network.domains.length === 0) {
    net.push(
      <Node
        key="none"
        x={NET_X}
        y={ny}
        width={NET_W}
        icon={Globe}
        label={network.mode === "deny" ? "Deny all" : "No hosts allowed"}
        tone="muted"
        onJump={toNetwork}
      />,
    );
    ny += ROW;
  } else {
    for (const domain of network.domains) {
      net.push(<Node key={domain} x={NET_X} y={ny} width={NET_W} icon={Globe} label={domain} onJump={toNetwork} />);
      lines.push(<Line key={domain} from={gateway} to={[NET_X, center(ny)]} />);
      ny += ROW;
    }
  }
  ny += GAP;
  net.push(
    <Node
      key="blocked"
      x={NET_X}
      y={ny}
      width={NET_W}
      icon={Ban}
      label="Host · LAN · metadata"
      title="Always blocked: this computer, local network, loopback, link-local and cloud metadata"
      tone="blocked"
      onJump={toNetwork}
    />,
  );
  lines.push(<Line key="blocked" from={gateway} to={[NET_X, center(ny)]} blocked />);
  const height = Math.max(boxBottom, ny + H) + 16;

  return (
    <svg
      className="map"
      viewBox={`0 0 ${WIDTH} ${height}`}
      role="group"
      aria-label={`Map of shield ${shield.name}`}
    >
      <text className="map-caption" x={HOST_X} y={40}>
        HOST
      </text>
      <text className="map-caption" x={NET_X} y={40}>
        NETWORK
      </text>
      <g className={`map-boundary ${status.state}`}>
        <rect x={BOX_X} y={16} width={BOX_W} height={boxBottom - 16} rx={14} />
        <Shield x={BOX_X + 16} y={26} size={18} />
        <text className="map-title" x={BOX_X + 42} y={40}>
          {fit(shield.name, BOX_W - 140)}
        </text>
        <text className="map-state" x={BOX_X + BOX_W - 16} y={40} textAnchor="end">
          microVM · {status.state}
        </text>
      </g>
      {lines}
      <g className="map-gateway">
        <circle cx={gateway[0]} cy={gateway[1]} r={11} />
        <Network x={gateway[0] - 7} y={gateway[1] - 7} size={14} />
      </g>
      {outside}
      {inside}
      {net}
    </svg>
  );
}
