import { Button, Card } from "@driif/motiv";
import { Hourglass } from "lucide-react";
import { useEffect, useState } from "react";
import type { Run } from "../../App";
import { answerAsk, Ask, AskAnswer } from "./api";

const ANSWERS: { answer: AskAnswer; label: string }[] = [
  { answer: "once", label: "Allow once" },
  { answer: "untilStop", label: "Allow until stop" },
  { answer: "always", label: "Always allow" },
  { answer: "block", label: "Block" },
];

function Countdown({ until }: { until: number }) {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  return <span className="muted mono">{Math.max(0, Math.ceil((until - now) / 1000))} s</span>;
}

/** One pending ask with its countdown and its answers. */
export function AskRow({ vmKey, ask, run }: { vmKey: string; ask: Ask; run: Run }) {
  return (
    <li className="row">
      <Hourglass size={14} className="warn" />
      <span className="grow mono">
        {ask.method} {ask.host}
        {ask.port === 443 || ask.port === 80 ? "" : `:${ask.port}`}
      </span>
      <Countdown until={ask.expiresAt} />
      {ANSWERS.map(({ answer, label }) => (
        <Button
          key={answer}
          size="sm"
          variant={answer === "block" ? "ghost" : "outline"}
          onClick={() => run(() => answerAsk(vmKey, ask.id, answer))}
        >
          {label}
        </Button>
      ))}
    </li>
  );
}

export interface AskGroup {
  vmKey: string;
  /** The session that opens on Open Network. */
  sessionId: string;
  /** The names of the sessions of the VM. */
  names: string;
  asks: Ask[];
}

interface Props {
  groups: AskGroup[];
  run: Run;
  onOpen: (sessionId: string) => void;
}

/** The pending asks of the VMs whose Network tab is not open. */
export function PendingAsks({ groups, run, onOpen }: Props) {
  return groups.map((group) => (
    <Card
      key={group.vmKey}
      variant="outlined"
      padding="sm"
      className="stack"
      role="alert"
      aria-label={`Requests of ${group.names} that wait for you`}
    >
      <div className="row">
        <span className="grow">
          {group.asks.length === 1 ? "A request" : `${group.asks.length} requests`} of {group.names}{" "}
          {group.asks.length === 1 ? "waits" : "wait"} for you. The proxy blocks a request with no answer in time.
        </span>
        <Button size="sm" variant="ghost" onClick={() => onOpen(group.sessionId)}>
          Open Network
        </Button>
      </div>
      <ul className="list">
        {group.asks.map((ask) => (
          <AskRow key={ask.id} vmKey={group.vmKey} ask={ask} run={run} />
        ))}
      </ul>
    </Card>
  ));
}
