import {
  Button,
  Card,
  CardBody,
  CardHeader,
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
  Plus,
  TriangleAlert,
  X,
} from "lucide-react";
import { FormEvent, useEffect, useRef, useState } from "react";
import {
  NetEvent,
  NetStage,
  NetTest,
  putRule,
  removeRule,
  setNetworkDefault,
  setOffline,
  testNetwork,
} from "./api";
import {
  Action,
  AGENT_NAMES,
  DefaultAction,
  Rule,
  ProjectView,
  Target,
} from "../../shared/api";
import type { Intent, Run } from "../../App";
import { NetworkActivity } from "./NetworkActivity";
import { RuleCredentials } from "./RuleCredentials";
import { errorMessage } from "../../shared/errorMessage";
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
  project: ProjectView;
  events: NetEvent[];
  run: Run;
  intent?: Intent;
  onIntentDone: () => void;
  /** The tab is the one on screen. Hidden tabs stay mounted and still intersect. */
  active: boolean;
  /** Gets the last `seq` while the Activity section is in view. */
  onSeen: (seq: number) => void;
}

export function NetworkTab({
  project,
  events,
  run,
  intent,
  onIntentDone,
  active,
  onSeen,
}: Props) {
  const { id, network } = project;
  const catalog = useCatalog();
  const [adding, setAdding] = useState<"service" | "host">();
  const [host, setHost] = useState("");
  const [problem, setProblem] = useState<string>();
  const [expanded, setExpanded] = useState<string>();
  const [credentialsOf, setCredentialsOf] = useState<string>();
  const [testHost, setTestHost] = useState("example.com");
  const [test, setTest] = useState<NetTest | "running">();
  const hostField = useRef<HTMLInputElement>(null);
  const activity = useRef<HTMLDivElement>(null);
  const [activityVisible, setActivityVisible] = useState(false);
  const latestSeq = events[events.length - 1]?.seq ?? 0;
  const running = project.status.state === "running";

  useEffect(() => {
    if (intent?.kind !== "addRule") return;
    onIntentDone();
    setAdding("host");
  }, [intent]);

  useEffect(() => {
    if (adding === "host") hostField.current?.focus();
  }, [adding]);

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

  function runTest(event: FormEvent) {
    event.preventDefault();
    setTest("running");
    testNetwork(id, testHost.trim()).then(setTest, (e) =>
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

  const loginsOf = (rule: Rule) =>
    project.logins.filter((l) =>
      l.hosts.some((h) => ruleDomains(rule, catalog).some((d) => covers(d, h))),
    );
  const blockedNeeds = project.agentNeeds.filter((need) => !need.allowed);
  const unused = catalog.filter(
    (s) =>
      !network.rules.some(
        (r) => r.target.kind === "service" && r.target.id === s.id,
      ),
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
            <li>Always blocked: your Mac, your network, cloud metadata.</li>
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
        </CardBody>
      </Card>

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
                      onClick={() => run(() => removeRule(id, rule.id))}
                    />
                  </div>
                  {expanded === rule.id && (
                    <p className="muted mono">{domains.join(", ")}</p>
                  )}
                  {credentialsOf === rule.id && takesCredentials && (
                    <RuleCredentials
                      project={project}
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

      <div ref={activity}>
        <NetworkActivity
          project={project}
          events={events}
          catalog={catalog}
          run={run}
        />
      </div>

      <Card variant="subtle" padding="md">
        <CardHeader
          title="Always blocked"
          titleAs="h2"
          description="No rule and no default opens these. You cannot change this."
        />
        <CardBody>
          <ul className="list">
            {ALWAYS_BLOCKED.map((group) => (
              <li key={group.label} className="row">
                <Ban size={14} className="danger" />
                <span className="grow">{group.label}</span>
                <span className="muted mono">{group.hint}</span>
              </li>
            ))}
          </ul>
        </CardBody>
      </Card>
    </div>
  );
}
