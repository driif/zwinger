import { Button, Card, CardBody, CardHeader, FloatingField, SpringCheckbox } from "@driif/motiv";
import { KeyRound, Pencil, Plus, Trash2, TriangleAlert } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import { addDomain, listSecrets, setNetworkMode, NetworkPolicy, Places, putSecret, removeSecret, SecretView, ShieldView } from "./api";
import type { Run } from "./App";

const PRESETS = [
  { label: "Anthropic", name: "ANTHROPIC_API_KEY", hosts: ["api.anthropic.com"], fields: ["x-api-key", "authorization"] },
  { label: "OpenAI", name: "OPENAI_API_KEY", hosts: ["api.openai.com"], fields: ["authorization"] },
  { label: "Gemini", name: "GEMINI_API_KEY", hosts: ["generativelanguage.googleapis.com"], fields: ["x-goog-api-key"] },
  { label: "GitHub", name: "GITHUB_TOKEN", hosts: ["api.github.com", "github.com"], fields: ["authorization"] },
];

const PLACES: { key: "query" | "body"; label: string }[] = [
  { key: "query", label: "Query" },
  { key: "body", label: "Body" },
];

const NO_PLACES: Places = { headerFields: [], query: false, body: false };

const describe = (places: Places) =>
  [...places.headerFields, ...PLACES.filter((p) => places[p.key]).map((p) => p.label.toLowerCase())].join(", ");

const NAME = /^[A-Z_][A-Z0-9_]*$/;

/** If the allow-list entry `domain` lets requests to the exact `host` out. */
function covers(domain: string, host: string) {
  if (!domain.startsWith("*.")) return domain === host;
  const base = domain.slice(2);
  return host === base || host.endsWith(`.${base}`);
}

/** The same rules as `sandbox_policy` in the backend. */
export const reachable = (network: NetworkPolicy, host: string) =>
  network.mode === "public" || (network.mode === "list" && network.domains.some((d) => covers(d, host)));

/** If a request to the allow-list entry `domain` can get one of the secret `hosts`. Secret hosts are exact. */
export const coveredBy = (domain: string, hosts: string[]) => hosts.some((h) => covers(domain, h));

interface Draft {
  isNew: boolean;
  name: string;
  hosts: string;
  fields: string;
  places: Places;
  value: string;
  error?: string;
}

interface Props {
  shield: ShieldView;
  run: Run;
}

