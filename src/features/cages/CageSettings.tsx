import {
  Badge,
  Button,
  Card,
  CardBody,
  CardHeader,
  FloatingField,
  LiquidSwitch,
  SegmentedControl,
  TextField,
} from "@driif/motiv";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderPlus, X } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import { addMount, setAgentsUnprompted, setMountReadonly } from "../settings/api";
import { AGENT_NAMES, CageView, StartRecord, VmCage } from "../../shared/api";
import { setCageResources } from "./api";
import type { Intent, Run, Runtimes } from "../../App";
import { InstallControl } from "../tasks/roomParts";
import { errorMessage } from "../../shared/errorMessage";
import { removeMountWithUndo } from "../../shared/undo";
import { phaseName } from "../projects/StartProgress";
import { direction, duration, trendSummary } from "./startTrend";

export const WORKSPACE = "/workspace";
/** The same bounds as `cage::CPUS` and `cage::MEMORY_MIB` in the backend. */
const CPUS = { min: 1, max: 8 };
const MEMORY_MIB = { min: 512, max: 8192 };

/** The same rules as `cage::guest_path` in the backend. */
function guestError(path: string) {
  const parts = path.split("/").filter(Boolean);
  if (!path.startsWith("/") || parts.length === 0 || parts.some((p) => p === "." || p === ".."))
    return "Must be an absolute path, like /mnt/data.";
  const normalized = `/${parts.join("/")}`;
  if (normalized === WORKSPACE || normalized.startsWith(`${WORKSPACE}/`)) return `Must not be inside ${WORKSPACE}.`;
}

interface MountDraft {
  host: string;
  /** The dropped path, if it is a link to `host`. */
  link?: string;
  guest: string;
  readonly: boolean;
  error?: string;
}

interface ResourcesDraft {
  cpus?: string;
  memoryMib?: string;
  error?: string;
}

interface Props {
  cage: CageView;
  /** The cage whose VM the runtime state comes from. */
  vm: VmCage | null;
  runtimes: Runtimes;
  run: Run;
  /** Only a `mount` intent applies here. The caller marks each intent done. */
  intent?: Intent;
}

