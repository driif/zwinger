import {
  Button,
  Card,
  CardBody,
  CardHeader,
  Confirm,
  CopyField,
  FloatingField,
  LiquidSwitch,
  SegmentedControl,
} from "@driif/motiv";
import {
  Ban,
  Check,
  ChevronDown,
  ChevronRight,
  KeyRound,
  Laptop,
  Plus,
  TriangleAlert,
  X,
} from "lucide-react";
import { FormEvent, ReactNode, useEffect, useRef, useState } from "react";
import {
  GUEST_HOST,
  NetEvent,
  NetStage,
  NetTest,
  putRule,
  Service,
  setHostPort,
  setNetworkDefault,
  setOffline,
  testNetwork,
} from "./api";
import {
  Action,
  AGENT_NAMES,
  DefaultAction,
  Rule,
  CageView,
  VmCage,
  SessionView,
  Target,
} from "../../shared/api";
import type { Intent, Run } from "../../App";
import { NetworkActivity } from "./NetworkActivity";
import { RuleCredentials } from "./RuleCredentials";
import { errorMessage } from "../../shared/errorMessage";
import { removeRuleWithUndo } from "../../shared/undo";
import {
  covers,
  credentialHosts,
  ruleDomains,
  ruleLabel,
  serviceIcon,
  serviceOf,
  useCatalog,
} from "./network";

/** The groups that the proxy blocks after it resolves the host on this Mac. */
const ALWAYS_BLOCKED = [
  { label: "This computer", hint: "127.0.0.0/8, ::1" },
  { label: "Local network", hint: "10/8, 172.16/12, 192.168/16, fc00::/7" },
  { label: "Shared and link-local", hint: "100.64/10, 169.254/16, fe80::/10" },
  { label: "Cloud metadata", hint: "169.254.169.254, fd00:ec2::254" },
  { label: "Token refresh of the host logins", hint: "the Mac renews them" },
];

const LABEL = /^(?!-)[a-z0-9-]{1,63}(?<!-)$/;

/** Format only; the backend also rejects public suffixes. */
function domainError(input: string) {
  const domain = input.trim().toLowerCase();
  const host = domain.startsWith("*.") ? domain.slice(2) : domain;
  const labels = host.split(".");
  if (host.length > 253) return "Longer than 253 characters.";
  if (labels.length < 2) return "Needs at least two labels, like example.com.";
  if (!labels.every((l) => LABEL.test(l)))
    return "Use only letters, digits, hyphens and dots, with no scheme, path or port.";
}

interface Props {
  session: SessionView;
  /** The effective cage: the rules that the tab edits. `null`: the session has no cage. */
  cage: CageView | null;
  /** The cage of the session's VM: the test and the activity apply to it. */
  vmCage: VmCage | null;
  /** Shows in place of the rules when `cage` is null. */
  noCage: ReactNode;
  /** The network events of the session's VM. */
  events: NetEvent[];
  run: Run;
  intent?: Intent;
  onIntentDone: () => void;
  /** The tab is the one on screen. Hidden tabs stay mounted and still intersect. */
  active: boolean;
  /** Gets the last `seq` while the Activity section is in view. */
  onSeen: (seq: number) => void;
  /** Starts the session's VM. */
  onStart: () => void;
}

