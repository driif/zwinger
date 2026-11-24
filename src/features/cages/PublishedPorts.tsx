import { Button, Card, CardBody, CardHeader, LiquidSwitch, TextField } from "@driif/motiv";
import { openUrl } from "@tauri-apps/plugin-opener";
import { ExternalLink, Plus, TriangleAlert, X } from "lucide-react";
import { FormEvent, useState } from "react";
import type { CageView, PublishedPort, VmCage } from "../../shared/api";
import type { Run } from "../../App";
import { Notice } from "../../shared/Notice";
import { errorMessage } from "../../shared/errorMessage";
import { setPublishedPorts } from "./api";

/** A port from 1 to 65535, else `undefined`. */
function parsePort(text: string) {
  const port = Number(text.trim());
  return /^\d+$/.test(text.trim()) && port >= 1 && port <= 65535 ? port : undefined;
}

interface Props {
  cage: CageView;
  vm: VmCage | null;
  run: Run;
}

/** The guest ports that the VMs of the cage publish on 127.0.0.1 of this Mac (ADR 0010). */
export function PublishedPorts({ cage, vm, run }: Props) {
  const [guest, setGuest] = useState("");
  const [host, setHost] = useState("");
  const [problem, setProblem] = useState<string>();
  const ports = cage.publishedPorts;
  const applied = vm?.status.state === "running" ? vm.appliedPorts : [];

  function save(next: PublishedPort[], done?: () => void) {
    run(() => setPublishedPorts(cage.id, next).then(done, (e) => setProblem(errorMessage(e))));
  }

  function add(event: FormEvent) {
    event.preventDefault();
    const guestPort = parsePort(guest);
    const hostPort = host.trim() === "" ? guestPort : parsePort(host);
    if (guestPort === undefined || hostPort === undefined) return setProblem("Use ports from 1 to 65535.");
    save([...ports, { guest: guestPort, host: hostPort, on: false }], () => {
      setGuest("");
      setHost("");
    });
  }

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Published ports"
        titleAs="h2"
        description="Open a server of the VM on this Mac, such as a dev server. The next VM start applies a change."
      />
      <CardBody className="stack">
        <Notice icon={<TriangleAlert size={14} />}>
          A published port listens on 127.0.0.1 only: other computers cannot connect. But each app and each user on this
          Mac can connect, and so can web pages in your browser. A new port is off: turn it on only for a server that you
          trust. Turn the port off when you do not use it; the port closes when the VM restarts.
        </Notice>
        {ports.length === 0 && <p className="muted">No published ports. Nothing on this Mac can connect to the VM.</p>}
        <ul className="list">
          {ports.map((port) => {
            const live = applied.some((p) => p.guest === port.guest && p.host === port.host);
            const url = `http://localhost:${port.host}`;
            const state = live
              ? port.on
                ? "published"
                : "still published until the VM restarts"
              : !port.on
                ? "off"
                : vm?.status.state === "running"
                  ? "not published: restart the VM, or the port was in use"
                  : "published at the next start";
            return (
              <li key={port.guest} className="row">
                <LiquidSwitch
                  label={
                    <span className="mono">
                      {port.guest} → localhost:{port.host}
                    </span>
                  }
                  checked={port.on}
                  onChange={(on) => save(ports.map((p) => (p.guest === port.guest ? { ...p, on } : p)))}
                />
                <span className="grow muted">{state}</span>
                <Button
                  size="sm"
                  variant="outline"
                  iconLeft={<ExternalLink size={14} />}
                  disabled={!live}
                  onClick={() => run(() => openUrl(url))}
                >
                  Open
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label={`Remove guest port ${port.guest}`}
                  iconLeft={<X size={14} />}
                  onClick={() => save(ports.filter((p) => p.guest !== port.guest))}
                />
              </li>
            );
          })}
        </ul>
        <form className="row" onSubmit={add}>
          <TextField
            label="Guest port"
            className="mono"
            inputMode="numeric"
            value={guest}
            onChange={(e) => {
              setGuest(e.target.value);
              setProblem(undefined);
            }}
          />
          <TextField
            label="Port on this Mac"
            className="mono"
            inputMode="numeric"
            hint="Empty: the guest port"
            value={host}
            onChange={(e) => {
              setHost(e.target.value);
              setProblem(undefined);
            }}
          />
          <Button size="sm" type="submit" variant="outline" iconLeft={<Plus size={14} />} disabled={guest.trim() === ""}>
            Publish
          </Button>
        </form>
        {problem && <p role="alert">{problem}</p>}
      </CardBody>
    </Card>
  );
}
