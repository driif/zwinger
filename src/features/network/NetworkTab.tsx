import { Button, Card, CardBody, CardHeader, FloatingField } from "@driif/motiv";
import { Ban, Check, X } from "lucide-react";
import { FormEvent, ReactNode, useEffect, useRef, useState } from "react";
import { GUEST_HOST, NetEvent, NetStage, NetTest, testNetwork } from "./api";
import { CageView, VmCage, SessionView } from "../../shared/api";
import type { Intent, Run } from "../../App";
import { LivePolicy } from "./LivePolicy";
import { NetworkPolicy } from "./NetworkPolicy";
import { NetworkActivity } from "./NetworkActivity";
import { errorMessage } from "../../shared/errorMessage";
import { useCatalog } from "./network";

/** The groups that the proxy blocks after it resolves the host on this Mac. */
const ALWAYS_BLOCKED = [
  { label: "This computer", hint: "127.0.0.0/8, ::1" },
  { label: "Local network", hint: "10/8, 172.16/12, 192.168/16, fc00::/7" },
  { label: "Shared and link-local", hint: "100.64/10, 169.254/16, fe80::/10" },
  { label: "Cloud metadata", hint: "169.254.169.254, fd00:ec2::254" },
  { label: "Token refresh of the host logins", hint: "the Mac renews them" },
];

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
              Allowed services and hosts, and the hosts allowed until the VM
              stops. An Allow rule with methods blocks the other methods. The
              credentials are added here.
            </li>
            <li>Everything else: the default. Ask holds the request until you answer it.</li>
          </ol>
          <p className="muted">
            Changes apply to the next request, with no restart.
          </p>
        </CardBody>
      </Card>

      {cage ? (
        <NetworkPolicy
          key={cage.id}
          cage={cage}
          needs={session.agentNeeds}
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

      {running && session.vmKey && (
        <LivePolicy key={session.vmKey} vmKey={session.vmKey} run={run} />
      )}

      <div ref={activity}>
        <NetworkActivity
          cage={cage}
          vmKey={session.vmKey}
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
