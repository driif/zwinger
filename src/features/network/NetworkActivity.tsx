import { Button, Card, CardBody, CardHeader, FilterChips } from "@driif/motiv";
import { Ban, Check, Download, KeyRound, Play, X } from "lucide-react";
import { useEffect, useState } from "react";
import { exportNetEvents, NetEvent, putRule, Service, setHostPort, wildcardFor } from "./api";
import { Action, CageView, Target } from "../../shared/api";
import type { Run } from "../../App";
import { decide, serviceFor } from "./network";

const FILTERS = [
  { value: "all", label: "All" },
  { value: "blocked", label: "Blocked" },
  { value: "allowed", label: "Allowed" },
];

const REASONS: Record<string, string> = {
  offline: "offline",
  "always-blocked": "always blocked",
  default: "the default",
  tokeninfo: "login check",
  "host-port": "service on this Mac",
  method: "method refused",
  misdirected: "named another host than its connection",
  direct: "direct connection blocked — this tool ignores the proxy",
};

const reasonText = (reason: string) =>
  reason.startsWith("rule:")
    ? `rule ${reason.slice("rule:".length)}`
    : (REASONS[reason] ?? reason);

interface Group {
  /** The host, with its port for the gateway name of this Mac. */
  key: string;
  host: string;
  count: number;
  last: NetEvent;
  credentials: string[];
}

interface Props {
  /** The effective cage: a rule that the view adds goes to it. `null`: the view adds no rules. */
  cage: CageView | null;
  /** The key of the session's VM. `null`: the session has no VM key. */
  vmKey: string | null;
  /** The session's VM runs. */
  running: boolean;
  events: NetEvent[];
  catalog: Service[];
  /** Starts the session's VM; it asks first when the VM runs for other sessions. */
  onStart: () => void;
  run: Run;
}

