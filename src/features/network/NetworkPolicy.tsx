import {
  Button,
  Card,
  CardBody,
  CardHeader,
  Confirm,
  CopyField,
  FilterChips,
  FloatingField,
  LiquidSwitch,
  Select,
  SegmentedControl,
  SpringCheckbox,
} from "@driif/motiv";
import { ChevronDown, ChevronRight, KeyRound, Laptop, Plus, TriangleAlert, X } from "lucide-react";
import { FormEvent, ReactNode, useEffect, useRef, useState } from "react";
import { GUEST_HOST, putRule, setHostPort, setNetworkDefault, setOffline } from "./api";
import { Action, AGENT_NAMES, AgentNeed, CageView, DefaultAction, Rule, Target } from "../../shared/api";
import type { Intent, Run } from "../../App";
import { PolicyProfiles } from "./PolicyProfiles";
import { RuleCredentials } from "./RuleCredentials";
import { errorMessage } from "../../shared/errorMessage";
import { removeRuleWithUndo } from "../../shared/undo";
import { covers, credentialHosts, domainError, ruleDomains, ruleLabel, serviceIcon, serviceOf, useCatalog } from "./network";

/** The methods that the rule editor offers. No choice allows all methods. */
const METHODS = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"].map((m) => ({ value: m, label: m }));

interface Props {
  cage: CageView;
  run: Run;
  /** The hosts that the agents of a session use. The editor offers to allow the blocked ones. */
  needs?: AgentNeed[];
  intent?: Intent;
  onIntentDone?: () => void;
  /** The network test, at the end of the Internet access card. */
  test?: ReactNode;
}

