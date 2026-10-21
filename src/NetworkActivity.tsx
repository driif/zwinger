import { Button, Card, CardBody, CardHeader, FilterChips } from "@driif/motiv";
import { Ban, Check, KeyRound, Lock, LucideIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { allowHost, isBlocked, NetEvent, ShieldView, wildcardFor } from "./api";
import type { Run } from "./App";
import { reachable } from "./SecretsSection";

const FILTERS = [
  { value: "all", label: "All" },
  { value: "blocked", label: "Blocked" },
  { value: "allowed", label: "Allowed" },
];

interface Props {
  shield: ShieldView;
  events: NetEvent[];
  run: Run;
}

export function NetworkActivity({ shield, events, run }: Props) {
  const { id, network } = shield;
  const [filter, setFilter] = useState("all");
  // Per host: the wildcard, `null` if there is none, or `undefined` while it loads.
  const [wildcards, setWildcards] = useState<Record<string, string | null>>({});
  const secretHosts = new Set(shield.secrets.flatMap((s) => s.hosts));
  const shown = events
    .filter((e) => filter === "all" || isBlocked(e) === (filter === "blocked"))
    .reverse();
  const blockedHosts = [...new Set(events.filter(isBlocked).map((e) => e.host))];
  const missing = blockedHosts.filter((h) => !(h in wildcards)).join(" ");

  useEffect(() => {
    if (!missing) return;
    const hosts = missing.split(" ");
    Promise.all(hosts.map(wildcardFor)).then((found) =>
      setWildcards((known) => ({ ...known, ...Object.fromEntries(hosts.map((h, i) => [h, found[i]])) })),
    );
  }, [missing]);

  function describe(event: NetEvent): [LucideIcon, string, string] {
    if (isBlocked(event)) return [Ban, "blocked", "danger"];
    if (event.kind === "dns_allowed") return [Check, "allowed", "muted"];
    // The SNI only names a host that can get a secret; the log does not show an injection.
    return secretHosts.has(event.host) ? [KeyRound, "secret host", "warn"] : [Lock, "TLS", "muted"];
  }

  function actions(host: string) {
    if (reachable(network, host)) {
      return <span className="muted">{shield.pendingRestart ? "allowed — restart to apply" : "allowed"}</span>;
    }
    const parent = wildcards[host];
    return (
      <>
        <Button size="sm" variant="outline" onClick={() => run(() => allowHost(id, host))}>
          Allow
        </Button>
        {parent && (
          <Button size="sm" variant="ghost" onClick={() => run(() => allowHost(id, parent))}>
            Allow <span className="mono">{parent}</span>
          </Button>
        )}
      </>
    );
  }

  return (
    <Card variant="outlined" padding="md">
      <CardHeader title="Activity" titleAs="h2" description="The hosts that the agent looked up or connected to." />
      <CardBody className="stack">
        <FilterChips
          label="Show"
          showSummary={false}
          options={FILTERS}
          value={[filter]}
          onChange={(selected) => setFilter(selected.find((f) => f !== filter) ?? "all")}
        />
        {shown.length === 0 && <p className="muted">No network activity yet.</p>}
        <ul className="list">
          {shown.map((event) => {
            const [Icon, label, tone] = describe(event);
            return (
              <li key={event.seq} className="row">
                <span className="muted mono">{new Date(event.at).toLocaleTimeString()}</span>
                <span className={`row ${tone}`}>
                  <Icon size={14} />
                  {label}
                </span>
                <span className="grow mono" title={event.host}>
                  {event.host}
                </span>
                {event.dst && <span className="muted mono">{event.dst}</span>}
                {isBlocked(event) && actions(event.host)}
              </li>
            );
          })}
        </ul>
        <p className="muted">
          Not shown: blocked connections to IP addresses and plain HTTP requests. Secret injection is not logged.
        </p>
      </CardBody>
    </Card>
  );
}
