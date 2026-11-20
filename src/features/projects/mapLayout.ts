import type { Edge, Node } from "@xyflow/react";
import { Ban, Bot, Folder, FolderOpen, FolderPlus, FolderSymlink, Globe, KeyRound, Laptop, type LucideIcon, Package, Plus, UserCog } from "lucide-react";
import { GUEST_HOST, type NetEvent, type Service } from "../network/api.ts";
import { AGENT_NAMES, type AgentKind, type CageView, type SessionView, type Target, type VmCage } from "../../shared/api.ts";
import { covers, decide, domainError, ruleDomains, ruleLabel, ruleMatches, serviceIcon, serviceOf } from "../network/network.ts";

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
const CAP = 6;
const ACTIVE_MS = 10_000;
const BLOCKED_MS = 60_000;
const BLOCKED_CAP = 5;

export type Jump = "settings" | "network";

export interface ItemData extends Record<string, unknown> {
  charWidth?: number;
  icon: LucideIcon;
  label: string;
  title: string;
  tone?: "warn" | "blocked" | "denied" | "harness" | "muted";
  badges: string[];
  /** Traffic in the last 10 s. */
  active?: boolean;
  /** The retained entries of the VM that match the host node. */
  count?: number;
  /** The body bytes of the matching proxy requests. Absent when no proxy request matches. */
  bytes?: string;
  jump?: Jump;
  hostTarget?: string;
}

export interface BoundaryData extends Record<string, unknown> {
  name: string;
  state: VmCage["status"]["state"];
}

export type MapNode = Node<ItemData, "item"> | Node<BoundaryData, "boundary"> | Node<{ label: string }, "caption">;

export type MapEdit = { kind: "mount" } | { kind: "addRule"; target?: Target };

export function mapEdit(source: string, target: string | null, nodes: MapNode[]): MapEdit | undefined {
  if (source === "boundary") {
    if (target === null) return { kind: "addRule" };
    if (target === "new-host" && nodes.some((node) => node.id === target)) return { kind: "addRule" };
    const node = nodes.find((item) => item.id === target);
    if (node?.type === "item" && node.data.hostTarget)
      return { kind: "addRule", target: { kind: "host", host: node.data.hostTarget } };
  }
  if (source === "source:new-folder" && target === "boundary" && nodes.some((node) => node.id === source))
    return { kind: "mount" };
}

export interface MapMetrics {
  rowHeight: number;
  space: number;
  charWidth: number;
}

const canon = (host: string) => host.toLowerCase().replace(/\.+$/, "");

const tally = () => ({ proxy: 0, direct: 0, up: 0, down: 0 });
type Tally = ReturnType<typeof tally>;

function addEvent(total: Tally, event: NetEvent) {
  if (event.method === null) {
    total.direct++;
  } else {
    total.proxy++;
    total.up += event.up;
    total.down += event.down;
  }
}

const UNITS = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB", "RB", "QB"];
function size(bytes: number) {
  let value = bytes;
  let unit = 0;
  while (value >= 999.5 && unit < UNITS.length - 1) {
    value /= 1000;
    unit++;
  }
  return `${unit === 0 || value >= 10 ? Math.round(value) : value.toFixed(1)} ${UNITS[unit]}`;
}

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;

function withMetric(data: ItemData, { proxy, direct, up, down }: Tally): ItemData {
  const bytes = proxy
    ? `; proxy body bytes, request and response bodies without headers: ${size(up)} sent, ${size(down)} received, ${size(up + down)} total`
    : "";
  const none = direct ? `. A blocked direct connection has no measured body bytes` : "";
  return {
    ...data,
    count: proxy + direct,
    bytes: proxy ? `↑${size(up)} ↓${size(down)}` : undefined,
    title: `${data.title}. Retained activity of this VM: ${plural(proxy, "proxy request")}, ${plural(direct, "blocked direct connection")}${bytes}${none}`,
  };
}

const under = (path: string, base: string) => path === base || path.startsWith(`${base}/`);

/**
 * The map of a session and its cage: host paths on the left, the microVM boundary with its
 * folders in the middle, egress on the right. `vm` is the cage of the session's VM: its state and runtimes. The auto layout uses fixed
 * columns; a position in `session.layout` replaces it. Child positions are
 * relative to the boundary.
 */