/** The network policy editor of a cage. An edit applies to the next request of each running VM of the cage. */
export function NetworkPolicy({ cage, run, needs = [], intent, onIntentDone, test }: Props) {
  const { id, network } = cage;
  const catalog = useCatalog();
  const [adding, setAdding] = useState<"service" | "host">();
  const [host, setHost] = useState("");
  const [problem, setProblem] = useState<string>();
  const [mapDraft, setMapDraft] = useState<Target>();
  const [mapProblem, setMapProblem] = useState<string>();
  const [reviewCredentials, setReviewCredentials] = useState(false);
  const [mapSaving, setMapSaving] = useState(false);
  const savingMap = useRef(false);
  const [expanded, setExpanded] = useState<string>();
  const [credentialsOf, setCredentialsOf] = useState<string>();
  const [methodsOf, setMethodsOf] = useState<string>();
  const hostField = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (intent?.kind !== "addRule") return;
    onIntentDone?.();
    if (intent.map) {
      setMapDraft(intent.target ?? { kind: "host", host: "" });
      setMapProblem(undefined);
      setReviewCredentials(false);
    } else {
      setAdding("host");
    }
  }, [intent]);

  useEffect(() => {
    if (adding === "host") hostField.current?.focus();
  }, [adding]);

  const add = (target: Target, action: Action) =>
    run(() => putRule(id, { target, action }));

  function submitHost(action: Action) {
    const error = domainError(host);
    if (error) return setProblem(error);
    run(() =>
      putRule(id, { target: { kind: "host", host }, action }).then(
        () => {
          setHost("");
          setAdding(undefined);
        },
        (e) => setProblem(errorMessage(e)),
      ),
    );
  }

  const loginsOf = (rule: Rule) =>
    cage.logins.filter((l) =>
      l.hosts.some((h) => ruleDomains(rule, catalog).some((d) => covers(d, h))),
    );
  const blockedNeeds = needs.filter((need) => !need.allowed);
  const unused = catalog.filter(
    (s) =>
      !network.rules.some(
        (r) => r.target.kind === "service" && r.target.id === s.id,
      ),
  );
  const target = mapDraft?.kind === "host"
    ? { kind: "host" as const, host: mapDraft.host.trim().toLowerCase() }
    : mapDraft?.id ? mapDraft : undefined;
  const existing = target && network.rules.find((rule) =>
    rule.target.kind === target.kind && (target.kind === "host"
      ? rule.target.kind === "host" && rule.target.host.toLowerCase().replace(/\.+$/, "") === target.host.replace(/\.+$/, "")
      : rule.target.kind === "service" && rule.target.id === target.id));
  const domains = target?.kind === "host" ? [target.host] : catalog.find((service) => service.id === target?.id)?.domains ?? [];
  const remainingBlocks = network.rules.filter((rule) => rule.action === "block" && rule.id !== existing?.id &&
    domains.some((domain) => ruleDomains(rule, catalog).some((blocked) => covers(blocked, domain) || covers(domain, blocked))));
  const credentials = existing?.action === "block" ? existing.credentials ?? [] : [];

  function saveMapRule(event: FormEvent) {
    event.preventDefault();
    if (!target || savingMap.current) return;
    if (target.kind === "host") {
      const error = domainError(target.host);
      if (error) return setMapProblem(error);
    }
    if (existing?.action === "allow" || (credentials.length > 0 && !reviewCredentials)) return;
    savingMap.current = true;
    setMapSaving(true);
    run(() => putRule(id, { id: existing?.id, target, action: "allow", methods: existing?.methods }).then(
      () => setMapDraft(undefined),
      (error) => setMapProblem(errorMessage(error)),
    ).finally(() => {
      savingMap.current = false;
      setMapSaving(false);
    }));
  }

  return (
    <>
      <Card variant="outlined" padding="md">
        <CardHeader title="Internet access" titleAs="h2" />
        <CardBody className="stack">
          <SegmentedControl
            label="Everything else"
            options={[
              { value: "block", label: "Block everything else" },
              { value: "ask", label: "Ask for everything else" },
              { value: "allow", label: "Allow everything else" },
            ]}
            value={network.default}
            onChange={(value) =>
              run(() => setNetworkDefault(id, value as DefaultAction))
            }
          />
          {network.default === "block" ? (
            <p className="muted">A host that no rule allows is blocked.</p>
          ) : network.default === "ask" ? (
            <p className="muted">
              A request that no rule decides waits up to 30 seconds for your
              answer in this tab. With no answer, it is blocked.
            </p>
          ) : (
            <p className="warn row">
              <TriangleAlert size={14} />A host that no rule blocks is allowed.
              Only the always-blocked groups stay closed.
            </p>
          )}
          <LiquidSwitch
            label="Offline: block all requests now and keep the rules"
            checked={network.offline}
            onChange={(on) => run(() => setOffline(id, on))}
          />
          {test}
        </CardBody>
      </Card>

      <HostPort key={id} cageId={id} port={network.hostPort} run={run} />

      <Card variant="outlined" padding="md">
        <CardHeader
          title="Services and hosts"
          titleAs="h2"
          description="The exceptions to the default."
        />
        <CardBody className="stack">
          {mapDraft && (
            <Card variant="subtle" padding="md">
              <CardHeader title="Review Allow rule" titleAs="h3" description={`Allow a target in cage ${cage.name}.`} />
              <CardBody className="stack">
                <form className="stack" onSubmit={saveMapRule}>
                  <SegmentedControl
                    label="Target type"
                    options={[{ value: "host", label: "Host" }, { value: "service", label: "Service" }]}
                    value={mapDraft.kind}
                    onChange={(kind) => {
                      if (mapSaving) return;
                      setMapDraft(kind === "host" ? { kind: "host", host: "" } : { kind: "service", id: "" });
                      setMapProblem(undefined);
                      setReviewCredentials(false);
                    }}
                  />
                  {mapDraft.kind === "host" ? (
                    <FloatingField
                      label="Host"
                      className="mono"
                      value={mapDraft.host}
                      hint={mapProblem}
                      aria-invalid={mapProblem !== undefined}
                      showValidity={false}
                      disabled={mapSaving}
                      onChange={(event) => {
                        setMapDraft({ kind: "host", host: event.target.value });
                        setMapProblem(undefined);
                        setReviewCredentials(false);
                      }}
                    />
                  ) : (
                    <Select
                      label="Service"
                      options={[{ value: "", label: "Choose a service" }, ...catalog.map((service) => ({ value: service.id, label: service.name }))]}
                      value={mapDraft.id}
                      disabled={mapSaving}
                      onChange={(value) => {
                        setMapDraft({ kind: "service", id: value });
                        setMapProblem(undefined);
                        setReviewCredentials(false);
                      }}
                    />
                  )}
                  {mapProblem && mapDraft.kind === "service" && <p className="danger">{mapProblem}</p>}
                  {target && existing && (
                    <p>
                      This target has a {existing.action === "block" ? "Block" : "Allow"} rule.
                      {existing.action === "block" && " Saving changes that rule to Allow."}
                    </p>
                  )}
                  {network.offline && <p className="warn">Offline still blocks all requests after this save.</p>}
                  {remainingBlocks.length > 0 && (
                    <p className="warn">Other Block rules still take priority for matching hosts: {remainingBlocks.map((rule) => ruleLabel(rule, catalog)).join(", ")}.</p>
                  )}
                  {credentials.length > 0 && (
                    <SpringCheckbox
                      label={`I reviewed that allowing this rule activates ${credentials.map((credential) => credential.name).join(", ")}.`}
                      checked={reviewCredentials}
                      disabled={mapSaving}
                      onChange={(event) => setReviewCredentials(event.target.checked)}
                    />
                  )}
                  <div className="row end">
                    <Button variant="ghost" disabled={mapSaving} onClick={() => setMapDraft(undefined)}>Cancel</Button>
                    <Button type="submit" loading={mapSaving} disabled={mapSaving || !target || existing?.action === "allow" || (credentials.length > 0 && !reviewCredentials)}>Save Allow rule</Button>
                  </div>
                </form>
              </CardBody>
            </Card>
          )}
          {blockedNeeds.map((need) => {
            const name =
              catalog.find((s) => s.id === need.service)?.name ?? need.service;
            return (
              <div key={`${need.agent}:${need.host}`} className="row">
                <span className="warn row grow">
                  <TriangleAlert size={14} />
                  {AGENT_NAMES[need.agent]} uses {name} (
                  <span className="mono">{need.host}</span>), which is blocked.
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() =>
                    add({ kind: "service", id: need.service }, "allow")
                  }
                >
                  Allow {name}
                </Button>
              </div>
            );
          })}
          {network.rules.length === 0 && (
            <p className="muted">No rules. Every host gets the default.</p>
          )}
          <ul className="list">
            {network.rules.map((rule) => {
              const service = serviceOf(rule.target, catalog);
              const Icon = serviceIcon(service);
              const domains = ruleDomains(rule, catalog);
              const credentials = rule.credentials ?? [];
              // On a Block rule the credentials are inactive: the proxy never adds them.
              const inactive =
                rule.action === "block" && credentials.length > 0;
              const takesCredentials =
                credentialHosts(rule, catalog).length > 0 &&
                (rule.action === "allow" || inactive);
              const logins = loginsOf(rule);
              const other: Action = rule.action === "allow" ? "block" : "allow";
              return (
                <li key={rule.id} className="stack">
                  <div className="row">
                    <Icon size={14} />
                    <span
                      className={service ? "grow" : "grow mono"}
                      title={domains.join(", ")}
                    >
                      {ruleLabel(rule, catalog)}
                    </span>
                    <span className={rule.action === "allow" ? "ok" : "danger"}>
                      {rule.action === "allow" ? "Allowed" : "Blocked"}
                    </span>
                    {rule.action === "allow" && (
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => setMethodsOf(methodsOf === rule.id ? undefined : rule.id)}
                      >
                        {rule.methods ? rule.methods.join(", ") : "All methods"}
                      </Button>
                    )}
                    {service && (
                      <Button
                        size="sm"
                        variant="ghost"
                        iconLeft={
                          expanded === rule.id ? (
                            <ChevronDown size={14} />
                          ) : (
                            <ChevronRight size={14} />
                          )
                        }
                        onClick={() =>
                          setExpanded(
                            expanded === rule.id ? undefined : rule.id,
                          )
                        }
                      >
                        {domains.length} domains
                      </Button>
                    )}
                    {logins.map((l) => (
                      <span key={l.name} className="muted row">
                        <KeyRound size={14} />
                        Your Mac login: {AGENT_NAMES[l.origin.agent]}
                      </span>
                    ))}
                    {takesCredentials && (
                      <Button
                        size="sm"
                        variant="ghost"
                        iconLeft={<KeyRound size={14} />}
                        onClick={() =>
                          setCredentialsOf(
                            credentialsOf === rule.id ? undefined : rule.id,
                          )
                        }
                      >
                        {credentials.length > 0
                          ? credentials.map((c) => c.name).join(", ")
                          : "Add token…"}
                        {inactive && " (inactive)"}
                      </Button>
                    )}
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => {
                        if (other === "allow" && credentials.length > 0) {
                          setMapDraft(rule.target);
                          setMapProblem(undefined);
                          setReviewCredentials(false);
                        } else run(() =>
                          putRule(id, {
                            id: rule.id,
                            target: rule.target,
                            action: other,
                            methods: rule.methods,
                          }),
                        );
                      }}
                    >
                      {other === "allow" ? credentials.length > 0 ? "Review Allow" : "Allow" : "Block"}
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={`Remove ${ruleLabel(rule, catalog)}`}
                      iconLeft={<X size={14} />}
                      onClick={() => removeRuleWithUndo(run, id, rule.id, ruleLabel(rule, catalog))}
                    />
                  </div>
                  {expanded === rule.id && (
                    <p className="muted mono">{domains.join(", ")}</p>
                  )}
                  {methodsOf === rule.id && rule.action === "allow" && (
                    <FilterChips
                      label={`Methods of ${ruleLabel(rule, catalog)}`}
                      options={METHODS}
                      value={rule.methods ?? []}
                      summary={(count) =>
                        count === 0 ? "All methods" : "Only these methods; the others are blocked"
                      }
                      clearLabel="All methods"
                      onChange={(methods) =>
                        run(() =>
                          putRule(id, {
                            id: rule.id,
                            target: rule.target,
                            action: rule.action,
                            methods: methods.length > 0 ? methods : undefined,
                          }),
                        )
                      }
                    />
                  )}
                  {credentialsOf === rule.id && takesCredentials && (
                    <RuleCredentials
                      cage={cage}
                      rule={rule}
                      catalog={catalog}
                      run={run}
                    />
                  )}
                </li>
              );
            })}
          </ul>

          {adding === "service" && (
            <ul className="list">
              {unused.map((service) => {
                const Icon = serviceIcon(service);
                return (
                  <li key={service.id} className="row">
                    <Icon size={14} />
                    <span className="grow" title={service.domains.join(", ")}>
                      {service.name}
                    </span>
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() =>
                        add({ kind: "service", id: service.id }, "allow")
                      }
                    >
                      Allow
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() =>
                        add({ kind: "service", id: service.id }, "block")
                      }
                    >
                      Block
                    </Button>
                  </li>
                );
              })}
            </ul>
          )}
          {adding === "host" && (
            <form
              className="row"
              onSubmit={(e) => {
                e.preventDefault();
                submitHost("allow");
              }}
            >
              <FloatingField
                ref={hostField}
                label="Host or *.domain"
                className="grow mono"
                value={host}
                hint={problem}
                aria-invalid={problem !== undefined}
                showValidity={false}
                onChange={(e) => {
                  setHost(e.target.value);
                  setProblem(undefined);
                }}
              />
              <Button type="submit" variant="outline">
                Allow
              </Button>
              <Button variant="ghost" onClick={() => submitHost("block")}>
                Block
              </Button>
            </form>
          )}
          <div className="row">
            <Button
              variant={adding === "service" ? "secondary" : "outline"}
              iconLeft={<Plus size={14} />}
              onClick={() =>
                setAdding(adding === "service" ? undefined : "service")
              }
            >
              Add service
            </Button>
            <Button
              variant={adding === "host" ? "secondary" : "outline"}
              iconLeft={<Plus size={14} />}
              onClick={() => setAdding(adding === "host" ? undefined : "host")}
            >
              Add host
            </Button>
          </div>
        </CardBody>
      </Card>

      <PolicyProfiles cageId={id} network={network} run={run} />
    </>
  );
}

