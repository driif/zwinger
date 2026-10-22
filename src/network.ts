import {
  Bot,
  Box,
  Boxes,
  Brain,
  Container,
  GitBranch,
  Globe,
  LucideIcon,
  Package,
  Sparkles,
} from "lucide-react";
import { useEffect, useState } from "react";
import { networkCatalog, NetworkPolicy, Rule, Service, Target } from "./api";

/** The catalog names lucide icons. lucide has no brand icons, so GitHub gets a branch. */
const ICONS: Record<string, LucideIcon> = {
  github: GitBranch,
  package: Package,
  box: Box,
  boxes: Boxes,
  container: Container,
  sparkles: Sparkles,
  bot: Bot,
  brain: Brain,
};

export const serviceIcon = (service?: Service) =>
  (service && ICONS[service.icon]) ?? Globe;

let catalog: Promise<Service[]> | undefined;

/** The service catalog of the app. It does not change while the app runs. */
export function useCatalog() {
  const [services, setServices] = useState<Service[]>([]);
  useEffect(() => {
    catalog ??= networkCatalog();
    catalog.then(setServices, () => (catalog = undefined));
  }, []);
  return services;
}

/** `*.a.io` covers `a.io` and each host below it. */
export function covers(pattern: string, host: string) {
  if (!pattern.startsWith("*.")) return pattern === host;
  const base = pattern.slice(2);
  return host === base || host.endsWith(`.${base}`);
}

export const serviceOf = (target: Target, catalog: Service[]) =>
  target.kind === "service"
    ? catalog.find((s) => s.id === target.id)
    : undefined;

/** The exact hosts and wildcards of the rule. */
export const ruleDomains = (rule: Rule, catalog: Service[]) =>
  rule.target.kind === "host"
    ? [rule.target.host]
    : (serviceOf(rule.target, catalog)?.domains ?? []);

/** The service name or the host, as the log reason `rule:<label>` shows it. */
export const ruleLabel = (rule: Rule, catalog: Service[]) =>
  rule.target.kind === "host"
    ? rule.target.host
    : (serviceOf(rule.target, catalog)?.name ?? rule.target.id);

export const ruleMatches = (rule: Rule, host: string, catalog: Service[]) =>
  ruleDomains(rule, catalog).some((d) => covers(d, host));

/** The hosts that get the rule's credentials. Empty: the rule takes no credential, such as a wildcard host rule. */
export function credentialHosts(rule: Rule, catalog: Service[]) {
  if (rule.target.kind === "service")
    return serviceOf(rule.target, catalog)?.credentialHosts ?? [];
  return rule.target.host.startsWith("*.") ? [] : [rule.target.host];
}

/**
 * The order of the proxy, without the always-blocked addresses, which only a
 * lookup on the Mac shows: offline, Block rules, Allow rules, the default.
 */
export function decide(
  network: NetworkPolicy,
  host: string,
  catalog: Service[],
): { allowed: boolean; rule?: Rule } {
  if (network.offline) return { allowed: false };
  const block = network.rules.find(
    (r) => r.action === "block" && ruleMatches(r, host, catalog),
  );
  if (block) return { allowed: false, rule: block };
  const allow = network.rules.find(
    (r) => r.action === "allow" && ruleMatches(r, host, catalog),
  );
  if (allow) return { allowed: true, rule: allow };
  return { allowed: network.default === "allow" };
}

/** The first single service whose domains cover the host. The bundle of registries is not a single service. */
export const serviceFor = (host: string, catalog: Service[]) =>
  catalog.find(
    (s) => s.id !== "registries" && s.domains.some((d) => covers(d, host)),
  );
