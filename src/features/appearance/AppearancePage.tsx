import { Button, Card, CardBody, CardHeader, LiquidSwitch, SegmentedControl, Select } from "@driif/motiv";
import { Check, Copy, Download, Pencil, Trash2, TriangleAlert, Upload } from "lucide-react";
import { useEffect, useState } from "react";
import { Prefs, PrefsPatch } from "../settings/api";
import type { Run } from "../../App";
import { Notice } from "../../shared/Notice";
import { deleteTheme, exportTheme, importAndReload, saveTheme, Theme } from "./api";
import { ThemeEditor } from "./ThemeEditor";
import { Appearance, BUILT_INS, isBuiltIn } from "./themes";
import "./appearance.css";

const SWATCH = ["--motiv-color-surface", "--motiv-color-primary", "--motiv-color-text", "--motiv-color-border"];

interface Props {
  appearance: Appearance;
  prefs: Prefs;
  onPrefs: (patch: PrefsPatch) => Promise<void>;
  run: Run;
}

/** Picks the theme, or a light and a dark theme that follow the system. Edits the user themes. */
export function AppearancePage({ appearance, prefs, onPrefs, run }: Props) {
  const { themes, skipped, reload, preview } = appearance;
  const [editing, setEditing] = useState<Theme>();
  const [asking, setAsking] = useState<string>();
  const [imported, setImported] = useState<string>();

  // A draft must not stay on the window after the page closes.
  useEffect(() => () => preview(undefined), []);

  /** With follow-system, the theme takes the light or the dark place of its base. */
  const slot = (theme: Theme): "theme" | "lightTheme" | "darkTheme" =>
    !prefs.followSystem ? "theme" : theme.base === "dark" ? "darkTheme" : "lightTheme";
  const use = (theme: Theme) => run(() => onPrefs({ [slot(theme)]: theme.id }));

  function duplicate(theme: Theme) {
    setAsking(undefined);
    setEditing({ ...theme, id: `custom-${Date.now().toString(36)}`, name: `${theme.name} copy` });
  }

  const save = (theme: Theme) =>
    run(async () => {
      await saveTheme(theme);
      await reload();
      // A new theme is selected before the draft goes, so the window does not flash the old theme.
      if (!themes.some((t) => t.id === theme.id)) await onPrefs({ [slot(theme)]: theme.id });
      setEditing(undefined);
      preview(undefined);
    });

  const remove = (theme: Theme) =>
    run(async () => {
      await deleteTheme(theme.id);
      setAsking(undefined);
      await reload();
      // A deleted theme falls back to the built-in default of its place.
      if (![prefs.theme, prefs.lightTheme, prefs.darkTheme].includes(theme.id)) return;
      const fallbacks = { theme: BUILT_INS[0].id, lightTheme: BUILT_INS[1].id, darkTheme: BUILT_INS[0].id };
      await onPrefs(Object.fromEntries(Object.entries(fallbacks).filter(([slot]) => prefs[slot as keyof typeof fallbacks] === theme.id)));
    });

  // A cancel or an error keeps the last notice.
  const importFile = () =>
    run(async () => {
      const message = await importAndReload(reload);
      if (message) setImported(message);
    });

  const inUse = (theme: Theme) => prefs[slot(theme)] === theme.id;

  return (
    <div className="tab-body">
      {skipped.length > 0 && (
        <Notice icon={<TriangleAlert size={14} className="warn" />}>
          These theme files are not valid and were skipped: <span className="mono">{skipped.join(", ")}</span>
        </Notice>
      )}
      <Card variant="outlined" padding="md">
        <CardHeader title="Layout" titleAs="h2" description="Changes apply at once." />
        <CardBody className="stack">
          <SegmentedControl
            label="Density"
            options={[{ value: "default", label: "Default" }, { value: "compact", label: "Compact" }]}
            value={prefs.density}
            onChange={(density) => run(() => onPrefs({ density: density as Prefs["density"] }))}
          />
          <Select
            label="Sidebar font size"
            options={[{ value: "", label: "Theme default" }, ...[12, 13, 14, 15].map((size) => ({ value: String(size), label: `${size} px` }))]}
            value={prefs.sidebarFontSize === null ? "" : String(prefs.sidebarFontSize)}
            onChange={(size) => run(() => onPrefs({ sidebarFontSize: size ? Number(size) : null }))}
          />
        </CardBody>
      </Card>
      <Card variant="outlined" padding="md">
        <CardHeader
          title="Theme"
          titleAs="h2"
          description="Built-in themes are read-only. Duplicate one to make your own theme."
          actions={
            <Button size="sm" variant="outline" iconLeft={<Upload size={14} />} onClick={importFile}>
              Import theme…
            </Button>
          }
        />
        <CardBody className="stack">
          {imported && (
            <Notice icon={<Check size={14} />} onDismiss={() => setImported(undefined)}>
              {imported}
            </Notice>
          )}
          <LiquidSwitch
            label="Follow the system: use a light theme and a dark theme as macOS changes its appearance"
            checked={prefs.followSystem}
            onChange={(followSystem) => run(() => onPrefs({ followSystem }))}
          />
          <ul className="theme-grid">
            {themes.map((theme) => (
              <li key={theme.id}>
                <Card variant={inUse(theme) ? "subtle" : "outlined"} padding="sm" className="stack">
                  <div className="theme-swatch" style={{ background: theme.tokens["--motiv-color-bg"] }} aria-hidden>
                    {SWATCH.map((token) => (
                      <span key={token} style={{ background: theme.tokens[token] }} />
                    ))}
                  </div>
                  <div className="row">
                    <strong className="grow">{theme.name}</strong>
                    <span className="muted">
                      {theme.base}
                      {isBuiltIn(theme.id) && " · built-in"}
                    </span>
                  </div>
                  {asking === theme.id ? (
                    <div className="row end">
                      <span className="grow muted">Delete this theme?</span>
                      <Button size="sm" variant="ghost" onClick={() => setAsking(undefined)}>
                        Cancel
                      </Button>
                      <Button size="sm" variant="danger" onClick={() => remove(theme)}>
                        Delete
                      </Button>
                    </div>
                  ) : (
                    <div className="row">
                      <Button
                        size="sm"
                        variant={inUse(theme) ? "ghost" : "outline"}
                        iconLeft={inUse(theme) ? <Check size={14} /> : undefined}
                        disabled={inUse(theme)}
                        onClick={() => use(theme)}
                      >
                        {inUse(theme) ? "In use" : prefs.followSystem ? `Use for ${theme.base}` : "Use"}
                      </Button>
                      <span className="grow" />
                      <Button size="sm" variant="ghost" iconLeft={<Copy size={14} />} onClick={() => duplicate(theme)}>
                        Duplicate
                      </Button>
                      {!isBuiltIn(theme.id) && (
                        <>
                          <Button size="sm" variant="ghost" iconLeft={<Download size={14} />} onClick={() => run(() => exportTheme(theme.id))}>
                            Export
                          </Button>
                          <Button size="sm" variant="ghost" aria-label={`Edit ${theme.name}`} iconLeft={<Pencil size={14} />} onClick={() => setEditing(theme)} />
                          <Button
                            size="sm"
                            variant="ghost"
                            aria-label={`Delete ${theme.name}`}
                            iconLeft={<Trash2 size={14} />}
                            onClick={() => setAsking(theme.id)}
                          />
                        </>
                      )}
                    </div>
                  )}
                </Card>
              </li>
            ))}
          </ul>
        </CardBody>
      </Card>
      {editing && (
        <ThemeEditor
          key={editing.id}
          theme={editing}
          onPreview={preview}
          onSave={save}
          onCancel={() => {
            setEditing(undefined);
            preview(undefined);
          }}
        />
      )}
    </div>
  );
}
