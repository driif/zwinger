import { call } from "../../shared/api";

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
