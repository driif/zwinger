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

export type NumberSetting = "fontSize" | "lineHeight" | "padding" | "scrollback";

/** The labels and limits of the numeric terminal settings. The backend uses the same limits. */
export const NUMBERS: Record<NumberSetting, { label: string; min: number; max: number; step: number }> = {
  fontSize: { label: "Font size", min: 8, max: 32, step: 0.5 },
  lineHeight: { label: "Line height", min: 1, max: 2, step: 0.05 },
  padding: { label: "Padding", min: 0, max: 32, step: 1 },
  scrollback: { label: "Scrollback lines", min: 1000, max: 100000, step: 1000 },
};

/** `value` clamped to the limits of `setting`, and a note if it was out of range. */
export function clampNumber(setting: NumberSetting, value: number) {
  const { min, max } = NUMBERS[setting];
  const clamped = Math.min(max, Math.max(min, value));
  return { value: clamped, note: clamped === value ? undefined : adjustmentNote(setting, String(value), clamped) };
}

/** The note for an `input` out of range that `setting` got as the limit `value`. */
export function adjustmentNote(setting: NumberSetting, input: string, value: number) {
  const { label, max } = NUMBERS[setting];
  return `${label}: ${input} is ${value === max ? "more than the maximum" : "less than the minimum"}. Set to ${value}.`;
}
