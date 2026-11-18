import {
  Badge,
  Button,
  FloatingField,
  SegmentedControl,
  Select,
  SpringCheckbox,
} from "@driif/motiv";
import { KeyRound, Pencil, Plus, Trash2, TriangleAlert } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import { Service, setRuleCredential, storedCredentials } from "./api";
import { CageView, Credential, CredentialSource, Inject, Rule } from "../../shared/api";
import { libraryCredentials } from "../settings/api";
import type { Run } from "../../App";
import { echoHost } from "./echoHost";
import { errorMessage } from "../../shared/errorMessage";
import { credentialHosts, serviceOf } from "./network";
import { expiresSoon } from "../projects/tokenExpiry";

const NAME = /^[A-Z_][A-Z0-9_]*$/;

const PLACES = [
  { key: "query", label: "Query" },
  { key: "body", label: "Body" },
] as const;

const describe = (inject: Inject) =>
  inject.mode === "set"
    ? `${inject.header}: ${inject.template}`
    : `replaces the placeholder in ${[
        ...inject.places.headerFields,
        ...PLACES.filter((p) => inject.places[p.key]).map((p) =>
          p.label.toLowerCase(),
        ),
      ].join(", ")}`;

interface Draft {
  isNew: boolean;
  source: CredentialSource;
  name: string;
  mode: Inject["mode"];
  header: string;
  template: string;
  fields: string;
  query: boolean;
  body: boolean;
  value: string;
  error?: string;
}

function draftOf(credential: Credential): Draft {
  const { inject } = credential;
  return {
    isNew: false,
    source: credential.source ?? "local",
    name: credential.name,
    mode: inject.mode,
    header: inject.mode === "set" ? inject.header : "authorization",
    template: inject.mode === "set" ? inject.template : "Bearer {secret}",
    fields:
      inject.mode === "replace"
        ? inject.places.headerFields.join(", ")
        : "authorization",
    query: inject.mode === "replace" && inject.places.query,
    body: inject.mode === "replace" && inject.places.body,
    value: "",
  };
}

interface Props {
  cage: CageView;
  rule: Rule;
  catalog: Service[];
  run: Run;
}

/**
 * The credentials of a rule. The proxy adds them to the HTTPS requests to the rule's credential hosts.
 * On a Block rule they are inactive: they stay, but the proxy adds them nowhere.
 */
