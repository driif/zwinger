import type { Theme } from "./api.ts";
import { BUILT_INS } from "./builtIns.ts";

export const CACHE = "zwinger.theme";
export const DEFAULT_BACKGROUND = BUILT_INS[0].tokens["--motiv-color-bg"];

const isRecord = (value: unknown): value is Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value) && Object.getPrototypeOf(value) === Object.prototype;

export function applyThemeTokens(theme: Pick<Theme, "base" | "tokens">) {
  const { style, dataset } = document.documentElement;
  for (const name of [...style]) {
    if (name.startsWith("--motiv-") || name.startsWith("--zw-")) style.removeProperty(name);
  }
  dataset.theme = theme.base;
  for (const [name, value] of Object.entries(theme.tokens)) style.setProperty(name, value);
  style.backgroundColor = `var(--motiv-color-bg, ${theme.base === "light" ? BUILT_INS[1].tokens["--motiv-color-bg"] : DEFAULT_BACKGROUND})`;
}

export function parseCachedTheme(cached: string | null): Theme {
  if (!cached) return BUILT_INS[0];
  try {
    const parsed: unknown = JSON.parse(cached);
    if (!isRecord(parsed)) return BUILT_INS[0];
    const { base, tokens, terminal } = parsed;
    if ((base !== "light" && base !== "dark") || !isRecord(tokens)
      || !Object.entries(tokens).every(([name, value]) =>
        (name.startsWith("--motiv-") || name.startsWith("--zw-")) && typeof value === "string")) return BUILT_INS[0];

    const safeTerminal = isRecord(terminal)
      && typeof terminal.background === "string"
      && typeof terminal.foreground === "string"
      && typeof terminal.cursor === "string"
      && typeof terminal.selection === "string"
      && Array.isArray(terminal.ansi)
      && terminal.ansi.length === 16
      && terminal.ansi.every((color) => typeof color === "string")
      ? {
          background: terminal.background,
          foreground: terminal.foreground,
          cursor: terminal.cursor,
          selection: terminal.selection,
          ansi: terminal.ansi as string[],
        }
      : BUILT_INS[0].terminal;

    return {
      id: typeof parsed.id === "string" ? parsed.id : BUILT_INS[0].id,
      name: typeof parsed.name === "string" ? parsed.name : BUILT_INS[0].name,
      base,
      tokens: tokens as Record<string, string>,
      terminal: safeTerminal,
    };
  } catch {
    return BUILT_INS[0];
  }
}

export function applyCachedTheme(): Theme {
  let theme = BUILT_INS[0];
  try {
    theme = parseCachedTheme(localStorage.getItem(CACHE));
  } catch {
    // Storage may be unavailable. The default is already selected.
  }
  applyThemeTokens(theme);
  return theme;
}

export function readAppliedBackgroundColor(): string {
  const root = document.documentElement;
  const fallback = root.dataset.theme === "light" ? BUILT_INS[1].tokens["--motiv-color-bg"] : DEFAULT_BACKGROUND;
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const context = canvas.getContext("2d", { willReadFrequently: true });
  if (!context) return fallback;
  const computed = getComputedStyle(root).backgroundColor;
  context.fillStyle = computed;
  context.fillRect(0, 0, 1, 1);
  const [red, green, blue, alpha] = context.getImageData(0, 0, 1, 1).data;
  if (alpha !== 255) return fallback;
  return "#" + [red, green, blue].map((part) => part.toString(16).padStart(2, "0")).join("");
}
