import { Button, Card, CardBody, CardHeader, FloatingField } from "@driif/motiv";
import { X } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import type { Run } from "../../App";
import type { Rule } from "../../shared/api";
import { errorMessage } from "../../shared/errorMessage";
import {
  addTemporaryRule,
  Ask,
  listAsks,
  onNetAsks,
  removeTemporaryRule,
  temporaryRules,
} from "./api";
import { domainError } from "./network";
import { AskRow } from "./PendingAsks";

interface Props {
  vmKey: string;
  run: Run;
}

/** The pending asks and the Allow rules until stop of a running VM. */
export function LivePolicy({ vmKey, run }: Props) {
  const [asks, setAsks] = useState<Ask[]>([]);
  const [rules, setRules] = useState<Rule[]>([]);
  const [host, setHost] = useState("");
  const [problem, setProblem] = useState<string>();

  useEffect(() => {
    const load = () => {
      listAsks(vmKey).then(setAsks);
      temporaryRules(vmKey).then(setRules);
    };
    load();
    const stop = onNetAsks((key) => key === vmKey && load());
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, [vmKey]);

  function add(event: FormEvent) {
    event.preventDefault();
    const error = domainError(host);
    if (error) return setProblem(error);
    run(() =>
      addTemporaryRule(vmKey, host).then(
        () => setHost(""),
        (e) => setProblem(errorMessage(e)),
      ),
    );
  }

  return (
    <>
      {asks.length > 0 && (
        <Card variant="outlined" padding="md">
          <CardHeader
            title="Requests that wait for you"
            titleAs="h2"
            description="No rule decides these. The proxy blocks each one that has no answer after 30 seconds."
          />
          <CardBody>
            <ul className="list">
              {asks.map((ask) => (
                <AskRow key={ask.id} vmKey={vmKey} ask={ask} run={run} />
              ))}
            </ul>
          </CardBody>
        </Card>
      )}
      <Card variant="outlined" padding="md">
        <CardHeader
          title="Allowed until the VM stops"
          titleAs="h2"
          description="These rules apply to this VM only. They are never saved. Block rules, Offline and the always-blocked groups still win."
        />
        <CardBody className="stack">
          {rules.length === 0 && <p className="muted">No rules until stop.</p>}
          <ul className="list">
            {rules.map((rule) => {
              const label = rule.target.kind === "host" ? rule.target.host : rule.target.id;
              return (
                <li key={rule.id} className="row">
                  <span className="grow mono">{label}</span>
                  <span className="ok">Allowed</span>
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={`Remove ${label}`}
                    iconLeft={<X size={14} />}
                    onClick={() => run(() => removeTemporaryRule(vmKey, rule.id))}
                  />
                </li>
              );
            })}
          </ul>
          <form className="row" onSubmit={add}>
            <FloatingField
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
            <Button type="submit" variant="outline" disabled={host.trim() === ""}>
              Allow until stop
            </Button>
          </form>
        </CardBody>
      </Card>
    </>
  );
}
