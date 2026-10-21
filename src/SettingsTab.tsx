import {
  Button,
  Card,
  CardBody,
  CardHeader,
  FloatingField,
  LiquidSwitch,
  RadioGroup,
  SegmentedControl,
} from "@driif/motiv";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderPlus, Trash2, X } from "lucide-react";
import { FormEvent, useEffect, useRef, useState } from "react";
import {
  AGENT_NAMES,
  addMount,
  deleteShield,
  removeMount,
  renameShield,
  setAgentsUnprompted,
  setHarness,
  setMountReadonly,
  ShieldView,
} from "./api";
import type { Intent, Run, Runtimes } from "./App";
import { InstallControl } from "./roomParts";
import { AppsSection } from "./AppsSection";

const WORKSPACE = "/workspace";

/** The same rules as `shield::guest_path` in the backend. */
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

interface Props {
  shield: ShieldView;
  folders: string[];
  runtimes: Runtimes;
  run: Run;
  intent?: Intent;
  onIntentDone: () => void;
}

export function SettingsTab({ shield, folders, runtimes, run, intent, onIntentDone }: Props) {
  const { id, status } = shield;
  const [name, setName] = useState(shield.name);
  const [nameError, setNameError] = useState<string>();
  const [draft, setDraft] = useState<MountDraft>();
  const [confirmDelete, setConfirmDelete] = useState(false);
  const nameField = useRef<HTMLInputElement>(null);
  const harnessGroup = useRef<HTMLFieldSetElement>(null);

  const harnessOptions = [
    ...folders.map((f) => `${WORKSPACE}/${f}`),
    ...shield.mounts.map((m) => m.guest),
  ];
  if (shield.harness && !harnessOptions.includes(shield.harness)) harnessOptions.push(shield.harness);

  function rename(event: FormEvent) {
    event.preventDefault();
    run(() => renameShield(id, name).catch((e) => setNameError(String(e))));
  }

  function draftMount(host: string, dropped?: string) {
    const base = host.split("/").filter(Boolean).pop() ?? "data";
    setDraft({ host, link: dropped !== host ? dropped : undefined, guest: `/mnt/${base}`, readonly: true });
  }

  async function pickMount() {
    const host = await open({ directory: true, title: "Mount a folder into the shield" });
    if (host !== null) draftMount(host);
  }

  useEffect(() => {
    if (!intent || intent.kind === "app") return;
    onIntentDone();
    switch (intent.kind) {
      case "rename":
        nameField.current?.focus();
        nameField.current?.select();
        break;
      case "mount":
        // A folder dropped on the map comes with its canonical path.
        if (intent.host) draftMount(intent.host, intent.dropped);
        else run(pickMount);
        break;
      case "harness":
        harnessGroup.current?.querySelector<HTMLInputElement>("input:checked")?.focus();
        break;
      case "delete":
        setConfirmDelete(true);
    }
  }, [intent]);

  function confirmMount(event: FormEvent) {
    event.preventDefault();
    if (!draft) return;
    const error = guestError(draft.guest);
    if (error) return setDraft({ ...draft, error });
    run(() =>
      addMount(id, draft.host, draft.guest, draft.readonly).then(
        () => setDraft(undefined),
        (e) => setDraft({ ...draft, error: String(e) }),
      ),
    );
  }

  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader title="Shield" titleAs="h2" />
        <CardBody className="stack">
          <form className="row" onSubmit={rename}>
            <FloatingField
              ref={nameField}
              label="Name"
              className="grow"
              value={name}
              maxLength={64}
              hint={nameError}
              aria-invalid={nameError !== undefined}
              showValidity={false}
              onChange={(e) => {
                setName(e.target.value);
                setNameError(undefined);
              }}
            />
            <Button type="submit" variant="outline" disabled={name.trim() === shield.name}>
              Rename
            </Button>
          </form>
          <p className="muted">
            Folder <span className="mono">{shield.folder}</span> is <span className="mono">{WORKSPACE}</span> in the
            shield. It is read-write.
          </p>
        </CardBody>
      </Card>

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
          {shield.mounts.length === 0 && !draft && <p className="muted">No mounts. Only {WORKSPACE} is visible.</p>}
          <ul className="list">
            {shield.mounts.map((m) => (
              <li key={m.guest} className="row">
                <span className="mono">{m.guest}</span>
                <span className="grow muted mono" title={m.host}>
                  {m.host}
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  aria-label={`${m.guest} is ${m.readonly ? "read-only" : "read-write"}. Toggle.`}
                  onClick={() => run(() => setMountReadonly(id, m.guest, !m.readonly))}
                >
                  {m.readonly ? "ro" : "rw"}
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label={`Remove ${m.guest}`}
                  iconLeft={<X size={14} />}
                  onClick={() => run(() => removeMount(id, m.guest))}
                />
              </li>
            ))}
          </ul>
        </CardBody>
      </Card>

      <Card variant="outlined" padding="md">
        <CardHeader
          title="Harness"
          titleAs="h2"
          description="The folder where the pi agent starts. Put its skills and instructions there."
        />
        <CardBody>
          <RadioGroup
            ref={harnessGroup}
            name={`harness-${id}`}
            legend="Agent starts in"
            options={[
              { value: "", label: <span className="mono">{WORKSPACE}</span>, hint: "No harness" },
              ...harnessOptions.map((path) => ({ value: path, label: <span className="mono">{path}</span> })),
            ]}
            value={shield.harness ?? ""}
            onChange={(value) => run(() => setHarness(id, value || null))}
          />
        </CardBody>
      </Card>

      <Card variant="outlined" padding="md">
        <CardHeader
          title="Agent runtimes"
          titleAs="h2"
          description="For all shields. A shield mounts the installed runtimes read-only when it starts."
        />
        <CardBody className="stack">
          <LiquidSwitch
            label="Agents run without permission prompts"
            checked={shield.agentsUnprompted}
            onChange={(on) => run(() => setAgentsUnprompted(id, on))}
          />
          <p className="muted">
            For this shield: the shield is the sandbox. Run the agents from the Agents group or as claude, codex, pi and
            gemini in a shield terminal. With no login from this Mac, log in with the agent's own /login: allow its
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
                  {runtime.installed && status.state === "running" && !shield.runtimes.includes(runtime.name)
                    ? " · restart the shield to use it"
                    : ""}
                </span>
                {!runtime.installed && <InstallControl runtime={runtime} runtimes={runtimes} />}
              </li>
            ))}
          </ul>
        </CardBody>
      </Card>

      <AppsSection
        shield={shield}
        run={run}
        intent={intent?.kind === "app" ? intent : undefined}
        onIntentDone={onIntentDone}
      />

      <Card variant="outlined" padding="md">
        <CardHeader title="Delete shield" titleAs="h2" description="Stops the shield. Your folders stay on disk." />
        <CardBody className="row">
          {confirmDelete ? (
            <>
              <span className="grow danger">Delete {shield.name}?</span>
              <Button variant="ghost" onClick={() => setConfirmDelete(false)}>
                Cancel
              </Button>
              <Button variant="danger" iconLeft={<Trash2 size={14} />} onClick={() => run(() => deleteShield(id))}>
                Delete
              </Button>
            </>
          ) : (
            <Button
              variant="danger"
              iconLeft={<Trash2 size={14} />}
              disabled={status.state === "starting" || status.state === "stopping"}
              onClick={() => setConfirmDelete(true)}
            >
              Delete shield…
            </Button>
          )}
        </CardBody>
      </Card>
    </div>
  );
}
