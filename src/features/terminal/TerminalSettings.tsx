import { Button, Card, CardBody, CardHeader, LiquidSwitch, SegmentedControl, Select, TextField } from "@driif/motiv";
import { Download } from "lucide-react";
import { useState } from "react";
import type { Run } from "../../App";
import { saveTheme } from "../appearance/api";
import { Appearance } from "../appearance/themes";
import { CursorStyle, Prefs, TerminalPrefs } from "../settings/api";
import { importGhosttyConfig } from "./api";
import { ghosttyTheme } from "./settings";

const CURSORS = [
  { value: "bar", label: "Bar" },
  { value: "block", label: "Block" },
  { value: "underline", label: "Underline" },
];

/** A text field that saves on blur. A number out of `min`–`max` is clamped; the backend uses the same limits. */
function Field({ label, value, min, max, step, onSave }: {
  label: string;
  value: string | number;
  min?: number;
  max?: number;
  step?: number;
  onSave: (value: string) => void;
}) {
  const [draft, setDraft] = useState<string>();
  const numeric = typeof value === "number";
  const clean = (text: string) =>
    numeric ? String(Math.min(max!, Math.max(min!, Number(text)))) : text.trim();
  const valid = (text: string) => text.trim() !== "" && (!numeric || Number.isFinite(Number(text)));
  return (
    <TextField
      label={label}
      type={numeric ? "number" : "text"}
      min={min}
      max={max}
      step={step}
      value={draft ?? value}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => {
        if (draft !== undefined && valid(draft) && clean(draft) !== String(value)) onSave(clean(draft));
        setDraft(undefined);
      }}
    />
  );
}

interface Props {
  prefs: Prefs;
  onPrefs: (prefs: Prefs) => Promise<void>;
  appearance: Appearance;
  run: Run;
}

/** The look and keys of all terminals, and the import of a Ghostty config. */
export function TerminalSettings({ prefs, onPrefs, appearance, run }: Props) {
  const terminal = prefs.terminal;
  const [note, setNote] = useState<string>();
  const save = (change: Partial<TerminalPrefs>) => run(() => onPrefs({ ...prefs, terminal: { ...terminal, ...change } }));

  const importGhostty = () =>
    run(async () => {
      setNote(undefined);
      const found = await importGhosttyConfig();
      const next = { ...terminal, ...found.settings };
      if (found.palette) {
        const theme = ghosttyTheme(appearance.theme, found.palette, appearance.themes);
        await saveTheme(theme);
        await appearance.reload();
        next.themeId = theme.id;
      }
      await onPrefs({ ...prefs, terminal: next });
      setNote(found.ignored.length > 0 ? `Imported. Not used: ${found.ignored.join(", ")}.` : "Imported.");
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
          {note && <p className="muted">{note}</p>}
          <Field label="Font family" value={terminal.fontFamily} onSave={(fontFamily) => save({ fontFamily })} />
          <div className="row">
            <Field label="Font size" value={terminal.fontSize} min={8} max={32} step={0.5} onSave={(v) => save({ fontSize: Number(v) })} />
            <Field label="Line height" value={terminal.lineHeight} min={1} max={2} step={0.05} onSave={(v) => save({ lineHeight: Number(v) })} />
            <Field label="Padding" value={terminal.padding} min={0} max={32} step={1} onSave={(v) => save({ padding: Number(v) })} />
            <Field label="Scrollback lines" value={terminal.scrollback} min={1000} max={100000} step={1000} onSave={(v) => save({ scrollback: Number(v) })} />
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