export function SecretsSection({ shield, run }: Props) {
  const { id, network } = shield;
  const [secrets, setSecrets] = useState<SecretView[]>([]);
  const [reloads, setReloads] = useState(0);
  const [draft, setDraft] = useState<Draft>();
  const [removing, setRemoving] = useState<string>();
  const config = JSON.stringify(shield.secrets);

  useEffect(() => {
    let current = true;
    listSecrets(id).then((next) => current && setSecrets(next));
    return () => {
      current = false;
    };
  }, [id, config, reloads]);

  const reload = () => setReloads((n) => n + 1);

  function save(event: FormEvent) {
    event.preventDefault();
    if (!draft) return;
    const name = draft.name.trim();
    if (!NAME.test(name)) return setDraft({ ...draft, error: "Use A–Z, 0–9 and _. Do not start with a digit." });
    if (draft.isNew && secrets.some((s) => s.name === name))
      return setDraft({ ...draft, error: `${name} already exists. Edit it instead.` });
    const hosts = draft.hosts.split(/[\s,]+/).filter(Boolean);
    const places = { ...draft.places, headerFields: draft.fields.split(/[\s,]+/).filter(Boolean) };
    run(() =>
      putSecret(id, { name, hosts, places }, draft.value === "" ? null : draft.value).then(
        () => {
          setDraft(undefined);
          reload();
        },
        (e) => setDraft({ ...draft, value: "", error: String(e) }),
      ),
    );
  }

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Secrets"
        titleAs="h2"
        description="The agent sees only a placeholder. The proxy puts in the value for the listed hosts only. Changes apply on the next start."
      />
      <CardBody className="stack">
        {secrets.length === 0 && <p className="muted">No secrets.</p>}
        <ul className="list">
          {secrets.map((secret) => (
            <li key={secret.name} className="stack">
              <div className="row">
                <KeyRound size={14} />
                <span className="grow mono" title={`${secret.name}=${secret.placeholder}`}>
                  {secret.name}
                </span>
                {!secret.set && <span className="danger">No value</span>}
                {removing === secret.name ? (
                  <>
                    <span className="danger">Remove?</span>
                    <Button size="sm" variant="ghost" onClick={() => setRemoving(undefined)}>
                      Cancel
                    </Button>
                    <Button
                      size="sm"
                      variant="danger"
                      onClick={() => run(() => removeSecret(id, secret.name).then(() => setRemoving(undefined)))}
                    >
                      Remove
                    </Button>
                  </>
                ) : (
                  <>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={`Edit ${secret.name}`}
                      iconLeft={<Pencil size={14} />}
                      onClick={() =>
                        setDraft({
                          isNew: false,
                          name: secret.name,
                          hosts: secret.hosts.join(", "),
                          fields: secret.places.headerFields.join(", "),
                          places: secret.places,
                          value: "",
                        })
                      }
                    />
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={`Remove ${secret.name}`}
                      iconLeft={<Trash2 size={14} />}
                      onClick={() => setRemoving(secret.name)}
                    />
                  </>
                )}
              </div>
              {secret.hosts.map((host) => (
                <div key={host} className="row">
                  <span className="grow mono muted">
                    {host} · {describe(secret.places)}
                  </span>
                  {!reachable(network, host) && (
                    <>
                      <span className="warn row">
                        <TriangleAlert size={14} />
                        {network.mode === "deny" ? "Deny all blocks this host" : "Not on the allow list"}
                      </span>
                      <Button
                        size="sm"
                        variant="outline"
                        onClick={() =>
                          run(async () => {
                            if (network.mode === "deny") await setNetworkMode(id, "list");
                            await addDomain(id, host);
                          })
                        }
                      >
                        {network.mode === "deny" ? "Use allow list and add" : "Add to allow list"}
                      </Button>
                    </>
                  )}
                </div>
              ))}
            </li>
          ))}
        </ul>

        {draft ? (
          <form className="stack" onSubmit={save}>
            <FloatingField
              label="Variable name"
              className="mono"
              value={draft.name}
              disabled={!draft.isNew}
              onChange={(e) => setDraft({ ...draft, name: e.target.value.toUpperCase(), error: undefined })}
            />
            <FloatingField
              label="Hosts, separated by commas"
              className="mono"
              value={draft.hosts}
              onChange={(e) => setDraft({ ...draft, hosts: e.target.value, error: undefined })}
            />
            <FloatingField
              label="Header fields, separated by commas"
              className="mono"
              value={draft.fields}
              onChange={(e) => setDraft({ ...draft, fields: e.target.value, error: undefined })}
            />
            <div className="row">
              <span className="muted">Also put the value in</span>
              {PLACES.map(({ key, label }) => (
                <SpringCheckbox
                  key={key}
                  label={label}
                  checked={draft.places[key]}
                  onChange={(e) =>
                    setDraft({ ...draft, places: { ...draft.places, [key]: e.target.checked }, error: undefined })
                  }
                />
              ))}
            </div>
            {(draft.places.query || draft.places.body) && (
              <span className="warn row">
                <TriangleAlert size={14} />
                A host that echoes the query or the body back can show the value to the agent.
              </span>
            )}
            <FloatingField
              label={draft.isNew ? "Value" : "New value (empty keeps the current value)"}
              type="password"
              autoComplete="off"
              value={draft.value}
              hint={draft.error}
              aria-invalid={draft.error !== undefined}
              showValidity={false}
              onChange={(e) => setDraft({ ...draft, value: e.target.value, error: undefined })}
            />
            <div className="row end">
              <Button variant="ghost" onClick={() => setDraft(undefined)}>
                Cancel
              </Button>
              <Button type="submit" disabled={draft.isNew && draft.value === ""}>
                Save
              </Button>
            </div>
          </form>
        ) : (
          <div className="row">
            <Button
              variant="outline"
              iconLeft={<Plus size={14} />}
              onClick={() =>
                setDraft({ isNew: true, name: "", hosts: "", fields: "authorization", places: NO_PLACES, value: "" })
              }
            >
              Add secret
            </Button>
            {PRESETS.filter((p) => !secrets.some((s) => s.name === p.name)).map((preset) => (
              <Button
                key={preset.name}
                size="sm"
                variant="ghost"
                onClick={() =>
                  setDraft({
                    isNew: true,
                    name: preset.name,
                    hosts: preset.hosts.join(", "),
                    fields: preset.fields.join(", "),
                    places: NO_PLACES,
                    value: "",
                  })
                }
              >
                {preset.label}
              </Button>
            ))}
          </div>
        )}
      </CardBody>
    </Card>
  );
}
