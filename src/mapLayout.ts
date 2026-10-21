import type { Edge, Node } from "@xyflow/react";
import { Ban, Bot, Folder, FolderOpen, FolderSymlink, Globe, KeyRound, LucideIcon, Package } from "lucide-react";
import { isBlocked, NetEvent, ShieldView } from "./api";
import { coveredBy, reachable } from "./SecretsSection";

const HOST_X = 16;
const HOST_W = 230;
const BOX_X = 300;
const BOX_Y = 16;
const BOX_W = 300;
const NODE_X = 20;
const NODE_W = 260;
const FOLDER_X = 44;
const FOLDER_W = 236;
const NET_X = 660;
const NET_W = 224;
const TOP = 72;
const ROW = 36;
const GAP = 12;
const H = 28;
const CAP = 6;
const ACTIVE_MS = 10_000;
const BLOCKED_MS = 60_000;
const BLOCKED_CAP = 5;

export type Jump = "settings" | "network";

export interface ItemData extends Record<string, unknown> {
  icon: LucideIcon;
  label: string;
  title: string;
  tone?: "warn" | "blocked" | "denied" | "harness" | "muted";
  badges: string[];
  /** Traffic in the last 10 s. */
  active?: boolean;
  jump?: Jump;
}

export interface BoundaryData extends Record<string, unknown> {
  name: string;
  state: ShieldView["status"]["state"];
}

export type MapNode = Node<ItemData, "item"> | Node<BoundaryData, "boundary"> | Node<{ label: string }, "caption">;

const under = (path: string, base: string) => path === base || path.startsWith(`${base}/`);

/**
 * The map of a shield: host paths on the left, the microVM boundary with its
 * folders in the middle, egress on the right. The auto layout uses fixed
 * columns; a position in `shield.layout` replaces it. Child positions are
 * relative to the boundary.
 */
