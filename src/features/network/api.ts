import { listen } from "@tauri-apps/api/event";
import { call, type Action, type DefaultAction, type Inject, type Target } from "../../shared/api.ts";

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

export const setNetworkDefault = (cageId: string, value: DefaultAction) =>
  call<void>("set_network_default", { cageId, default: value });

export const setOffline = (cageId: string, offline: boolean) =>
  call<void>("set_offline", { cageId, offline });

/** No `rule.id`: adds the rule, or changes the rule with the same target. The credentials stay. Resolves to the rule id. */
export const putRule = (
  cageId: string,
  rule: { id?: string; target: Target; action: Action },
) => call<string>("put_rule", { cageId, rule });

export const removeRule = (cageId: string, ruleId: string) =>
  call<void>("remove_rule", { cageId, ruleId });

/** `inject` null removes the credential. `value` null keeps the stored value; a new name, or a name with no stored value, needs one. */
export const setRuleCredential = (
  cageId: string,
  ruleId: string,
  name: string,
  inject: Inject | null,
  value: string | null,
) => call<void>("set_rule_credential", { cageId, ruleId, name, inject, value });

/** The names of the cage's credentials that have a stored value. It returns no values. */
export const storedCredentials = (cageId: string) =>
  call<string[]>("stored_credentials", { cageId });

export interface NetStage {
  ok: boolean;
  detail: string;
}

/** `target` is null if the VM did not reach its proxy. */
export interface NetTest {
  proxy: NetStage;
  target: NetStage | null;
}

/** First the VM reaches its proxy, then the host through it. */
export const testNetwork = (cageId: string, host: string) =>
  call<NetTest>("test_network", { cageId, host });

/** The wildcard of the host's registrable domain, as `*.bbc.co.uk` for `news.bbc.co.uk`. `null` if there is none. */
export const wildcardFor = (host: string) =>
  call<string | null>("wildcard_for", { host });

/** A request of the proxy log, or a direct connection that the VM blocked. `seq` counts per cage; `at` is in ms since the epoch. */
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

/** The events that the backend keeps for the VM, oldest first. */
export const listNetEvents = (vmKey: string) => call<NetEvent[]>("list_net_events", { vmKey });

/** Resolves to the function that stops the listener. */
export const onNetEvent = (
  handler: (vmKey: string, event: NetEvent) => void,
) =>
  listen<{ vmKey: string; event: NetEvent }>("net-event", ({ payload }) =>
    handler(payload.vmKey, payload.event),
  );
