import type { NetEvent } from "./api.ts";

export type ActivityFilter = "all" | "allowed" | "blocked";

export interface ActivityGroup {
  /** The host, with its port for the gateway name of this Mac. */
  key: string;
  host: string;
  count: number;
  last: NetEvent;
  credentials: string[];
}

/** Groups the events by host, newest host first. `search` keeps the hosts that contain it, in any case. */
export function groupActivity(events: NetEvent[], filter: ActivityFilter, search: string): ActivityGroup[] {
  const needle = search.trim().toLowerCase();
  const groups = new Map<string, ActivityGroup>();
  for (const event of events) {
    if (filter !== "all" && event.allowed !== (filter === "allowed")) continue;
    const key = event.port === undefined ? event.host : `${event.host}:${event.port}`;
    if (!key.toLowerCase().includes(needle)) continue;
    const group = groups.get(key) ?? { key, host: event.host, count: 0, last: event, credentials: [] };
    group.count += 1;
    group.last = event;
    group.credentials = [...new Set([...group.credentials, ...event.credentials])];
    groups.set(key, group);
  }
  return [...groups.values()].sort((a, b) => b.last.at - a.last.at);
}
