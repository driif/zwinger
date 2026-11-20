import assert from "node:assert/strict";
import { test } from "node:test";
import type { CageView, Rule, SessionView } from "../../shared/api.ts";
import { GUEST_HOST, type NetEvent, type Service } from "../network/api.ts";
import { type ItemData, mapEdit, mapLayout } from "./mapLayout.ts";

const session = {
  name: "Example",
  folder: "/projects/example",
  guestFolder: "/workspace/example",
  harness: "/workspace/src/tools",
  guestHarness: "/workspace/example/src/tools",
  layout: {},
} as SessionView;
const cage: CageView = {
  id: "dev", name: "Dev", mounts: [], hostAgents: [], logins: [], agentsUnprompted: false,
  network: { default: "block", offline: false, rules: [] }, claudeToken: null, vms: [],
  resources: { cpus: 1, memoryMib: 512 },
};
const metrics = { rowHeight: 24, space: 4, charWidth: 8 };

test("the map uses shared VM paths and marks the folder that contains the harness", () => {
  const { nodes } = mapLayout(session, cage, null, ["s", "src"], [], [], 0, metrics);
  assert.equal(nodes.find((node) => node.id === "workspace")?.data.label, "/workspace/example");
  const folder = nodes.find((node) => node.id === "folder:src");
  assert.equal(folder?.data.title, "/workspace/example/src, harness /workspace/example/src/tools");
  assert.deepEqual(folder?.data.badges, ["harness"]);
  assert.deepEqual(nodes.find((node) => node.id === "folder:s")?.data.badges, []);
});

test("the map shows the host port grant, and offline or a Block rule blocks it", () => {
  const granted = { ...cage, network: { ...cage.network, hostPort: 11434 } };
  const node = (c: CageView) => mapLayout(session, c, null, [], [], [], 0, metrics).nodes.find((n) => n.id === "host-port");
  assert.equal(node(cage), undefined);
  assert.equal(node(granted)?.data.label, "Service on this Mac · 11434");
  assert.match(String(node(granted)?.data.title), /^http:\/\/host\.microsandbox\.internal:11434 reaches 127\.0\.0\.1:11434/);
  const offline = { ...granted, network: { ...granted.network, offline: true } };
  assert.equal(node(offline)?.data.tone, "blocked");
  const rule = { id: "r", target: { kind: "host", host: "*.microsandbox.internal" }, action: "block" } as const;
  const ruled = { ...granted, network: { ...granted.network, rules: [rule] } };
  assert.equal(node(ruled)?.data.tone, "blocked");
  assert.match(String(node(ruled)?.data.title), /but the rule \*\.microsandbox\.internal blocks it\. /);
});

test("a dragged folder keeps its saved position inside the boundary", () => {
  const layout = { "folder:src": { x: 13, y: 26 } };
  const { nodes } = mapLayout({ ...session, layout }, cage, null, ["src"], [], [], 0, metrics);
  const folder = nodes.find((node) => node.id === "folder:src");
  assert.equal(folder?.parentId, "boundary");
  assert.deepEqual(folder?.position, { x: 13, y: 26 });
});

const event = (seq: number, host: string, extra: Partial<NetEvent> = {}): NetEvent => ({
  seq, at: seq, method: "GET", host, path: "/", status: 200, up: 10, down: 20, ms: 1, allowed: true,
  reason: "rule:outdated-human-label", credentials: [], ...extra,
});
const direct = (seq: number, host: string, extra: Partial<NetEvent> = {}) =>
  event(seq, host, { method: null, path: null, status: null, up: 0, down: 0, allowed: false, reason: "direct", ...extra });
const withRules = (rules: Rule[], extra: Partial<CageView["network"]> = {}): CageView => ({
  ...cage, network: { ...cage.network, rules, ...extra },
});
const hostRule = (id: string, host: string): Rule => ({ id, target: { kind: "host", host }, action: "allow" });
const data = (c: CageView, events: NetEvent[], id: string, catalog: Service[] = [], now = 0) =>
  mapLayout(session, c, null, [], events, catalog, now, metrics).nodes.find((n) => n.id === id)?.data as ItemData | undefined;