export function NetworkActivity({ cage, vmKey, running, events, catalog, onStart, run }: Props) {
  const [filter, setFilter] = useState("all");
  // Per host: the wildcard, `null` if there is none. A host that is not here is still loading.
  const [wildcards, setWildcards] = useState<Record<string, string | null>>({});

  const groups = new Map<string, Group>();
  for (const event of events) {
    if (filter !== "all" && event.allowed !== (filter === "allowed")) continue;
    const key =
      event.port === undefined ? event.host : `${event.host}:${event.port}`;
    const group = groups.get(key) ?? {
      key,
      host: event.host,
      count: 0,
      last: event,
      credentials: [],
    };
    group.count += 1;
    group.last = event;
    group.credentials = [
      ...new Set([...group.credentials, ...event.credentials]),
    ];
    groups.set(key, group);
  }
  const shown = [...groups.values()].sort((a, b) => b.last.at - a.last.at);
  const missing = shown
    .filter((g) => g.last.port === undefined)
    .map((g) => g.host)
    .filter((h) => !(h in wildcards))
    .join(" ");

  useEffect(() => {
    if (!missing) return;
    const hosts = missing.split(" ");
    Promise.all(hosts.map(wildcardFor)).then((found) =>
      setWildcards((known) => ({
        ...known,
        ...Object.fromEntries(hosts.map((h, i) => [h, found[i]])),
      })),
    );
  }, [missing]);

  function actions({ host, last }: Group) {
    if (!cage) return null;
    // The gateway name of this Mac takes no rule: only the host port grant opens it.
    if (last.port !== undefined)
      return cage.network.hostPort === last.port ? (
        <Button
          size="sm"
          variant="outline"
          iconLeft={<X size={14} />}
          onClick={() => run(() => setHostPort(cage.id, null))}
        >
          Remove
        </Button>
      ) : null;
    // A rule does not open the floor, and a tool that ignores the proxy stays blocked.
    if (
      last.reason === "always-blocked" ||
      last.reason === "direct" ||
      last.reason === "tokeninfo"
    )
      return null;
    // Keep the method list of an existing rule for this target.
    const set = (target: Target, action: Action) => {
      const existing = cage.network.rules.find((rule) =>
        target.kind === "host"
          ? rule.target.kind === "host" && rule.target.host.toLowerCase() === target.host.toLowerCase()
          : rule.target.kind === "service" && rule.target.id === target.id,
      );
      return run(() => putRule(cage.id, { id: existing?.id, target, action, methods: existing?.methods }));
    };
    const action: Action = decide(cage.network, host, catalog).allowed
      ? "block"
      : "allow";
    const verb = action === "allow" ? "Allow" : "Block";
    const parent = wildcards[host];
    const service = serviceFor(host, catalog);
    return (
      <>
        {service && (
          <Button
            size="sm"
            variant="outline"
            onClick={() => set({ kind: "service", id: service.id }, action)}
          >
            Part of {service.name} — {verb} {service.name}
          </Button>
        )}
        <Button
          size="sm"
          variant={service ? "ghost" : "outline"}
          onClick={() => set({ kind: "host", host }, action)}
        >
          {verb} this host
        </Button>
        {parent && (
          <Button
            size="sm"
            variant="ghost"
            onClick={() => set({ kind: "host", host: parent }, action)}
          >
            {verb} <span className="mono">{parent}</span>
          </Button>
        )}
      </>
    );
  }

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Activity"
        titleAs="h2"
        description="The requests of the session's VM, by host. The newest host is first."
        actions={
          vmKey && (
            <Button
              size="sm"
              variant="ghost"
              iconLeft={<Download size={14} />}
              onClick={() => run(() => exportNetEvents(vmKey))}
            >
              Export
            </Button>
          )
        }
      />
      <CardBody className="stack">
        <FilterChips
          label="Show"
          showSummary={false}
          options={FILTERS}
          value={[filter]}
          onChange={(selected) =>
            setFilter(selected.find((f) => f !== filter) ?? "all")
          }
        />
        {shown.length === 0 &&
          (events.length > 0 ? (
            <p className="muted">No {filter} activity.</p>
          ) : running ? (
            <p className="muted">
              No network activity yet. The hosts show here when the agent
              reaches out.
            </p>
          ) : cage ? (
            <div className="row">
              <p className="muted grow">
                No network activity. Start the cage to see the hosts that the
                agent reaches.
              </p>
              <Button
                size="sm"
                variant="outline"
                iconLeft={<Play size={14} />}
                onClick={onStart}
              >
                Start
              </Button>
            </div>
          ) : (
            <p className="muted">No network activity.</p>
          ))}
        <ul className="list">
          {shown.map((group) => {
            const { last } = group;
            return (
              <li key={group.key} className="stack">
                <div className="row">
                  <span className="muted mono">
                    {new Date(last.at).toLocaleTimeString()}
                  </span>
                  <span className={`row ${last.allowed ? "ok" : "danger"}`}>
                    {last.allowed ? <Check size={14} /> : <Ban size={14} />}
                    {last.allowed ? "allowed" : "blocked"}
                  </span>
                  <span className="grow mono" title={group.key}>
                    {group.key}
                  </span>
                  <span className="muted">×{group.count}</span>
                </div>
                <div className="row">
                  <span className="grow muted">
                    <span className="mono">
                      {last.method} {last.path}
                    </span>{" "}
                    {last.status !== null && `${last.status} · `}
                    {reasonText(last.reason)}
                    {last.note && ` · ${last.note}`}
                  </span>
                  {group.credentials.length > 0 && (
                    <span
                      className="warn row"
                      title="The proxy added these credentials"
                    >
                      <KeyRound size={14} />
                      {group.credentials.join(", ")}
                    </span>
                  )}
                  {actions(group)}
                </div>
              </li>
            );
          })}
        </ul>
        <p className="muted">
          The log keeps the names of the query parameters, never their values,
          headers or bodies.
        </p>
      </CardBody>
    </Card>
  );
}
