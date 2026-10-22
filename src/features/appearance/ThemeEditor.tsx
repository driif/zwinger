import { Button, Card, CardBody, CardFooter, CardHeader, TextField } from "@driif/motiv";
import { useEffect, useState } from "react";
import { Theme } from "./api";

const COLORS: { token: string; label: string }[] = [
  { token: "--motiv-color-primary", label: "Primary" },
  { token: "--motiv-color-secondary", label: "Secondary" },
  { token: "--motiv-color-bg", label: "Background" },
  { token: "--motiv-color-surface", label: "Surface" },
  { token: "--motiv-color-text", label: "Text" },
  { token: "--motiv-color-border", label: "Border" },
  { token: "--motiv-color-danger", label: "Danger" },
  { token: "--motiv-color-success", label: "Success" },
  { token: "--motiv-color-warning", label: "Warning" },
];

const SIZES: { token: string; label: string; hint: string }[] = [
  { token: "--motiv-radius", label: "Corner radius", hint: "For example 0.375rem" },
  { token: "--motiv-font-size", label: "Font size", hint: "For example 0.8125rem. Smaller is more compact." },
];

interface Props {
  theme: Theme;
  /** The window shows each change at once. */
  onPreview: (theme: Theme) => void;
  onSave: (theme: Theme) => void;
  onCancel: () => void;
}

/** Edits a user theme. The window previews the draft until save or cancel. */
export function ThemeEditor({ theme, onPreview, onSave, onCancel }: Props) {
  const [draft, setDraft] = useState(theme);

  useEffect(() => onPreview(draft), [draft]);

  const setToken = (token: string, value: string) => setDraft((d) => ({ ...d, tokens: { ...d.tokens, [token]: value } }));

  return (
    <Card variant="outlined" padding="md">
      <CardHeader title={`Edit ${theme.name}`} titleAs="h2" description="The window shows the changes now. Save keeps them." />
      <CardBody className="stack">
        <TextField label="Name" value={draft.name} maxLength={64} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
        <div className="theme-fields">
          {COLORS.map(({ token, label }) => (
            <TextField
              key={token}
              type="color"
              label={label}
              value={draft.tokens[token] ?? "#000000"}
              onChange={(e) => setToken(token, e.target.value)}
            />
          ))}
        </div>
        <div className="theme-fields">
          {SIZES.map(({ token, label, hint }) => (
            <TextField
              key={token}
              label={label}
              hint={hint}
              value={draft.tokens[token] ?? ""}
              maxLength={32}
              onChange={(e) => setToken(token, e.target.value)}
            />
          ))}
        </div>
      </CardBody>
      <CardFooter className="row end">
        <Button size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <Button size="sm" disabled={!draft.name.trim()} onClick={() => onSave({ ...draft, name: draft.name.trim() })}>
          Save
        </Button>
      </CardFooter>
    </Card>
  );
}