test("only the new folder node can draw a mount to the cage boundary", () => {
  const map = mapLayout(session, cage, null, ["src"], [], [], 0, metrics);
  assert.equal(map.nodes.find((n) => n.id === "source:new-folder")?.type, "item");
  assert.deepEqual(mapEdit("source:new-folder", "boundary", map.nodes), { kind: "mount" });
  assert.equal(mapEdit("workspace", "boundary", map.nodes), undefined);
  assert.equal(mapEdit("folder:src", "boundary", map.nodes), undefined);
  assert.equal(mapEdit("source:new-folder", "workspace", map.nodes), undefined);
  assert.equal(map.edges.some((edge) => edge.source === "source:new-folder"), false);
  const saved = { ...cage, mounts: [{ host: "/tmp/data", guest: "/mnt/data", readonly: true, id: null }] };
  const next = mapLayout(session, saved, null, ["src"], [], [], 0, metrics);
  assert.equal(next.edges.some((edge) => edge.source === "source:/mnt/data" && edge.target === "mount:/mnt/data"), true);
});

test("the gateway targets only eligible exact hosts and a blank canvas", () => {
  const rules: Rule[] = [
    { id: "exact", target: { kind: "host", host: "blocked.test" }, action: "block" },
    { id: "wild", target: { kind: "host", host: "*.wild.test" }, action: "block" },
    { id: "service", target: { kind: "service", id: "svc" }, action: "block" },
  ];
  const map = mapLayout(session, withRules(rules), null, [], [direct(1, "fresh.test"), direct(2, "127.0.0.1")], [], 10, metrics);
  assert.deepEqual(mapEdit("boundary", "rule:exact", map.nodes), { kind: "addRule", target: { kind: "host", host: "blocked.test" } });
  assert.deepEqual(mapEdit("boundary", "denied:fresh.test", map.nodes), { kind: "addRule", target: { kind: "host", host: "fresh.test" } });
  assert.deepEqual(mapEdit("boundary", "new-host", map.nodes), { kind: "addRule" });
  assert.deepEqual(mapEdit("boundary", null, map.nodes), { kind: "addRule" });
  assert.equal(map.edges.some((edge) => edge.target === "new-host"), false);
  assert.equal(map.edges.some((edge) => edge.target === "rule:new"), false);
  const saved = mapLayout(session, withRules([...rules, hostRule("new", "fresh.test")]), null, [], [], [], 10, metrics);
  assert.equal(saved.edges.some((edge) => edge.source === "boundary" && edge.target === "rule:new"), true);
  assert.equal(mapEdit("boundary", "rule:new", saved.nodes), undefined);
  for (const target of ["default", "group:blocked", "host-port", "rule:wild", "rule:service", "folder:src", "denied:absent", "denied:127.0.0.1"])
    assert.equal(mapEdit("boundary", target, map.nodes), undefined);
  assert.equal(map.edges.some((edge) => edge.target === "denied:fresh.test"), true);
  assert.equal(map.edges.some((edge) => edge.target === "rule:exact"), true);
  assert.equal(map.edges.some((edge) => edge.target === "rule:wild"), true);
});

test("a wildcard counts its apex and subdomains, at the domain boundary", () => {
  const events = [
    event(1, "example.test"),
    event(2, "a.example.test"),
    event(3, "b.a.example.test"),
    direct(4, "c.example.test"),
    event(5, "notexample.test"),
    event(6, "example.test.evil.test"),
  ];
  const node = data(withRules([hostRule("w", "*.example.test")]), events, "rule:w");
  assert.equal(node?.count, 4);
  assert.equal(node?.bytes, "↑30 B ↓60 B");
  assert.match(String(node?.title), /3 proxy requests, 1 blocked direct connection; proxy body bytes, request and response bodies without headers: 30 B sent, 60 B received, 90 B total\. A blocked direct connection has no measured body bytes$/);
});

test("a service counts an entry once, also when several of its domains match it", () => {
  const service = { id: "svc", name: "Svc", domains: ["api.example.test", "*.example.test", "example.test"] } as Service;
  const rules: Rule[] = [{ id: "s", target: { kind: "service", id: "svc" }, action: "allow" }, hostRule("h", "api.example.test")];
  const events = [event(1, "api.example.test"), event(2, "example.test"), event(3, "x.example.test")];
  assert.equal(data(withRules(rules), events, "rule:s", [service])?.count, 3);
  // A different node can count the same entry.
  assert.equal(data(withRules(rules), events, "rule:h", [service])?.count, 1);
});

