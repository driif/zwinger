import { Button, Card, CardBody, CardHeader, LiquidSwitch, SegmentedControl, Select, TextField } from "@driif/motiv";
import { Download } from "lucide-react";
import { useState } from "react";
import type { Run } from "../../App";
import { saveTheme } from "../appearance/api";
import { Appearance } from "../appearance/themes";
import { CursorStyle, Prefs, PrefsPatch, TerminalPrefs } from "../settings/api";
import { importGhosttyConfig } from "./api";
import { adjustmentNote, clampNumber, ghosttyTheme, NUMBERS, NumberSetting } from "./settings";

const CURSORS = [
  { value: "bar", label: "Bar" },
  { value: "block", label: "Block" },
  { value: "underline", label: "Underline" },
];

/** A text field that saves on blur. An empty value is not saved. */
function Field({ label, value, onSave }: { label: string; value: string; onSave: (value: string) => void }) {
  const [draft, setDraft] = useState<string>();
  return (
    <TextField
      label={label}
      value={draft ?? value}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => {
        if (draft !== undefined && draft.trim() !== "" && draft.trim() !== value) onSave(draft.trim());
        setDraft(undefined);
      }}
    />
  );
}

/** A number field that saves on blur. A number out of range is clamped and gives a note, also when the limit is already saved. */
function NumberField({ setting, value, onSave }: {
  setting: NumberSetting;
  value: number;
  onSave: (value: number, note?: string) => void;
}) {
  const [draft, setDraft] = useState<string>();
  const { label, min, max, step } = NUMBERS[setting];
  return (
    <TextField
      label={label}
      type="number"
      min={min}
      max={max}
      step={step}
      value={draft ?? value}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => {
        if (draft !== undefined && draft.trim() !== "" && Number.isFinite(Number(draft))) {
          const clamped = clampNumber(setting, Number(draft));
          if (clamped.note || clamped.value !== value) onSave(clamped.value, clamped.note);
        }
        setDraft(undefined);
      }}
    />
  );
}

interface Props {
  prefs: Prefs;
  onPrefs: (patch: PrefsPatch) => Promise<void>;
  appearance: Appearance;
  run: Run;
}

/** The look and keys of all terminals, and the import of a Ghostty config. */
export function TerminalSettings({ prefs, onPrefs, appearance, run }: Props) {
  const terminal = prefs.terminal;
  const [notes, setNotes] = useState<string[]>([]);
  const save = (change: Partial<TerminalPrefs>) => run(() => onPrefs({ terminal: change }));
  const saveNumber = (setting: NumberSetting) => (value: number, note?: string) =>
    run(async () => {
      if (value !== terminal[setting]) await onPrefs({ terminal: { [setting]: value } });
      setNotes(note ? [note] : []);
    });

  const importGhostty = () =>
    run(async () => {
      setNotes([]);
      const found = await importGhosttyConfig();
      const next: Partial<TerminalPrefs> = { ...found.settings };
      if (found.palette) {
        const theme = ghosttyTheme(appearance.theme, found.palette, appearance.themes);
        await saveTheme(theme);
        await appearance.reload();
        next.themeId = theme.id;
      }
      await onPrefs({ terminal: next });
      setNotes([
        found.ignored.length > 0 ? `Imported. Not used: ${found.ignored.join(", ")}.` : "Imported.",
        ...found.adjusted.map((a) => adjustmentNote(a.setting, `Ghostty ${a.input}`, a.value)),
      ]);
    });

  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader
          title="Terminal"
          titleAs="h2"
          description="The look and keys of all terminals. Changes apply at once."
          actions={
            <Button size="sm" variant="outline" iconLeft={<Download size={14} />} onClick={importGhostty}>
              Import from Ghostty
            </Button>
          }
        />
        <CardBody className="stack">
          <div className="terminal-notes stack" role="status">
            {notes.map((note) => <p key={note} className="muted">{note}</p>)}
          </div>
          <Field label="Font family" value={terminal.fontFamily} onSave={(fontFamily) => save({ fontFamily })} />
          <LiquidSwitch label="Font ligatures" checked={terminal.ligatures} onChange={(ligatures) => save({ ligatures })} />
          <div className="row">
            {(Object.keys(NUMBERS) as NumberSetting[]).map((setting) => (
              <NumberField key={setting} setting={setting} value={terminal[setting]} onSave={saveNumber(setting)} />
            ))}
          </div>
          <SegmentedControl
            label="Cursor"
            options={CURSORS}
            value={terminal.cursorStyle}
            onChange={(value) => save({ cursorStyle: value as CursorStyle })}
          />
          <LiquidSwitch label="Blink the cursor" checked={terminal.cursorBlink} onChange={(cursorBlink) => save({ cursorBlink })} />
          <LiquidSwitch
            label="Option is Meta: Option+B and Option+F move by word"
            checked={terminal.optionAsMeta}
            onChange={(optionAsMeta) => save({ optionAsMeta })}
          />
          <Select
            label="Colors"
            options={[{ value: "", label: "The app theme" }, ...appearance.themes.map((t) => ({ value: t.id, label: t.name }))]}
            value={terminal.themeId ?? ""}
            onChange={(themeId) => save({ themeId: themeId || null })}
          />
        </CardBody>
      </Card>
    </div>
  );
}