export function RuleCredentials({ cage, rule, catalog, run }: Props) {
  const credentials = rule.credentials ?? [];
  const hosts = credentialHosts(rule, catalog);
  const [header, template] = serviceOf(rule.target, catalog)?.credential ?? [
    "authorization",
    "Bearer {secret}",
  ];
  const [draft, setDraft] = useState<Draft>();
  const [removing, setRemoving] = useState<string>();
  // Rules can share a name, and with it one value.
  const [stored, setStored] = useState<Set<string>>();
  const [library, setLibrary] = useState<Set<string>>();
  const [vaultError, setVaultError] = useState<string>();

  useEffect(() => {
    Promise.all([storedCredentials(cage.id), libraryCredentials()]).then(
      ([names, entries]) => {
        setStored(new Set(names));
        setLibrary(new Set(entries.map((e) => e.name)));
        setVaultError(undefined);
      },
      (e) => setVaultError(errorMessage(e)),
    );
  }, [cage]);

  function save(event: FormEvent) {
    event.preventDefault();
    if (!draft) return;
    const name = draft.name.trim();
    if (!NAME.test(name))
      return setDraft({
        ...draft,
        error: "Use A–Z, 0–9 and _. Do not start with a digit.",
      });
    if (draft.isNew && credentials.some((c) => c.name === name))
      return setDraft({
        ...draft,
        error: `${name} is already on this rule. Edit it instead.`,
      });
    const split = (text: string) => text.split(/[\s,]+/).filter(Boolean);
    const inject: Inject =
      draft.mode === "set"
        ? { mode: "set", header: draft.header, template: draft.template }
        : {
            mode: "replace",
            places: {
              headerFields: split(draft.fields),
              query: draft.query,
              body: draft.body,
            },
          };
    run(() =>
      setRuleCredential(
        cage.id,
        rule.id,
        name,
        inject,
        draft.source,
        draft.source === "library" || draft.value === "" ? null : draft.value,
      ).then(
        () => setDraft(undefined),
        (e) => setDraft({ ...draft, value: "", error: errorMessage(e) }),
      ),
    );
  }

  const needsValue =
    draft?.source === "local" &&
    (!stored?.has(draft.name.trim()) ||
      // A switch from the library needs a value of this cage.
      credentials.find((c) => c.name === draft.name)?.source === "library");
  // A host that echoes the query or the body shows the value to the agent.
  const readBack = (places: { query: boolean; body: boolean }) =>
    (places.query || places.body) &&
    hosts.map((host) => (
      <span key={host} className="warn row">
        <TriangleAlert size={14} />
        The agent can read this value back from {host}.
      </span>
    ));

  return (
    <div className="stack">
      {rule.action === "block" ? (
        <p className="warn row">
          <TriangleAlert size={14} />
          Inactive: the rule blocks{" "}
          <span className="mono">{hosts.join(", ")}</span>, so the proxy adds
          these nowhere. Allow the rule to use them.
        </p>
      ) : (
        <p className="muted">
          Goes to <span className="mono">{hosts.join(", ")}</span> over HTTPS
          only. The agent sees a placeholder, never the value.
        </p>
      )}
      {hosts.filter(echoHost).map((host) => (
        <span key={host} className="warn row">
          <TriangleAlert size={14} />
          {host} echoes requests back, so the agent can read the value.
        </span>
      ))}
      {vaultError && (
        <p className="danger row">
          <TriangleAlert size={14} />
          {vaultError}
        </p>
      )}
      <ul className="list">
        {credentials.map((credential) => (
          <li key={credential.name} className="stack">
            <div className="row">
              <KeyRound size={14} />
              <span
                className="mono"
                title={`$${credential.name} is $MSB_${credential.name}$ in the cage`}
              >
                {credential.name}
              </span>
              <Badge
                size="sm"
                tone={credential.source === "library" ? "success" : "neutral"}
                title={
                  credential.source === "library"
                    ? "The shared value from Settings > Credentials"
                    : "The value of this cage"
                }
              >
                {credential.source === "library" ? "library" : "this cage"}
              </Badge>
              <span className="grow muted mono">
                {describe(credential.inject)}
              </span>
              {credential.source === "library" && library && !library.has(credential.name) && (
                <span
                  className="warn row"
                  title="The library has no entry of this name. Add it in Settings > Credentials."
                >
                  <TriangleAlert size={14} />
                  Not in library
                </span>
              )}
              {credential.source !== "library" && stored && !stored.has(credential.name) && (
                <span
                  className="warn row"
                  title="It has no stored value. Edit it and enter the value again."
                >
                  <TriangleAlert size={14} />
                  Not set
                </span>
              )}
              {credential.name === "CLAUDE_CODE_OAUTH_TOKEN" && cage.claudeToken?.expiresAt && (
                <span className={expiresSoon(cage.claudeToken.expiresAt) ? "warn" : "muted"}>
                  expires {new Date(cage.claudeToken.expiresAt).toLocaleDateString()}
                </span>
              )}
              {removing === credential.name ? (
                <>
                  <span className="danger">Remove?</span>
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => setRemoving(undefined)}
                  >
                    Cancel
                  </Button>
                  <Button
                    size="sm"
                    variant="danger"
                    onClick={() =>
                      run(() =>
                        setRuleCredential(
                          cage.id,
                          rule.id,
                          credential.name,
                          null,
                          credential.source ?? "local",
                          null,
                        ).then(() => setRemoving(undefined)),
                      )
                    }
                  >
                    Remove
                  </Button>
                </>
              ) : (
                <>
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={`Edit ${credential.name}`}
                    iconLeft={<Pencil size={14} />}
                    onClick={() => setDraft(draftOf(credential))}
                  />
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={`Remove ${credential.name}`}
                    iconLeft={<Trash2 size={14} />}
                    onClick={() => setRemoving(credential.name)}
                  />
                </>
              )}
            </div>
            {credential.inject.mode === "replace" &&
              readBack(credential.inject.places)}
          </li>
        ))}
      </ul>
      {draft ? (
        <form className="stack" onSubmit={save}>
          <SegmentedControl
            label="Value from"
            options={[
              { value: "local", label: "This cage" },
              { value: "library", label: "Credential library" },
            ]}
            value={draft.source}
            onChange={(source) =>
              setDraft({
                ...draft,
                source: source as CredentialSource,
                value: "",
                error: undefined,
              })
            }
          />
          {draft.source === "library" && draft.isNew ? (
            <Select
              label="Library entry"
              placeholder={library?.size ? "Pick an entry" : "The library is empty. Add one in Settings > Credentials."}
              options={[...(library ?? [])].map((name) => ({ value: name, label: name }))}
              value={draft.name}
              onChange={(name) => setDraft({ ...draft, name, error: undefined })}
            />
          ) : (
            <FloatingField
              label={draft.source === "library" ? "Library entry" : "Variable name"}
              className="mono"
              value={draft.name}
              disabled={!draft.isNew}
              onChange={(e) =>
                setDraft({
                  ...draft,
                  name: e.target.value.toUpperCase(),
                  error: undefined,
                })
              }
            />
          )}
          {draft.source === "library" && stored?.has(draft.name) && (
            <p className="muted">
              This cage also has its own value of {draft.name}. This rule uses the library value; other rules keep
              theirs.
            </p>
          )}
          <SegmentedControl
            label="How the proxy adds it"
            options={[
              { value: "set", label: "Set a header" },
              { value: "replace", label: "Replace the placeholder" },
            ]}
            value={draft.mode}
            onChange={(mode) =>
              setDraft({
                ...draft,
                mode: mode as Inject["mode"],
                error: undefined,
              })
            }
          />
          {draft.mode === "set" ? (
            <>
              <p className="muted">
                Each request gets the header, also when the agent sends none.
              </p>
              <div className="row">
                <FloatingField
                  label="Header"
                  className="mono"
                  value={draft.header}
                  onChange={(e) =>
                    setDraft({
                      ...draft,
                      header: e.target.value,
                      error: undefined,
                    })
                  }
                />
                <FloatingField
                  label="Value template"
                  hint="{secret} is the value. {basic:user} is Basic credentials of user and the value."
                  className="grow mono"
                  value={draft.template}
                  onChange={(e) =>
                    setDraft({
                      ...draft,
                      template: e.target.value,
                      error: undefined,
                    })
                  }
                />
              </div>
            </>
          ) : (
            <>
              <p className="muted">
                The value goes only where the agent sends{" "}
                <span className="mono">${draft.name || "NAME"}</span>.
              </p>
              <FloatingField
                label="Header fields, separated by commas"
                className="mono"
                value={draft.fields}
                onChange={(e) =>
                  setDraft({
                    ...draft,
                    fields: e.target.value,
                    error: undefined,
                  })
                }
              />
              <div className="row">
                <span className="muted">Also in</span>
                {PLACES.map(({ key, label }) => (
                  <SpringCheckbox
                    key={key}
                    label={label}
                    checked={draft[key]}
                    onChange={(e) =>
                      setDraft({
                        ...draft,
                        [key]: e.target.checked,
                        error: undefined,
                      })
                    }
                  />
                ))}
              </div>
              {readBack(draft)}
            </>
          )}
          {draft.source === "local" ? (
            <FloatingField
              label={
                needsValue ? "Value" : "New value (empty keeps the current value)"
              }
              type="password"
              autoComplete="off"
              value={draft.value}
              hint={draft.error}
              aria-invalid={draft.error !== undefined}
              showValidity={false}
              onChange={(e) =>
                setDraft({ ...draft, value: e.target.value, error: undefined })
              }
            />
          ) : (
            <p className={draft.error ? "danger" : "muted"}>
              {draft.error ??
                "The value stays in the library. A rotation there reaches this rule at once."}
            </p>
          )}
          <div className="row end">
            <Button variant="ghost" onClick={() => setDraft(undefined)}>
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={(needsValue && draft.value === "") || (draft.source === "library" && draft.name === "")}
            >
              Save
            </Button>
          </div>
        </form>
      ) : (
        <div className="row">
          <Button
            size="sm"
            variant="outline"
            iconLeft={<Plus size={14} />}
            onClick={() =>
              setDraft({
                isNew: true,
                source: "local",
                name: "",
                mode: "set",
                header,
                template,
                fields: header,
                query: false,
                body: false,
                value: "",
              })
            }
          >
            Add credential
          </Button>
        </div>
      )}
    </div>
  );
}
