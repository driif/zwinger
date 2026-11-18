import { Button, Card, CardBody, CardHeader, FloatingField } from "@driif/motiv";
import { KeyRound, Plus, RefreshCw, Trash2, TriangleAlert } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import { deleteLibraryCredential, LibraryCredential, libraryCredentials, setLibraryCredential } from "./api";
import { errorMessage } from "../../shared/errorMessage";

const NAME = /^[A-Z_][A-Z0-9_]*$/;

interface Draft {
  /** Set when the draft rotates an existing entry. */
  rotate: boolean;
  name: string;
  value: string;
  error?: string;
}

/**
 * The credential library: values that each cage can attach to its rules. The UI writes a value and never reads it.
 * A new entry is attached nowhere. A delete is refused while a rule attaches the entry.
 */
export function LibraryCredentials() {
  const [entries, setEntries] = useState<LibraryCredential[]>();
  const [error, setError] = useState<string>();
  const [draft, setDraft] = useState<Draft>();
  const [removing, setRemoving] = useState<string>();

  const load = () =>
    libraryCredentials().then(
      (list) => {
        setEntries(list);
        setError(undefined);
      },
      (e) => {
        // A stale list must not allow Add.
        setEntries(undefined);
        setError(errorMessage(e));
      },
    );

  useEffect(() => {
    load();
  }, []);

  function save(event: FormEvent) {
    event.preventDefault();
    if (!draft) return;
    const name = draft.name.trim();
    if (!NAME.test(name)) return setDraft({ ...draft, error: "Use A–Z, 0–9 and _. Do not start with a digit." });
    if (!draft.rotate && entries?.some((e) => e.name === name))
      return setDraft({ ...draft, error: `${name} is already in the library. Rotate it instead.` });
    setLibraryCredential(name, draft.value, draft.rotate).then(
      () => {
        setDraft(undefined);
        load();
      },
      (e) => {
        setDraft({ ...draft, value: "", error: errorMessage(e) });
        // The list can be stale, for example after an edit in another window.
        load();
      },
    );
  }

  function remove(name: string) {
    deleteLibraryCredential(name).then(
      () => {
        setRemoving(undefined);
        load();
      },
      (e) => {
        setRemoving(undefined);
        setError(errorMessage(e));
      },
    );
  }

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Credential library"
        titleAs="h2"
        description="Values that each cage can attach to its rules. A new entry goes to no cage until a rule attaches it. A rotation reaches each running cage that attaches it at once."
      />
      <CardBody className="stack">
        {error && (
          <p className="danger row">
            <TriangleAlert size={14} />
            <span className="grow">{error}</span>
            {entries === undefined && (
              <Button size="sm" variant="outline" onClick={load}>
                Retry
              </Button>
            )}
          </p>
        )}
        {entries?.length === 0 && <p className="muted">The library is empty.</p>}
        <ul className="list">
          {entries?.map((entry) => (
            <li key={entry.name} className="row">
              <KeyRound size={14} />
              <span className="mono">{entry.name}</span>
              <span className="grow muted">
                {entry.savedAt ? `saved ${new Date(entry.savedAt).toLocaleDateString()}` : ""}
              </span>
              {removing === entry.name ? (
                <>
                  <span className="danger">Delete?</span>
                  <Button size="sm" variant="ghost" onClick={() => setRemoving(undefined)}>
                    Cancel
                  </Button>
                  <Button size="sm" variant="danger" onClick={() => remove(entry.name)}>
                    Delete
                  </Button>
                </>
              ) : (
                <>
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={`Rotate ${entry.name}`}
                    iconLeft={<RefreshCw size={14} />}
                    onClick={() => setDraft({ rotate: true, name: entry.name, value: "" })}
                  />
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={`Delete ${entry.name}`}
                    iconLeft={<Trash2 size={14} />}
                    onClick={() => {
                      setError(undefined);
                      setRemoving(entry.name);
                    }}
                  />
                </>
              )}
            </li>
          ))}
        </ul>
        {draft ? (
          <form className="stack" onSubmit={save}>
            <FloatingField
              label="Variable name"
              className="mono"
              value={draft.name}
              disabled={draft.rotate}
              onChange={(e) => setDraft({ ...draft, name: e.target.value.toUpperCase(), error: undefined })}
            />
            <FloatingField
              label={draft.rotate ? "New value" : "Value"}
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
              <Button type="submit" disabled={draft.value === ""}>
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
              // Until the list is known, a new entry could replace an existing one.
              disabled={entries === undefined}
              onClick={() => setDraft({ rotate: false, name: "", value: "" })}
            >
              Add to library
            </Button>
          </div>
        )}
      </CardBody>
    </Card>
  );
}