test("host case and a trailing dot do not change the match", () => {
  const events = [event(1, "API.Example.Test."), direct(2, "api.example.test")];
  const node = data(withRules([hostRule("e", "api.example.test.")]), events, "rule:e");
  assert.equal(node?.count, 2);
  assert.equal(node?.bytes, "↑10 B ↓20 B");
  const login = { ...cage, logins: [{ name: "L", hosts: ["login.synthetic.test"], origin: { agent: "claude" } }] } as CageView;
  assert.equal(data(login, [event(1, "LOGIN.Synthetic.Test.")], "login-host:login.synthetic.test")?.count, 1);
});

test("the host port node counts only its own port, also blocked requests", () => {
  const granted = withRules([], { hostPort: 11434 });
  const events = [
    event(1, GUEST_HOST, { port: 11434, up: 100, down: 200, reason: "host-port" }),
    event(2, GUEST_HOST.toUpperCase(), { port: 11434, up: 1, down: 2, allowed: false, reason: "offline" }),
    event(3, GUEST_HOST, { port: 11435 }),
  ];
  const node = data(granted, events, "host-port");
  assert.equal(node?.count, 2);
  assert.equal(node?.bytes, "↑101 B ↓202 B");
});

test("a recently denied host counts only its blocked entries, by its canonical host", () => {
  const events = [
    event(1, "denied.test"),
    direct(2, "Denied.Test."),
    direct(3, "denied.test"),
    event(4, "DENIED.TEST", { allowed: false, up: 3, down: 4 }),
  ];
  const { nodes } = mapLayout(session, cage, null, [], events, [], 10, metrics);
  assert.deepEqual(nodes.filter((n) => n.id.startsWith("denied:")).map((n) => n.id), ["denied:denied.test"]);
  const node = data(cage, events, "denied:denied.test", [], 10);
  assert.equal(node?.count, 3);
  assert.equal(node?.bytes, "↑3 B ↓4 B");
});

test("native byte-counter limits keep sent and received labels bounded", () => {
  const maximum = Number("18446744073709551615");
  const events = Array.from({ length: 2000 }, (_, index) =>
    event(index + 1, "large.test", { up: maximum, down: maximum }));
  const node = data(withRules([hostRule("large", "large.test")]), events, "rule:large");
  assert.equal(node?.count, 2000);
  assert.equal(node?.bytes, "↑37 ZB ↓37 ZB");
  assert.match(String(node?.title), /37 ZB sent, 37 ZB received, 74 ZB total/);
});

test("zero, direct-only, measured-zero and mixed entries differ", () => {
  const rules = ["empty", "direct", "zero", "mixed"].map((n) => hostRule(n, `${n}.test`));
  const events = [
    direct(1, "direct.test"),
    event(2, "zero.test", { up: 0, down: 0 }),
    event(3, "mixed.test", { up: 1500, down: 2_500_000 }),
    direct(4, "mixed.test"),
  ];
  const node = (id: string) => data(withRules(rules), events, `rule:${id}`);
  assert.equal(node("empty")?.count, 0);
  assert.equal(node("empty")?.bytes, undefined);
  assert.equal(node("direct")?.count, 1);
  assert.equal(node("direct")?.bytes, undefined);
  assert.doesNotMatch(String(node("direct")?.title), /sent/);
  assert.match(String(node("direct")?.title), /1 blocked direct connection\. A blocked direct connection has no measured body bytes$/);
  assert.doesNotMatch(String(node("zero")?.title), /no measured/);
  assert.equal(node("zero")?.bytes, "↑0 B ↓0 B");
  assert.equal(node("mixed")?.count, 2);
  assert.equal(node("mixed")?.bytes, "↑1.5 kB ↓2.5 MB");
  assert.match(String(node("mixed")?.title), /1 proxy request, 1 blocked direct connection; proxy body bytes, request and response bodies without headers: .*\. A blocked direct connection has no measured body bytes$/);
  // Category nodes have no metric.
  assert.equal(data(withRules(rules), events, "default")?.count, undefined);
  assert.equal(data(withRules(rules), events, "group:blocked")?.count, undefined);
});

test("counts use the given events of the VM, also old rows of a stopped VM", () => {
  const rules = withRules([hostRule("a", "api.test")]);
  const old = [event(1, "api.test"), event(2, "api.test")];
  // Long after the activity dot and the denied nodes expire.
  const node = data(rules, old, "rule:a", [], 3_600_000);
  assert.equal(node?.count, 2);
  assert.equal(node?.active, false);
  assert.equal(data(rules, [event(9, "api.test")], "rule:a")?.count, 1);
  assert.equal(data(rules, [], "rule:a")?.count, 0);
});