interface HostPortProps {
  cageId: string;
  /** The granted port. `undefined`: no grant. */
  port?: number;
  run: Run;
}

/** The host port grant: one port of this Mac's loopback, for a service such as Ollama. */
function HostPort({ cageId, port, run }: HostPortProps) {
  const [input, setInput] = useState("");
  const [problem, setProblem] = useState<string>();
  const [confirming, setConfirming] = useState(false);
  const wanted = Number(input.trim());
  const valid = /^\d+$/.test(input.trim()) && wanted >= 1 && wanted <= 65535;

  function grant() {
    setConfirming(false);
    run(() =>
      setHostPort(cageId, wanted).then(
        () => setInput(""),
        (e) => setProblem(errorMessage(e)),
      ),
    );
  }

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Service on this Mac"
        titleAs="h2"
        description="Let the agent reach one port of this Mac, such as Ollama on 11434."
      />
      <CardBody className="stack">
        {port === undefined ? (
          <form
            className="row"
            onSubmit={(e) => {
              e.preventDefault();
              if (!valid) return setProblem("Use a port from 1 to 65535.");
              setConfirming(true);
            }}
          >
            <FloatingField
              label="Port on 127.0.0.1"
              className="grow mono"
              inputMode="numeric"
              value={input}
              hint={problem}
              aria-invalid={problem !== undefined}
              showValidity={false}
              onChange={(e) => {
                setInput(e.target.value);
                setProblem(undefined);
              }}
            />
            <Button
              type="submit"
              variant="outline"
              iconLeft={<Laptop size={14} />}
              disabled={input.trim() === ""}
            >
              Allow…
            </Button>
          </form>
        ) : (
          <>
            <div className="row">
              <CopyField
                className="grow mono"
                label="URL in the cage"
                value={`http://${GUEST_HOST}:${port}`}
              />
              <Button
                variant="ghost"
                iconLeft={<X size={14} />}
                onClick={() => run(() => setHostPort(cageId, null))}
              >
                Remove
              </Button>
            </div>
            <p className="muted">
              This grant lets the agent send plain HTTP to 127.0.0.1:{port}
              of this Mac. Offline or a Block rule for {GUEST_HOST} still
              blocks it. Remove blocks the next request. A response that
              streams now continues to its end.
            </p>
          </>
        )}
        <Confirm
          open={confirming}
          title={`Allow port ${valid ? wanted : ""} of this Mac?`}
          confirmLabel="Allow"
          onConfirm={grant}
          onCancel={() => setConfirming(false)}
        >
          <div className="stack">
            <p className="warn row">
              <TriangleAlert size={14} />
              The service acts as your Mac user. The agent can do all that the
              service lets a local client do.
            </p>
            <p>
              Only plain HTTP to 127.0.0.1 (IPv4) goes through. HTTPS,
              WebSocket and a service that listens on ::1 only do not work.
              No credential is added.
            </p>
          </div>
        </Confirm>
      </CardBody>
    </Card>
  );
}
