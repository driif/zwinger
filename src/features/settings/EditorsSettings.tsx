import { Button, Card, CardBody, CardHeader, LiquidSwitch, RadioGroup } from "@driif/motiv";
import { RefreshCw } from "lucide-react";
import { Prefs } from "./api";
import { Editors } from "../launcher/api";
import type { Run } from "../../App";

interface Props {
  editors?: Editors;
  prefs: Prefs;
  onPrefs: (prefs: Prefs) => Promise<void>;
  run: Run;
  onDetect: () => void;
}

/** The editors that the launchers offer, and the default of Open in editor. */
export function EditorsSettings({ editors, prefs, onPrefs, run, onDetect }: Props) {
  const on = editors?.editors.filter((e) => e.enabled) ?? [];
  const save = (next: Partial<Prefs["editors"]>) => run(() => onPrefs({ ...prefs, editors: { ...prefs.editors, ...next } }));
  const toggle = (id: string, enabled: boolean) =>
    save({ enabled: enabled ? [...on.map((e) => e.id), id] : on.filter((e) => e.id !== id).map((e) => e.id) });

  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader
          title="Editors"
          titleAs="h2"
          description="The editors in /Applications, ~/Applications and /Applications/Setapp. Open in editor opens the project folder, or the current folder of a terminal."
          actions={
            <Button size="sm" variant="outline" iconLeft={<RefreshCw size={14} />} onClick={onDetect}>
              Detect again
            </Button>
          }
        />
        <CardBody className="stack">
          {!editors && <p className="muted">Looking for editors…</p>}
          {editors?.editors.length === 0 && <p className="muted">No known editor is installed.</p>}
          <ul className="list">
            {editors?.editors.map((editor) => (
              <li key={editor.id} className="row">
                <LiquidSwitch label={editor.name} checked={editor.enabled} onChange={(checked) => toggle(editor.id, checked)} />
                <span className="grow muted mono">{editor.path}</span>
              </li>
            ))}
          </ul>
          {on.length > 0 && (
            <RadioGroup
              name="default-editor"
              legend="Default editor"
              options={on.map((e) => ({ value: e.id, label: e.name }))}
              value={editors?.default ?? ""}
              onChange={(id) => save({ default: id })}
            />
          )}
        </CardBody>
      </Card>
    </div>
  );
}
