import { Button, Card, CardBody, CardHeader, FilterChips, FloatingField, SegmentedControl } from "@driif/motiv";
import { Ban, Plus, TriangleAlert, X } from "lucide-react";
import { FormEvent, useEffect, useRef, useState } from "react";
import { addDomain, NetworkMode, removeDomain, setNetworkMode, ShieldView } from "./api";
import type { Intent, Run } from "./App";
import { SecretsSection } from "./SecretsSection";

const NPM_REGISTRY = "registry.npmjs.org";
const PROVIDERS = ["api.anthropic.com", "api.openai.com", "generativelanguage.googleapis.com"];
const PRESETS = [NPM_REGISTRY, ...PROVIDERS];

/** The groups that `sandbox_policy` in the backend denies in every mode. */
const ALWAYS_BLOCKED = [
  { label: "This computer", hint: "the host, except DNS" },
  { label: "Local network", hint: "private ranges" },
  { label: "Loopback", hint: "127.0.0.0/8, ::1" },
  { label: "Link-local", hint: "169.254.0.0/16, fe80::/10" },
  { label: "Cloud metadata", hint: "169.254.169.254" },
];

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

interface Props {
  shield: ShieldView;
  run: Run;
  intent?: Intent;
  onIntentDone: () => void;
}

export function NetworkTab({ shield, run, intent, onIntentDone }: Props) {
  const { id, network } = shield;
  const [domain, setDomain] = useState("");
  const [problem, setProblem] = useState<string>();
  const domainField = useRef<HTMLInputElement>(null);
  const focusDomain = useRef(false);
  const presetPending = useRef(false);
  const presets = network.domains.filter((d) => PRESETS.includes(d));
  const piReady = network.domains.includes(NPM_REGISTRY) && PROVIDERS.some((d) => network.domains.includes(d));

  useEffect(() => {
    if (intent?.kind !== "allowList") return;
    onIntentDone();
    focusDomain.current = true;
    if (network.mode !== "list") run(() => setNetworkMode(id, "list"));
  }, [intent]);

  // The domain field exists only in List mode, so the focus waits for it.
  useEffect(() => {
    if (!focusDomain.current || network.mode !== "list") return;
    focusDomain.current = false;
    domainField.current?.focus();
  });

  function submitDomain(event: FormEvent) {
    event.preventDefault();
    const error = domainError(domain);
    if (error) return setProblem(error);
    run(() =>
      addDomain(id, domain).then(
        () => setDomain(""),
        (e) => setProblem(String(e)),
      ),
    );
  }

  /**
   * The chips report the whole selection. Only the chip that changed is sent.
   * Clicks during a pending change are dropped: the chips would compare against
   * a selection that is not refreshed yet.
   */
  function setPresets(selected: string[]) {
    if (presetPending.current) return;
    const added = selected.find((d) => !presets.includes(d));
    const removed = presets.find((d) => !selected.includes(d));
    const change = added ? () => addDomain(id, added) : removed ? () => removeDomain(id, removed) : undefined;
    if (!change) return;
    presetPending.current = true;
    run(change).finally(() => {
      presetPending.current = false;
    });
  }

  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader title="Egress" titleAs="h2" description="Which hosts the agent can reach from inside the shield." />
        <CardBody className="stack">
          <SegmentedControl
            label="Network mode"
            options={[
              { value: "deny", label: "Deny all" },
              { value: "list", label: "Allow list" },
              { value: "public", label: "Public internet" },
            ]}
            value={network.mode}
            onChange={(mode) => run(() => setNetworkMode(id, mode as NetworkMode))}
          />
          {network.mode === "deny" && <p className="muted">The agent cannot reach any host. The allow list is kept.</p>}
          {network.mode === "public" && (
            <p className="warn row">
              <TriangleAlert size={14} />
              The agent can reach every public host. Only the groups below stay blocked.
            </p>
          )}
          {network.mode === "list" && (
            <>
              {network.domains.length === 0 && <p className="muted">No hosts yet. All egress is denied.</p>}
              <ul className="list">
                {network.domains.map((d) => (
                  <li key={d} className="row">
                    <span className="grow mono">{d}</span>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={`Remove ${d}`}
                      iconLeft={<X size={14} />}
                      onClick={() => run(() => removeDomain(id, d))}
                    />
                  </li>
                ))}
              </ul>
              <form className="row" onSubmit={submitDomain}>
                <FloatingField
                  ref={domainField}
                  label="Domain or *.domain"
                  className="grow mono"
                  value={domain}
                  hint={problem}
                  aria-invalid={problem !== undefined}
                  showValidity={false}
                  onChange={(e) => {
                    setDomain(e.target.value);
                    setProblem(undefined);
                  }}
                />
                <Button type="submit" variant="outline" aria-label="Add domain" iconLeft={<Plus size={14} />} />
              </form>
              <FilterChips
                label="Quick add"
                showSummary={false}
                options={PRESETS.map((d) => ({ value: d, label: d }))}
                value={presets}
                onChange={setPresets}
              />
              {!piReady && <p className="muted">pi needs {NPM_REGISTRY} and the API host of your LLM provider.</p>}
            </>
          )}
        </CardBody>
      </Card>
      <SecretsSection shield={shield} run={run} />
      <Card variant="subtle" padding="md">
        <CardHeader title="Always blocked" titleAs="h2" description="Every mode blocks these groups. You cannot change this." />
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
