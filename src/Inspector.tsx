import { Button, Card, CardBody, CardHeader, FilterChips, FloatingField, SegmentedControl } from "@driif/motiv";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderPlus, Play, Plus, Square, Trash2, TriangleAlert, X } from "lucide-react";
import { FormEvent, useState } from "react";
import {
  addMount,
  deleteShield,
  removeMount,
  setMountReadonly,
  setNetworkPolicy,
  ShieldView,
  startShield,
  stopShield,
} from "./api";

const NPM_REGISTRY = "registry.npmjs.org";
const PROVIDERS = ["api.anthropic.com", "api.openai.com", "generativelanguage.googleapis.com"];
const PRESETS = [NPM_REGISTRY, ...PROVIDERS];
const WORKSPACE = "/workspace";

const LABEL = /^(?!-)[a-z0-9-]{1,63}(?<!-)$/;

/** The same rules as `shield::domain` in the backend. */
function domainError(input: string) {
  const domain = input.trim().toLowerCase();
  const host = domain.startsWith("*.") ? domain.slice(2) : domain;
  const labels = host.split(".");
  if (host.length > 253) return "Longer than 253 characters.";
  if (labels.length < 2) return "Needs at least two labels, like example.com.";
  if (!labels.every((l) => LABEL.test(l)))
    return "Use only letters, digits, hyphens and dots, with no scheme, path or port.";
}

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
  guest: string;
  readonly: boolean;
  error?: string;
}

interface Props {
  shield: ShieldView;
  run: (action: () => Promise<unknown>) => void;
}

export function Inspector({ shield, run }: Props) {
  const { id, network, status } = shield;
  const piReady =
    network.allowAll ||
    (network.domains.includes(NPM_REGISTRY) && PROVIDERS.some((d) => network.domains.includes(d)));
  const [draft, setDraft] = useState<MountDraft>();
  const [domain, setDomain] = useState("");
  const [domainProblem, setDomainProblem] = useState<string>();

  async function pickMount() {
    const host = await open({ directory: true, title: "Mount a folder into the shield" });
    if (host === null) return;
    const name = host.split("/").filter(Boolean).pop() ?? "data";
    setDraft({ host, guest: `/mnt/${name}`, readonly: true });
  }

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

  function addDomain(event: FormEvent) {
    event.preventDefault();
    const error = domainError(domain);
    if (error) return setDomainProblem(error);
    run(() =>
      setNetworkPolicy(id, { ...network, domains: [...network.domains, domain.trim().toLowerCase()] }).then(
        () => setDomain(""),
        (e) => setDomainProblem(String(e)),
      ),
    );
  }

  function setPresets(selected: string[]) {
    const kept = network.domains.filter((d) => !PRESETS.includes(d) || selected.includes(d));
    const added = selected.filter((d) => !kept.includes(d));
    run(() => setNetworkPolicy(id, { ...network, domains: [...kept, ...added] }));
  }

  return (
    <aside className="inspector">
      <Card variant="outlined" padding="sm">
        <CardHeader
          title={shield.name}
          titleAs="h2"
          description={<span className="mono">{shield.folder} → /workspace</span>}
          actions={
            status.state === "running" || status.state === "stopping" ? (
              <Button
                size="sm"
                variant="secondary"
                iconLeft={<Square size={14} />}
                loading={status.state === "stopping"}
                onClick={() => run(() => stopShield(id))}
              >
                Stop
              </Button>
            ) : (
              <Button
                size="sm"
                iconLeft={<Play size={14} />}
                loading={status.state === "starting"}
                onClick={() => run(() => startShield(id))}
              >
                Start
              </Button>
            )
          }
        />
        <CardBody>
          {status.state === "error" && <p className="danger">{status.message}</p>}
          {status.state === "running" && (
            <p className={shield.pendingRestart ? "warn" : "muted"}>
              {shield.pendingRestart
                ? "Mounts or network changed. Restart the shield to apply."
                : "Mount and network changes apply on the next start."}
            </p>
          )}
        </CardBody>
      </Card>

      <Card variant="outlined" padding="sm">
        <CardHeader
          title="Mounts"
          titleAs="h3"
          actions={
            <Button
              size="sm"
              variant="outline"
              iconLeft={<FolderPlus size={14} />}
              disabled={draft !== undefined}
              onClick={() => run(pickMount)}
            >
              Add
            </Button>
          }
        />
        <CardBody>
          {draft && (
            <form className="form" onSubmit={confirmMount}>
              <p className="mono grow" title={draft.host}>
                {draft.host}
              </p>
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
          {shield.mounts.length === 0 && !draft && <p className="muted">Only /workspace is visible.</p>}
          <ul className="list">
            {shield.mounts.map((m) => (
              <li key={m.guest} className="row">
                <span className="grow mono" title={m.host}>
                  {m.guest}
                </span>
                <Button
                  size="sm"
                  variant="outline"
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

      <Card variant="outlined" padding="sm">
        <CardHeader title="Network" titleAs="h3" />
        <CardBody className="stack">
          <SegmentedControl
            label="Network mode"
            options={[
              { value: "list", label: "Allow list" },
              { value: "all", label: "Allow all" },
            ]}
            value={network.allowAll ? "all" : "list"}
            onChange={(v) => run(() => setNetworkPolicy(id, { ...network, allowAll: v === "all" }))}
          />
          {network.allowAll ? (
            <p className="warn row">
              <TriangleAlert size={14} />
              Public internet only. Host, LAN and cloud metadata stay blocked.
            </p>
          ) : (
            <>
              {network.domains.length === 0 && <p className="muted">All egress is denied.</p>}
              <ul className="list">
                {network.domains.map((d) => (
                  <li key={d} className="row">
                    <span className="grow mono">{d}</span>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={`Remove ${d}`}
                      iconLeft={<X size={14} />}
                      onClick={() =>
                        run(() =>
                          setNetworkPolicy(id, { ...network, domains: network.domains.filter((x) => x !== d) }),
                        )
                      }
                    />
                  </li>
                ))}
              </ul>
              <form className="row" onSubmit={addDomain}>
                <FloatingField
                  label="Domain or *.domain"
                  className="grow mono"
                  value={domain}
                  hint={domainProblem}
                  aria-invalid={domainProblem !== undefined}
                  showValidity={false}
                  onChange={(e) => {
                    setDomain(e.target.value);
                    setDomainProblem(undefined);
                  }}
                />
                <Button size="sm" type="submit" variant="outline" aria-label="Add domain" iconLeft={<Plus size={14} />} />
              </form>
              {!piReady && (
                <p className="muted">pi needs {NPM_REGISTRY} and the API domain of your LLM provider.</p>
              )}
              <FilterChips
                label="Quick add"
                showSummary={false}
                options={PRESETS.map((d) => ({ value: d, label: d }))}
                value={network.domains.filter((d) => PRESETS.includes(d))}
                onChange={setPresets}
              />
            </>
          )}
        </CardBody>
      </Card>

      <Button
        size="sm"
        variant="danger"
        iconLeft={<Trash2 size={14} />}
        disabled={status.state === "starting" || status.state === "stopping"}
        onClick={() => run(() => deleteShield(id))}
      >
        Delete shield
      </Button>
    </aside>
  );
}