/** The settings of a cage: its mounts and its agent runtimes. All sessions of the cage share them. */
export function CageSettings({ cage, vm, runtimes, run, intent }: Props) {
  const [draft, setDraft] = useState<MountDraft>();
  const [resources, setResources] = useState<ResourcesDraft>({});
  const saved = cage.resources;
  // A field with no draft shows the saved value, so a save elsewhere shows at once.
  const cpus = resources.cpus ?? String(saved.cpus);
  const memoryMib = resources.memoryMib ?? String(saved.memoryMib);
  const resourcesChanged = cpus !== String(saved.cpus) || memoryMib !== String(saved.memoryMib);
  const applied = vm?.status.state === "running" ? vm.appliedResources : null;

  function saveResources(event: FormEvent) {
    event.preventDefault();
    run(() =>
      setCageResources(cage.id, Number(cpus), Number(memoryMib)).then(
        () => setResources({}),
        (e) => setResources({ ...resources, error: errorMessage(e) }),
      ),
    );
  }

  function draftMount(host: string, dropped?: string) {
    const base = host.split("/").filter(Boolean).pop() ?? "data";
    setDraft({ host, link: dropped !== host ? dropped : undefined, guest: `/mnt/${base}`, readonly: true });
  }

  async function pickMount() {
    const host = await open({ directory: true, title: "Mount a folder into the cage" });
    if (host !== null) draftMount(host);
  }

  useEffect(() => {
    if (intent?.kind !== "mount") return;
    // A folder dropped on the map comes with its canonical path.
    if (intent.host) draftMount(intent.host, intent.dropped);
    else run(pickMount);
  }, [intent]);

  function confirmMount(event: FormEvent) {
    event.preventDefault();
    if (!draft) return;
    const error = guestError(draft.guest);
    if (error) return setDraft({ ...draft, error });
    run(() =>
      addMount(cage.id, draft.host, draft.guest, draft.readonly).then(
        () => setDraft(undefined),
        (e) => setDraft({ ...draft, error: errorMessage(e) }),
      ),
    );
  }

  return (
    <>
      <Card variant="outlined" padding="md">
        <CardHeader
          title="Mounts"
          titleAs="h2"
          description="More host folders that the agent can see."
          actions={
            <Button
              size="sm"
              variant="outline"
              iconLeft={<FolderPlus size={14} />}
              disabled={draft !== undefined}
              onClick={() => run(pickMount)}
            >
              Add mount
            </Button>
          }
        />
        <CardBody className="stack">
          {draft && (
            <form className="stack" onSubmit={confirmMount}>
              <p className="mono" title={draft.host}>
                {draft.host}
              </p>
              {draft.link && (
                <p className="muted">
                  <span className="mono">{draft.link}</span> is a link to <span className="mono">{draft.host}</span>.
                </p>
              )}
              <FloatingField
                label="Guest path"
                className="mono"
                value={draft.guest}
                hint={draft.error}
                aria-invalid={draft.error !== undefined}
                showValidity={false}
                onChange={(e) => setDraft({ ...draft, guest: e.target.value, error: undefined })}
              />
              <SegmentedControl
                label="Access"
                options={[
                  { value: "ro", label: "Read-only" },
                  { value: "rw", label: "Read-write" },
                ]}
                value={draft.readonly ? "ro" : "rw"}
                onChange={(v) => setDraft({ ...draft, readonly: v === "ro", error: undefined })}
              />
              <div className="row end">
                <Button size="sm" variant="ghost" onClick={() => setDraft(undefined)}>
                  Cancel
                </Button>
                <Button size="sm" type="submit">
                  Mount
                </Button>
              </div>
            </form>
          )}
          {cage.mounts.length === 0 && !draft && <p className="muted">No mounts. Only {WORKSPACE} is visible.</p>}
          <ul className="list">
            {cage.mounts.map((m) => (
              <li key={m.guest} className="row">
                <span className="mono">{m.guest}</span>
                <span className="grow muted mono" title={m.host}>
                  {m.host}
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  aria-label={`${m.guest} is ${m.readonly ? "read-only" : "read-write"}. Toggle.`}
                  onClick={() => run(() => setMountReadonly(cage.id, m.guest, !m.readonly))}
                >
                  {m.readonly ? "ro" : "rw"}
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label={`Remove ${m.guest}`}
                  iconLeft={<X size={14} />}
                  onClick={() => removeMountWithUndo(run, cage.id, m.guest)}
                />
              </li>
            ))}
          </ul>
        </CardBody>
      </Card>

      <Card variant="outlined" padding="md">
        <CardHeader
          title="Resources"
          titleAs="h2"
          description="The CPUs and memory of each VM of this cage. The next start applies a change."
        />
        <CardBody>
          <form className="stack" onSubmit={saveResources}>
            <div className="row">
              <TextField
                type="number"
                label="CPUs"
                min={CPUS.min}
                max={CPUS.max}
                step={1}
                value={cpus}
                hint={`${CPUS.min} to ${CPUS.max}`}
                onChange={(e) => setResources({ ...resources, cpus: e.target.value, error: undefined })}
              />
              <TextField
                type="number"
                label="Memory (MiB)"
                min={MEMORY_MIB.min}
                max={MEMORY_MIB.max}
                step={1}
                value={memoryMib}
                hint={`${MEMORY_MIB.min} to ${MEMORY_MIB.max}`}
                onChange={(e) => setResources({ ...resources, memoryMib: e.target.value, error: undefined })}
              />
            </div>
            {resources.error && <p role="alert">{resources.error}</p>}
            <p className="muted">
              A running VM keeps its values until it restarts.
              {applied && ` It runs with ${applied.cpus} CPUs and ${applied.memoryMib} MiB.`}
            </p>
            <div className="row end">
              <Button size="sm" type="submit" disabled={!resourcesChanged}>
                Save
              </Button>
            </div>
          </form>
        </CardBody>
      </Card>

      <Card variant="outlined" padding="md">
        <CardHeader
          title="Agent runtimes"
          titleAs="h2"
          description="For all cages. A cage mounts the installed runtimes read-only when it starts."
        />
        <CardBody className="stack">
          <LiquidSwitch
            label="Agents run without permission prompts"
            checked={cage.agentsUnprompted}
            onChange={(on) => run(() => setAgentsUnprompted(cage.id, on))}
          />
          <p className="muted">
            For this cage: the cage is the sandbox. Run the agents from the Agents group or as claude, codex, pi and
            gemini in a cage terminal. With no login from this Mac, log in with the agent's own /login: allow its
            provider hosts first.
          </p>
          <ul className="list">
            <li className="row">
              <strong>pi</strong>
              <span className="grow muted">Bundled with zwinger</span>
            </li>
            {runtimes.list.map((runtime) => (
              <li key={runtime.name} className="row">
                <strong>{AGENT_NAMES[runtime.name]}</strong>
                <span className="grow muted">
                  {runtime.installed ? `${runtime.version} installed` : `${runtime.version}, not installed`}
                  {runtime.installed && vm?.status.state === "running" && !vm.runtimes.includes(runtime.name)
                    ? " · restart the cage to use it"
                    : ""}
                </span>
                {!runtime.installed && <InstallControl runtime={runtime} runtimes={runtimes} />}
              </li>
            ))}
          </ul>
        </CardBody>
      </Card>
    </>
  );
}

/** The last start and the start trend of a VM. They do not depend on the cage that the project uses next. */
export function StartDiagnostics({ vm, startHistory }: { vm: VmCage | null; startHistory: StartRecord[] }) {
  return (
    <>
      {vm?.startTimings && (
        <Card variant="outlined" padding="md">
          <CardHeader
            title="Last start"
            titleAs="h2"
            description={`The start took ${vm.startTimings.totalMs} ms. Some phases ran at the same time.`}
          />
          <CardBody>
            <ul className="list">
              {vm.startTimings.phases.map(({ phase, ms }, i) => (
                <li key={i} className="row">
                  <span className="grow">{phaseName(phase)}</span>
                  <span className="muted mono">{ms} ms</span>
                </li>
              ))}
            </ul>
          </CardBody>
        </Card>
      )}

      {startHistory.length > 0 && (
        <Card variant="outlined" padding="md">
          <CardHeader title="Start trend" titleAs="h2" description={trendSummary(startHistory)} />
          <CardBody>
            <ol className="list">
              {startHistory.map(({ at, totalMs }, i) => (
                <li key={i} className="row">
                  <span className="grow">
                    Start {i + 1} of {startHistory.length}
                    <span className="muted"> · {new Date(at).toLocaleString()}</span>
                  </span>
                  <span className="muted mono">{duration(totalMs)}</span>
                  {i > 0 && <Badge>{direction(startHistory[i - 1].totalMs, totalMs)}</Badge>}
                </li>
              ))}
            </ol>
          </CardBody>
        </Card>
      )}
    </>
  );
}
