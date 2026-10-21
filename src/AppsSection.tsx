import { Button, Card, CardBody, CardHeader, FloatingField, SpringCheckbox } from "@driif/motiv";
import { Pencil, Plus, X } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import { App, putApp, removeApp, ShieldView } from "./api";
import type { Intent, Run } from "./App";

type Draft = Omit<App, "id"> & { id?: string; error?: string };

interface Props {
  shield: ShieldView;
  run: Run;
  intent?: Intent;
  onIntentDone: () => void;
}

/** The saved commands of the shield. They start in the shield VM, never on the host. */
export function AppsSection({ shield, run, intent, onIntentDone }: Props) {
  const { id } = shield;
  const [draft, setDraft] = useState<Draft>();
  const blank = (): Draft => ({ name: "", command: "", cwd: shield.harness ?? "/workspace", autostart: false });

  useEffect(() => {
    if (!intent) return;
    onIntentDone();
    const app = shield.apps.find((a) => a.id === intent.appId);
    setDraft(app ? { ...app } : blank());
  }, [intent]);

  function save(event: FormEvent) {
    event.preventDefault();
    if (!draft) return;
    const { error: _, ...app } = draft;
    run(() =>
      putApp(id, app).then(
        () => setDraft(undefined),
        (e) => setDraft({ ...draft, error: String(e) }),
      ),
    );
  }

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Apps"
        titleAs="h2"
        description="Saved commands that run in the shield, such as a dev server. Ports are not published to this computer."
        actions={
          <Button
            size="sm"
            variant="outline"
            iconLeft={<Plus size={14} />}
            disabled={draft !== undefined}
            onClick={() => setDraft(blank())}
          >
            Add app
          </Button>
        }
      />
      <CardBody className="stack">
        {draft && (
          <form className="stack" onSubmit={save}>
            <FloatingField
              label="Name"
              autoFocus
              value={draft.name}
              maxLength={64}
              showValidity={false}
              onChange={(e) => setDraft({ ...draft, name: e.target.value, error: undefined })}
            />
            <FloatingField
              label="Command"
              className="mono"
              value={draft.command}
              showValidity={false}
              onChange={(e) => setDraft({ ...draft, command: e.target.value, error: undefined })}
            />
            <FloatingField
              label="Starts in"
              className="mono"
              value={draft.cwd}
              hint={draft.error}
              aria-invalid={draft.error !== undefined}
              showValidity={false}
              onChange={(e) => setDraft({ ...draft, cwd: e.target.value, error: undefined })}
            />
            <SpringCheckbox
              label="Start with the shield"
              checked={draft.autostart}
              onChange={(e) => setDraft({ ...draft, autostart: e.target.checked })}
            />
            <div className="row end">
              <Button size="sm" variant="ghost" onClick={() => setDraft(undefined)}>
                Cancel
              </Button>
              <Button size="sm" type="submit" disabled={!draft.name.trim() || !draft.command.trim()}>
                Save
              </Button>
            </div>
          </form>
        )}
        {shield.apps.length === 0 && !draft && <p className="muted">No apps.</p>}
        <ul className="list">
          {shield.apps.map((app) => (
            <li key={app.id} className="row">
              <span>{app.name}</span>
              <span className="grow muted mono" title={`${app.cwd} $ ${app.command}`}>
                {app.command}
              </span>
              {app.autostart && <span className="muted">autostart</span>}
              <Button
                size="sm"
                variant="ghost"
                aria-label={`Edit ${app.name}`}
                iconLeft={<Pencil size={14} />}
                disabled={draft !== undefined}
                onClick={() => setDraft({ ...app })}
              />
              <Button
                size="sm"
                variant="ghost"
                aria-label={`Delete ${app.name}`}
                iconLeft={<X size={14} />}
                onClick={() => run(() => removeApp(id, app.id))}
              />
            </li>
          ))}
        </ul>
      </CardBody>
    </Card>
  );
}
