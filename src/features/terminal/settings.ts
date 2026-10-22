import type { Theme } from "../appearance/api";
import type { GhosttyPalette } from "./api";

const GHOSTTY = "Ghostty";

/**
 * The user theme "Ghostty": the base and tokens of `current`, and the terminal colors of the palette. A color that the
 * palette does not set comes from `current`. It takes the id of an existing "Ghostty" theme, so a new import replaces it.
 */
export function ghosttyTheme(current: Theme, palette: GhosttyPalette, themes: Theme[]): Theme {
  const colors = current.terminal;
  return {
    id: themes.find((t) => t.name === GHOSTTY)?.id ?? "ghostty",
    name: GHOSTTY,
    base: current.base,
    tokens: current.tokens,
    terminal: {
      background: palette.background ?? colors.background,
      foreground: palette.foreground ?? colors.foreground,
      cursor: palette.cursor ?? colors.cursor,
      selection: palette.selection ?? colors.selection,
      ansi: colors.ansi.map((color, i) => palette.ansi[i] ?? color),
    },
  };
}