export function NetworkTab({
  session,
  cage,
  vmCage,
  noCage,
  events,
  run,
  intent,
  onIntentDone,
  active,
  onSeen,
  onStart,
}: Props) {
  const catalog = useCatalog();
  const [testHost, setTestHost] = useState("example.com");
  const [test, setTest] = useState<NetTest | "running">();
  const activity = useRef<HTMLDivElement>(null);
  const [activityVisible, setActivityVisible] = useState(false);
  const latestSeq = events[events.length - 1]?.seq ?? 0;
  const running = vmCage?.status.state === "running";
  const hostPort = cage?.network.hostPort;

  useEffect(() => {
    if (!cage && intent) onIntentDone();
  }, [intent, cage]);

  useEffect(() => {
    const observer = new IntersectionObserver(([entry]) =>
      setActivityVisible(entry.isIntersecting),
    );
    observer.observe(activity.current!);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (active && activityVisible) onSeen(latestSeq);
  }, [active, activityVisible, latestSeq]);

  function runTest(event: FormEvent) {
    event.preventDefault();
    if (!running) return;
    setTest("running");
    testNetwork(vmCage.id, testHost.trim()).then(setTest, (e) =>
      setTest({ proxy: { ok: false, detail: errorMessage(e) }, target: null }),
    );
  }

  const stage = (label: string, { ok, detail }: NetStage) => (
    <li className={`row ${ok ? "ok" : "danger"}`}>
      {ok ? <Check size={14} /> : <X size={14} />}
      <span>{label}</span>
      <span className="grow muted">{detail}</span>
    </li>
  );

  const testForm = (
    <>
          <form className="row" onSubmit={runTest}>
            <FloatingField
              label="Host to test"
              className="grow mono"
              value={testHost}
              onChange={(e) => setTestHost(e.target.value)}
            />
            <Button
              type="submit"
              variant="outline"
              disabled={
                !running || test === "running" || testHost.trim() === ""
              }
              title={running ? undefined : "Start the cage to test"}
            >
              {test === "running" ? "Testing…" : "Test"}
            </Button>
          </form>
          {test && test !== "running" && (
            <ul className="list">
              {stage("Proxy", test.proxy)}
              {test.target && stage("Host", test.target)}
            </ul>
          )}
    </>
  );

  return (
    <div className="tab-body">
      <Card variant="subtle" padding="md">
        <CardHeader
          title="How the rules work"
          titleAs="h2"
          description="The agent can reach a host only if the rules allow it. The proxy checks in this order:"
        />
        <CardBody>
          <ol className="stack">
            <li>
              Always blocked: your Mac, your network, cloud metadata.
              {hostPort !== undefined &&
                ` The exception is 127.0.0.1:${hostPort}, the service on this Mac. Offline or a Block rule for ${GUEST_HOST} still blocks it.`}
            </li>
            <li>Blocked services and hosts. Block always beats Allow.</li>
            <li>
              Allowed services and hosts. Their credentials are added here.
            </li>
            <li>Everything else: the default.</li>
          </ol>
          <p className="muted">
            Changes apply to the next request, with no restart.
          </p>
        </CardBody>
      </Card>

      {cage ? (
        <CageRules
          session={session}
          cage={cage}
          catalog={catalog}
          run={run}
          intent={intent}
          onIntentDone={onIntentDone}
          test={testForm}
        />
      ) : (
        <>
          {noCage}
          <Card variant="outlined" padding="md">
            <CardHeader title="Test" titleAs="h2" />
            <CardBody className="stack">{testForm}</CardBody>
          </Card>
        </>
      )}

      <div ref={activity}>
        <NetworkActivity
          cage={cage}
          running={running}
          events={events}
          catalog={catalog}
          onStart={onStart}
          run={run}
        />
      </div>

      <Card variant="subtle" padding="md">
        <CardHeader
          title="Always blocked"
          titleAs="h2"
          description={
            hostPort === undefined
              ? "No rule and no default opens these. You cannot change this."
              : `No rule and no default opens these. The only exception is 127.0.0.1:${hostPort}, the service on this Mac.`
          }
        />
        <CardBody>
          <ul className="list">
            {ALWAYS_BLOCKED.map((group) => (
              <li key={group.label} className="row">
                <Ban size={14} className="danger" />
                <span className="grow">{group.label}</span>
                <span className="muted mono">
                  {group.label === "This computer" && hostPort !== undefined
                    ? `${group.hint}, except 127.0.0.1:${hostPort}`
                    : group.hint}
                </span>
              </li>
            ))}
          </ul>
        </CardBody>
      </Card>
    </div>
  );
}

interface RulesProps {
  session: SessionView;
  cage: CageView;
  catalog: Service[];
  run: Run;
  intent?: Intent;
  onIntentDone: () => void;
  /** The network test, at the end of the Internet access card. */
  test: ReactNode;
}

/** The rules of the effective cage. */
function CageRules({ session, cage, catalog, run, intent, onIntentDone, test }: RulesProps) {
  const { id, network } = cage;
  const [adding, setAdding] = useState<"service" | "host">();
  const [host, setHost] = useState("");
  const [problem, setProblem] = useState<string>();
  const [expanded, setExpanded] = useState<string>();
  const [credentialsOf, setCredentialsOf] = useState<string>();
  const hostField = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (intent?.kind !== "addRule") return;
    onIntentDone();
    setAdding("host");
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
  const blockedNeeds = session.agentNeeds.filter((need) => !need.allowed);
  const unused = catalog.filter(
    (s) =>
      !network.rules.some(
        (r) => r.target.kind === "service" && r.target.id === s.id,
      ),
  );

  return (
    <>
      <Card variant="outlined" padding="md">
        <CardHeader title="Internet access" titleAs="h2" />
        <CardBody className="stack">
          <SegmentedControl
            label="Everything else"
            options={[
              { value: "block", label: "Block everything else" },
              { value: "allow", label: "Allow everything else" },
            ]}
            value={network.default}
            onChange={(value) =>
              run(() => setNetworkDefault(id, value as DefaultAction))
            }
          />
          {network.default === "block" ? (
            <p className="muted">A host that no rule allows is blocked.</p>
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
                      onClick={() =>
                        run(() =>
                          putRule(id, {
                            id: rule.id,
                            target: rule.target,
                            action: other,
                          }),
                        )
                      }
                    >
                      {other === "allow" ? "Allow" : "Block"}
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