export function mapLayout(
  session: SessionView,
  cage: CageView,
  vm: VmCage | null,
  folders: string[],
  events: NetEvent[],
  catalog: Service[],
  now: number,
  metrics: MapMetrics,
) {
  const H = metrics.rowHeight;
  const ROW = H + metrics.space * 2;
  const GAP = metrics.space * 3;
  const TOP = BOX_Y + H * 2;
  const { harness } = session;
  // In a running VM, the paths that the guest sees. `harness` stays the stored path, which the folders match.
  const guestFolder = session.guestFolder ?? "/workspace";
  const guestHarness = session.guestHarness ?? harness;
  const inGuest = (path: string) => `${guestFolder}${path.slice("/workspace".length)}`;
  const { network } = cage;
  const runtimes = vm?.runtimes ?? [];
  const state = vm?.status.state ?? "stopped";
  const canonicalEvents = events.map((event) => ({ event, host: canon(event.host) }));
  const hosts = new Map<string, { all: Tally; blocked: Tally }>();
  const hostPortTotal = tally();
  for (const { event, host } of canonicalEvents) {
    let totals = hosts.get(host);
    if (!totals) {
      totals = { all: tally(), blocked: tally() };
      hosts.set(host, totals);
    }
    addEvent(totals.all, event);
    if (!event.allowed) addEvent(totals.blocked, event);
    if (host === GUEST_HOST && event.port === network.hostPort) addEvent(hostPortTotal, event);
  }
  const nodes: MapNode[] = [];
  const edges: Edge[] = [];
  const at = (id: string, x: number, y: number) => session.layout[id] ?? { x, y };

  const recent = (event: NetEvent, ms: number) => now - event.at < ms;
  const active = (reason: string) => events.some((e) => e.allowed && e.reason === reason && recent(e, ACTIVE_MS));

  const inner = (id: string, x: number, y: number, width: number, data: ItemData) =>
    nodes.push({
      id,
      type: "item",
      parentId: "boundary",
      extent: "parent",
      position: at(id, x, y - BOX_Y),
      width,
      ariaLabel: data.title,
      data: { ...data, charWidth: metrics.charWidth },
    });
  const outer = (id: string, x: number, y: number, width: number, data: ItemData) =>
    nodes.push({ id, type: "item", position: at(id, x, y), width, ariaLabel: data.title, data: { ...data, charWidth: metrics.charWidth } });
  const edge = (source: string, target: string, blocked?: boolean) =>
    edges.push({ id: `${source}>${target}`, source, target, className: blocked ? "blocked" : undefined });

  const allFolderPaths = folders.map((f) => `/workspace/${f}`);
  const guests = cage.mounts.map((m) => m.guest);
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
    label: guestFolder,
    title: `${guestFolder}, read-write`,
    badges: ["rw"],
    jump: "settings",
  });
  outer("source:/workspace", HOST_X, y, HOST_W, {
    icon: FolderOpen,
    label: session.folder,
    title: session.folder,
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
      title: isHarness ? `${inGuest(path)}, harness ${guestHarness}` : inGuest(path),
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
      title: `harness ${guestHarness}`,
      tone: "harness",
      badges: ["harness"],
      jump: "settings",
    });
  }

  if (cage.mounts.length > 0) y += GAP;
  for (const mount of cage.mounts) {
    y += ROW;
    const isHarness = mount.guest === harnessAt;
    inner(`mount:${mount.guest}`, NODE_X, y, NODE_W, {
      icon: isHarness ? Bot : FolderSymlink,
      label: mount.guest,
      title: `${mount.guest}, ${mount.readonly ? "read-only" : "read-write"}${isHarness ? `, harness ${guestHarness}` : ""}`,
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
  y += ROW;
  outer("source:new-folder", HOST_X, y, HOST_W, {
    icon: FolderPlus,
    label: "New folder from this Mac…",
    title: "Draw to the cage boundary to mount a folder from this Mac",
    tone: "muted",
    badges: [],
  });
  // Not user mounts: zwinger binds its own copy of each agent runtime, and the agent cannot change it.
  y += GAP;
  const fromMac = (kind: AgentKind) => cage.hostAgents.some((a) => a.kind === kind);
  for (const [name, origin] of [
    ["pi", fromMac("pi") ? "from this Mac, or bundled with zwinger if it does not load" : "bundled with zwinger"],
    ...runtimes.map((r) => [r, r === "gemini" ? "from this Mac" : "downloaded by zwinger"] as const),
  ] as const) {
    y += ROW;
    // pi has no permission prompts; the setting applies to the other agents.
    const prompts = name === "pi" ? undefined : cage.agentsUnprompted ? "no prompts" : "prompts";
    inner(`agent:${name}`, NODE_X, y, NODE_W, {
      icon: Package,
      label: `agent runtime · ${AGENT_NAMES[name]}`,
      title: `/opt/zwinger/${name}, ${AGENT_NAMES[name]} ${origin}, read-only${
        prompts ? `; it runs ${cage.agentsUnprompted ? "without" : "with"} permission prompts` : ""
      }`,
      tone: "muted",
      badges: prompts ? ["ro", prompts] : ["ro"],
      jump: "settings",
    });
  }
  // A host agent's profile is a read-only copy of its config dir on this Mac; its login stays on the Mac.
  for (const used of cage.hostAgents) {
    const name = AGENT_NAMES[used.kind];
    if (used.profile) {
      y += ROW;
      inner(`profile:${used.kind}`, NODE_X, y, NODE_W, {
        icon: UserCog,
        label: `profile (ro) · ${name}`,
        title: `/opt/zwinger/profile/${used.kind}, a read-only copy of the ${name} profile on this Mac, without logins, history or sessions`,
        tone: "muted",
        badges: ["ro"],
      });
      outer(`source:profile:${used.kind}`, HOST_X, y, HOST_W, {
        icon: UserCog,
        label: `${name} profile`,
        title: `The ${name} profile on this Mac, copied at each start`,
        badges: [],
      });
      edge(`source:profile:${used.kind}`, `profile:${used.kind}`);
    }
    if (used.login) {
      y += ROW;
      inner(`login:${used.kind}`, NODE_X, y, NODE_W, {
        icon: KeyRound,
        label: `login · ${name}`,
        title: `The ${name} login of this Mac. The agent sees a placeholder; the proxy adds the token to requests to the provider.`,
        tone: "muted",
        badges: ["key"],
        jump: "network",
      });
    }
  }
  // The boundary is first, so React Flow knows the parent before its children.
  nodes.unshift({
    id: "boundary",
    type: "boundary",
    position: at("boundary", BOX_X, BOX_Y),
    width: BOX_W,
    height: y + H + metrics.space * 5 - BOX_Y,
    ariaLabel: `Cage of ${session.name}, ${state}`,
    data: { name: session.name, state },
  });

  let ny = TOP;
  const host = (id: string, data: ItemData, blocked: boolean, total?: Tally) => {
    outer(id, NET_X, ny, NET_W, { ...(total ? withMetric(data, total) : data), jump: "network" });
    edge("boundary", id, blocked);
    ny += total ? ROW + H + metrics.space * 2 : ROW;
  };
  // The default is the edge style of "everything else".
  const closed = network.offline || network.default === "block";
  host(
    "default",
    {
      icon: Globe,
      label: network.offline ? "Offline: all blocked" : `Everything else: ${closed ? "blocked" : "allowed"}`,
      title: network.offline
        ? "Offline: the proxy blocks every request and keeps the rules"
        : `A host that no rule names is ${closed ? "blocked" : "allowed"}`,
      tone: network.offline ? "blocked" : closed ? "muted" : "warn",
      badges: [],
      active: active("default"),
    },
    closed,
  );
  outer("new-host", NET_X, ny, NET_W, {
    icon: Plus,
    label: "New host or service…",
    title: "Draw from the cage gateway to review an Allow rule",
    tone: "muted",
    badges: [],
  });
  ny += ROW;
  const loginsOf = (domains: string[]) =>
    cage.logins.filter((l) => l.hosts.some((h) => domains.some((d) => covers(d, h))));
  for (const rule of network.rules) {
    const service = serviceOf(rule.target, catalog);
    const label = ruleLabel(rule, catalog);
    const domains = ruleDomains(rule, catalog);
    const keys = [...(rule.credentials ?? []).map((c) => c.name), ...loginsOf(domains).map((l) => l.name)];
    const blocked = rule.action === "block" || network.offline;
    const targets = domains.map(canon);
    const total = tally();
    for (const [hostName, values] of hosts) {
      if (!targets.some((target) => covers(target, hostName))) continue;
      total.proxy += values.all.proxy;
      total.direct += values.all.direct;
      total.up += values.all.up;
      total.down += values.all.down;
    }
    host(
      `rule:${rule.id}`,
      {
        icon: rule.action === "block" ? Ban : serviceIcon(service),
        label,
        title: `${rule.action === "allow" ? "Allowed" : "Blocked"}: ${domains.join(", ")}${keys.length ? `; gets ${keys.join(", ")}` : ""}`,
        tone: rule.action === "block" ? "denied" : undefined,
        badges: keys.length ? ["key"] : [],
        active: active(`rule:${label}`),
        hostTarget: rule.action === "block" && rule.target.kind === "host" && !rule.target.host.startsWith("*.") && !domainError(canon(rule.target.host))
          ? canon(rule.target.host) : undefined,
      },
      blocked,
      total,
    );
  }
  if (network.hostPort !== undefined) {
    const url = `http://${GUEST_HOST}:${network.hostPort}`;
    // As in the proxy, Offline and a matching Block rule come before the grant.
    const block = network.rules.find((r) => r.action === "block" && ruleMatches(r, GUEST_HOST, catalog));
    const reach = `${url} reaches 127.0.0.1:${network.hostPort} of this Mac`;
    host(
      "host-port",
      {
        icon: Laptop,
        label: `Service on this Mac · ${network.hostPort}`,
        title: network.offline
          ? `${reach}, but the cage is offline`
          : block
            ? `${reach}, but the rule ${ruleLabel(block, catalog)} blocks it`
            : `${reach}, plain HTTP only`,
        tone: network.offline || block ? "blocked" : "warn",
        badges: [],
        active: active("host-port"),
      },
      network.offline || block !== undefined,
      hostPortTotal,
    );
  }
  // A host login that no rule names is shown on its own, as nothing else names it.
  const named = network.rules.flatMap((r) => ruleDomains(r, catalog));
  for (const login of cage.logins)
    for (const loginHost of login.hosts.filter((h) => !named.some((d) => covers(d, h)))) {
      const ok = decide(network, loginHost, catalog).allowed;
      const agent = AGENT_NAMES[login.origin.agent];
      host(
        `login-host:${loginHost}`,
        {
          icon: KeyRound,
          label: loginHost,
          title: ok ? `${loginHost} gets the ${agent} login` : `${loginHost} gets the ${agent} login, but it is blocked`,
          tone: ok ? undefined : "warn",
          badges: ["key"],
          hostTarget: !domainError(canon(loginHost)) ? canon(loginHost) : undefined,
        },
        !ok,
        hosts.get(canon(loginHost))?.all ?? tally(),
      );
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
      canonicalEvents
        .filter(({ event }) => !event.allowed && recent(event, BLOCKED_MS))
        .reverse()
        .map(({ host }) => host),
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
        active: canonicalEvents.some(({ event, host }) => !event.allowed && recent(event, ACTIVE_MS) && host === denied),
        hostTarget: !domainError(denied) && !/^\d+(?:\.\d+){3}$/.test(denied) ? denied : undefined,
      },
      true,
      hosts.get(denied)!.blocked,
    );

  for (const [id, label, x] of [
    ["caption:host", "HOST", HOST_X],
    ["caption:network", "NETWORK", NET_X],
  ] as const)
    nodes.push({ id, type: "caption", position: { x, y: H }, draggable: false, selectable: false, focusable: false, data: { label } });

  const hasSecrets = cage.logins.length > 0 || network.rules.some((r) => (r.credentials ?? []).length > 0);
  return { nodes, edges, hasSecrets };
}
