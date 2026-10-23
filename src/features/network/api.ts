import { listen } from "@tauri-apps/api/event";
import { Action, call, DefaultAction, Inject, Target } from "../../shared/api";

/** A service of the catalog. */
export interface Service {
  id: string;
  name: string;
  /** A lucide icon name. */
  icon: string;
  /** Exact hosts and `*.` wildcards. */
  domains: string[];
  /** The exact hosts that get a credential of the service rule. Empty: the service takes no credential. */
  credentialHosts: string[];
  /** The header and the template that the credential dialog proposes. */
  credential: [string, string] | null;
}

export const networkCatalog = () => call<Service[]>("network_catalog");

export const setNetworkDefault = (id: string, value: DefaultAction) =>
  call<void>("set_network_default", { id, default: value });

export const setOffline = (id: string, offline: boolean) =>
  call<void>("set_offline", { id, offline });

/** No `rule.id`: adds the rule, or changes the rule with the same target. The credentials stay. Resolves to the rule id. */
export const putRule = (
  id: string,
  rule: { id?: string; target: Target; action: Action },
) => call<string>("put_rule", { id, rule });

export const removeRule = (id: string, ruleId: string) =>
  call<void>("remove_rule", { id, ruleId });

/** `inject` null removes the credential. `value` null keeps the stored value; a new name needs one. */
export const setRuleCredential = (
  id: string,
  ruleId: string,
  name: string,
  inject: Inject | null,
  value: string | null,
) => call<void>("set_rule_credential", { id, ruleId, name, inject, value });

export interface NetStage {
  ok: boolean;
  detail: string;
}

/** `target` is null if the project did not reach its proxy. */
export interface NetTest {
  proxy: NetStage;
  target: NetStage | null;
}

/** First the project reaches its proxy, then the host through it. */
export const testNetwork = (id: string, host: string) =>
  call<NetTest>("test_network", { id, host });

/** The wildcard of the host's registrable domain, as `*.bbc.co.uk` for `news.bbc.co.uk`. `null` if there is none. */
export const wildcardFor = (host: string) =>
  call<string | null>("wildcard_for", { host });

/** A request of the proxy log, or a direct connection that the VM blocked. `seq` counts per project; `at` is in ms since the epoch. */
export interface NetEvent {
  seq: number;
  at: number;
  /** `null` for a direct connection. */
  method: string | null;
  host: string;
  /** The path with the names of the query parameters only. */
  path: string | null;
  status: number | null;
  /** Body bytes to the host and from it. */
  up: number;
  down: number;
  ms: number;
  allowed: boolean;
  /** `offline`, `always-blocked`, `rule:<name>`, `default`, `tokeninfo` or `direct`. */
  reason: string;
  /** The names of the injected credentials. */
  credentials: string[];
  /** What else the proxy did, such as `no credential: plain http`. */
  note?: string;
}

/** The events that the backend keeps for the project, oldest first. */
export const listNetEvents = (projectId: string) =>
  call<NetEvent[]>("list_net_events", { projectId });

/** Resolves to the function that stops the listener. */
export const onNetEvent = (
  handler: (projectId: string, event: NetEvent) => void,
) =>
  listen<{ projectId: string; event: NetEvent }>("net-event", ({ payload }) =>
    handler(payload.projectId, payload.event),
  );
