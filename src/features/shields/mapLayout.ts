import type { Edge, Node } from "@xyflow/react";
import { Ban, Bot, Folder, FolderOpen, FolderSymlink, Globe, KeyRound, LucideIcon, Package, UserCog } from "lucide-react";
import { NetEvent, Service } from "../network/api";
import { AGENT_NAMES, AgentKind, ShieldView } from "../../shared/api";
import { covers, decide, ruleDomains, ruleLabel, serviceIcon, serviceOf } from "../network/network";

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
export function mapLayout(shield: ShieldView, folders: string[], events: NetEvent[], catalog: Service[], now: number) {
  const { harness, network } = shield;
  const nodes: MapNode[] = [];
  const edges: Edge[] = [];
  const at = (id: string, x: number, y: number) => shield.layout[id] ?? { x, y };

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
  // Not user mounts: zwinger binds its own copy of each agent runtime, and the agent cannot change it.
  y += GAP;
  const fromMac = (kind: AgentKind) => shield.hostAgents.some((a) => a.kind === kind);
  for (const [name, origin] of [
    ["pi", fromMac("pi") ? "from this Mac, or bundled with zwinger if it does not load" : "bundled with zwinger"],
    ...shield.runtimes.map((r) => [r, r === "gemini" ? "from this Mac" : "downloaded by zwinger"] as const),
  ] as const) {
    y += ROW;
    // pi has no permission prompts; the setting applies to the other agents.
    const prompts = name === "pi" ? undefined : shield.agentsUnprompted ? "no prompts" : "prompts";
    inner(`agent:${name}`, NODE_X, y, NODE_W, {
      icon: Package,
      label: `agent runtime · ${AGENT_NAMES[name]}`,
      title: `/opt/zwinger/${name}, ${AGENT_NAMES[name]} ${origin}, read-only${
        prompts ? `; it runs ${shield.agentsUnprompted ? "without" : "with"} permission prompts` : ""
      }`,
      tone: "muted",
      badges: prompts ? ["ro", prompts] : ["ro"],
      jump: "settings",
    });
  }
  // A host agent's profile is a read-only copy of its config dir on this Mac; its login stays on the Mac.
  for (const used of shield.hostAgents) {
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
    height: y + H + 20 - BOX_Y,
    ariaLabel: `Shield ${shield.name}, microVM ${shield.status.state}`,
    data: { name: shield.name, state: shield.status.state },
  });

  let ny = TOP;
  const host = (id: string, data: ItemData, blocked?: boolean) => {
    outer(id, NET_X, ny, NET_W, { ...data, jump: "network" });
    edge("boundary", id, blocked);
    ny += ROW;
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
  const loginsOf = (domains: string[]) =>
    shield.logins.filter((l) => l.hosts.some((h) => domains.some((d) => covers(d, h))));
  for (const rule of network.rules) {
    const service = serviceOf(rule.target, catalog);
    const label = ruleLabel(rule, catalog);
    const domains = ruleDomains(rule, catalog);
    const keys = [...(rule.credentials ?? []).map((c) => c.name), ...loginsOf(domains).map((l) => l.name)];
    const blocked = rule.action === "block" || network.offline;
    host(
      `rule:${rule.id}`,
      {
        icon: rule.action === "block" ? Ban : serviceIcon(service),
        label,
        title: `${rule.action === "allow" ? "Allowed" : "Blocked"}: ${domains.join(", ")}${keys.length ? `; gets ${keys.join(", ")}` : ""}`,
        tone: rule.action === "block" ? "denied" : undefined,
        badges: keys.length ? ["key"] : [],
        active: active(`rule:${label}`),
      },
      blocked,
    );
  }
  // A host login that no rule names is shown on its own, as nothing else names it.
  const named = network.rules.flatMap((r) => ruleDomains(r, catalog));
  for (const login of shield.logins)
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
        },
        !ok,
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
      events
        .filter((e) => !e.allowed && recent(e, BLOCKED_MS))
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
        active: events.some((e) => !e.allowed && recent(e, ACTIVE_MS) && e.host === denied),
      },
      true,
    );

  for (const [id, label, x] of [
    ["caption:host", "HOST", HOST_X],
    ["caption:network", "NETWORK", NET_X],
  ] as const)
    nodes.push({ id, type: "caption", position: { x, y: 28 }, draggable: false, selectable: false, focusable: false, data: { label } });

  const hasSecrets = shield.logins.length > 0 || network.rules.some((r) => (r.credentials ?? []).length > 0);
  return { nodes, edges, hasSecrets };
}
