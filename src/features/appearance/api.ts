import { call } from "../../shared/api.ts";

/** The colors of the terminal. `ansi` has the 16 ANSI colors in order. */
export interface TerminalColors {
  background: string;
  foreground: string;
  cursor: string;
  selection: string;
  ansi: string[];
}

/** A look of the app: motiv's light or dark base, with `--motiv-*` and `--zw-*` overrides. */
export interface Theme {
  id: string;
  name: string;
  base: "light" | "dark";
  tokens: Record<string, string>;
  terminal: TerminalColors;
}

/** `skipped`: the file names of bad theme files. */
export interface ThemeList {
  themes: Theme[];
  skipped: string[];
}

/** The user themes in the app data. The built-in themes are in the frontend. */
export const listThemes = () => call<ThemeList>("list_themes");

/** Adds the theme, or replaces the user theme with the same id. */
export const saveTheme = (theme: Theme) => call<void>("save_theme", { theme });

export const deleteTheme = (id: string) => call<void>("delete_theme", { id });

/** `renamed`: a saved theme had `originalName`, so the theme got a number. */
export interface ImportResult {
  theme: Theme;
  originalName: string;
  renamed: boolean;
}

/** Asks for a theme file and adds it as a new theme. `null`: the user cancelled. */
export const importTheme = () => call<ImportResult | null>("import_theme");

/** Asks for a place and writes the theme file there. `false`: the user cancelled. */
export const exportTheme = (id: string) => call<boolean>("export_theme", { id });

/** The text of the notice after an import. */
export const importedMessage = ({ theme, originalName, renamed }: ImportResult) =>
  renamed ? `Imported '${originalName}' as '${theme.name}'` : `Imported '${theme.name}'`;

/**
 * Imports a theme file and reloads the themes. Returns the notice text, or `undefined` when the user cancelled.
 * The prefs do not change: the user selects the new theme with Use.
 */
export async function importAndReload(reload: () => Promise<void>, pick = importTheme) {
  const result = await pick();
  if (!result) return undefined;
  await reload();
  return importedMessage(result);
}