export function mapLayout(shield: ShieldView, folders: string[], events: NetEvent[], now: number) {
  const { harness, network } = shield;
  const nodes: MapNode[] = [];
  const edges: Edge[] = [];
  const at = (id: string, x: number, y: number) => shield.layout[id] ?? { x, y };

  const recent = (event: NetEvent, ms: number) => now - event.at < ms;
  const active = (domain: string) =>
    events.some((e) => !isBlocked(e) && recent(e, ACTIVE_MS) && coveredBy(domain, [e.host]));

  const inner = (id: string, x: number, y: number, width: number, data: ItemData) =>
    nodes.push({
      id,
      type: "item",
      parentId: "boundary",
      extent: "parent",
      position: at(id, x, y - BOX_Y),
      width,
      ariaLabel: data.title,
      data,
    });
  const outer = (id: string, x: number, y: number, width: number, data: ItemData) =>
    nodes.push({ id, type: "item", position: at(id, x, y), width, ariaLabel: data.title, data });
  const edge = (source: string, target: string, blocked?: boolean) =>
    edges.push({ id: `${source}>${target}`, source, target, className: blocked ? "blocked" : undefined });

  const allFolderPaths = folders.map((f) => `/workspace/${f}`);
  const guests = shield.mounts.map((m) => m.guest);
  // The deepest mount wins, as in the backend; mounts are never below /workspace.
  const harnessAt =
    harness === null
      ? undefined
      : (guests.filter((g) => under(harness, g)).sort((a, b) => b.length - a.length)[0] ??
        allFolderPaths.find((p) => under(harness, p)));
  const harnessBadge = (path: string) => (path === harnessAt ? ["harness"] : []);

  let y = TOP;
  inner("workspace", NODE_X, y, NODE_W, {
    icon: FolderOpen,
    label: "/workspace",
    title: "/workspace, read-write",
    badges: ["rw"],
    jump: "settings",
  });
  outer("source:/workspace", HOST_X, y, HOST_W, {
    icon: FolderOpen,
    label: shield.folder,
    title: shield.folder,
    badges: [],
    jump: "settings",
  });
  edge("source:/workspace", "workspace");

  for (const path of allFolderPaths.slice(0, CAP)) {
    y += ROW;
    const isHarness = path === harnessAt;
    inner(`folder:${path.slice("/workspace/".length)}`, FOLDER_X, y, FOLDER_W, {
      icon: isHarness ? Bot : Folder,
      label: path.slice("/workspace/".length),
      title: isHarness ? `${path}, harness ${harness}` : path,
      tone: isHarness ? "harness" : "muted",
      badges: harnessBadge(path),
      jump: "settings",
    });
  }
  if (folders.length > CAP) {
    y += ROW;
    const hidden = allFolderPaths.slice(CAP);
    inner("folder:+more", FOLDER_X, y, FOLDER_W, {
      icon: Folder,
      label: `+${hidden.length} more${harnessAt && hidden.includes(harnessAt) ? ", 1 with harness" : ""}`,
      title: hidden.join(", "),
      tone: "muted",
      badges: [],
    });
  }

  // A hidden folder, a folder past the listing limit, or folders that are still loading.
  if (harness !== null && harnessAt === undefined) {
    y += ROW;
    inner("harness", FOLDER_X, y, FOLDER_W, {
      icon: Bot,
      label: harness.slice("/workspace/".length),
      title: `harness ${harness}`,
      tone: "harness",
      badges: ["harness"],
      jump: "settings",
    });
  }

  if (shield.mounts.length > 0) y += GAP;
  for (const mount of shield.mounts) {
    y += ROW;
    const isHarness = mount.guest === harnessAt;
    inner(`mount:${mount.guest}`, NODE_X, y, NODE_W, {
      icon: isHarness ? Bot : FolderSymlink,
      label: mount.guest,
      title: `${mount.guest}, ${mount.readonly ? "read-only" : "read-write"}${isHarness ? `, harness ${harness}` : ""}`,
      tone: isHarness ? "harness" : undefined,
      badges: [...harnessBadge(mount.guest), mount.readonly ? "ro" : "rw"],
      jump: "settings",
    });
    outer(`source:${mount.guest}`, HOST_X, y, HOST_W, {
      icon: Folder,
      label: mount.host,
      title: mount.host,
      badges: [],
      jump: "settings",
    });
    edge(`source:${mount.guest}`, `mount:${mount.guest}`);
  }
  // Not a user mount: zwinger binds its own copy of pi, and the agent cannot change it.
  y += GAP + ROW;
  inner("agent:pi", NODE_X, y, NODE_W, {
    icon: Package,
    label: "agent runtime",
    title: "/opt/zwinger/pi, the pi bundled with zwinger, read-only",
    tone: "muted",
    badges: ["ro"],
  });
  // The boundary is first, so React Flow knows the parent before its children.
  nodes.unshift({
    id: "boundary",
    type: "boundary",
    position: at("boundary", BOX_X, BOX_Y),
    width: BOX_W,
    height: y + H + 20 - BOX_Y,
    ariaLabel: `Shield ${shield.name}, microVM ${shield.status.state}`,
    data: { name: shield.name, state: shield.status.state },
  });

  const secretHosts = [...new Set(shield.secrets.flatMap((s) => s.hosts))];
  const secretNames = (host: string) =>
    shield.secrets
      .filter((s) => s.hosts.includes(host))
      .map((s) => s.name)
      .join(", ");

  let ny = TOP;
  const host = (id: string, data: ItemData, blocked?: boolean) => {
    outer(id, NET_X, ny, NET_W, { ...data, jump: "network" });
    edge("boundary", id, blocked);
    ny += ROW;
  };
  if (network.mode === "public") {
    host("host:*", {
      icon: Globe,
      label: "Public internet",
      title: "Public internet",
      tone: "warn",
      badges: [],
      active: events.some((e) => !isBlocked(e) && recent(e, ACTIVE_MS)),
    });
  } else if (network.mode === "deny" || network.domains.length === 0) {
    const label = network.mode === "deny" ? "Deny all" : "No hosts allowed";
    outer("host:none", NET_X, ny, NET_W, { icon: Globe, label, title: label, tone: "muted", badges: [], jump: "network" });
    ny += ROW;
  } else {
    for (const domain of network.domains)
      host(`host:${domain}`, {
        icon: Globe,
        label: domain,
        title: domain,
        badges: coveredBy(domain, secretHosts) ? ["key"] : [],
        active: active(domain),
      });
  }
  // In List mode the allow-list nodes carry the key badges. In Public mode the
  // secret hosts are shown on their own, as nothing else names them.
  for (const secretHost of secretHosts) {
    const ok = reachable(network, secretHost);
    if (ok && network.mode === "list") continue;
    const id = `secret:${secretHost}`;
    const data: ItemData = {
      icon: KeyRound,
      label: secretHost,
      title: ok
        ? `${secretHost} gets ${secretNames(secretHost)}`
        : `${secretHost} gets ${secretNames(secretHost)}, but it is not on the allow list`,
      tone: ok ? undefined : "warn",
      badges: ["key"],
      active: ok && active(secretHost),
      jump: "network",
    };
    outer(id, NET_X, ny, NET_W, data);
    if (ok) edge("boundary", id);
    ny += ROW;
  }
  ny += GAP;
  host(
    "group:blocked",
    {
      icon: Ban,
      label: "Host · LAN · metadata",
      title: "Always blocked: this computer, local network, loopback, link-local and cloud metadata",
      tone: "blocked",
      badges: [],
    },
    true,
  );
  const blockedHosts = [
    ...new Set(
      events
        .filter((e) => isBlocked(e) && recent(e, BLOCKED_MS))
        .reverse()
        .map((e) => e.host),
    ),
  ].slice(0, BLOCKED_CAP);
  for (const denied of blockedHosts)
    host(
      `denied:${denied}`,
      {
        icon: Ban,
        label: denied,
        title: `${denied} was blocked in the last minute`,
        tone: "denied",
        badges: [],
        active: events.some((e) => isBlocked(e) && recent(e, ACTIVE_MS) && e.host === denied),
      },
      true,
    );

  for (const [id, label, x] of [
    ["caption:host", "HOST", HOST_X],
    ["caption:network", "NETWORK", NET_X],
  ] as const)
    nodes.push({ id, type: "caption", position: { x, y: 28 }, draggable: false, selectable: false, focusable: false, data: { label } });

  return { nodes, edges, hasSecrets: secretHosts.length > 0 };
}
