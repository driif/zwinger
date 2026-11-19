import { useEffect, useState, useSyncExternalStore } from "react";
import type { Prefs } from "../settings/api.ts";
import { listThemes, type Theme, type ThemeList } from "./api.ts";
import { BUILT_INS } from "./builtIns.ts";
import { applyThemeTokens, applyCachedTheme as applyCachedThemeTokens, CACHE } from "./themeCache.ts";

export { BUILT_INS } from "./builtIns.ts";

const DARK = "(prefers-color-scheme: dark)";

/** The theme on the window now. */
let active = BUILT_INS[0];

const themeListeners = new Set<() => void>();
/** The primary color of the theme on the window, `#rrggbb`. `undefined` until it is read after a theme change. */
let themeColor: string | undefined;

/** Applies a theme and notifies the Appearance page. */
export function applyTheme(theme: Theme) {
  applyThemeTokens(theme);
  active = theme;
  themeColor = undefined;
  for (const listener of themeListeners) listener();
}

const hex = (bytes: number[]) => "#" + bytes.map((n) => n.toString(16).padStart(2, "0")).join("");

/** `rgb(…)`, `rgba(…)` or `color(srgb …)` as an opaque `#rrggbb`. Other color spaces give `undefined`. */
function srgbHex(color: string) {
  const srgb = color.startsWith("color(srgb ");
  if (!srgb && !color.startsWith("rgb")) return;
  const numbers = color.slice(srgb ? 11 : 0).match(/[\d.]+(e-?\d+)?/g)?.map(Number);
  if (!numbers || numbers.length < 3) return;
  return hex(numbers.slice(0, 3).map((n) => Math.round(Math.min(255, Math.max(0, srgb ? n * 255 : n)))));
}

/** CSS resolves the token, also a `var()`, short hex, `oklch(…)` or `color(display-p3 …)`. A 1×1 sRGB canvas pixel converts it. */
function readThemeColor() {
  const probe = document.createElement("span");
  probe.hidden = true;
  probe.style.color = "var(--motiv-color-primary)";
  document.body.append(probe);
  const computed = getComputedStyle(probe).color;
  probe.remove();
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const context = canvas.getContext("2d", { colorSpace: "srgb", willReadFrequently: true });
  if (context) {
    context.fillStyle = computed;
    context.fillRect(0, 0, 1, 1);
    const [r, g, b, alpha] = context.getImageData(0, 0, 1, 1).data;
    // A transparent pixel has no color, so the computed components are used.
    if (alpha > 0) return hex([r, g, b]);
  }
  return srgbHex(computed) ?? computed;
}

const subscribeTheme = (listener: () => void) => {
  themeListeners.add(listener);
  return () => themeListeners.delete(listener);
};

/** The primary color of the theme on the window, as `#rrggbb`. It changes after each theme change. */
export const useThemeColor = () => useSyncExternalStore(subscribeTheme, () => (themeColor ??= readThemeColor()));

/** Before the first render: use the last valid theme or the default. */
export function applyCachedTheme() {
  const theme = applyCachedThemeTokens();
  active = theme;
  themeColor = undefined;
  return theme;
}

/** The theme that the preferences select. A missing theme gives the default. */
export function resolveTheme(prefs: Prefs, themes: Theme[], systemDark: boolean) {
  const id = prefs.followSystem ? (systemDark ? prefs.darkTheme : prefs.lightTheme) : prefs.theme;
  return themes.find((t) => t.id === id) ?? BUILT_INS[0];
}

export const isBuiltIn = (id: string) => BUILT_INS.some((t) => t.id === id);

/** The themes for the Appearance page. `preview` shows a draft on the window; no draft shows the selected theme. */
export interface Appearance {
  themes: Theme[];
  /** The theme on the window: the draft, else the selected theme. */
  theme: Theme;
  skipped: string[];
  reload: () => Promise<void>;
  preview: (draft?: Theme) => void;
}

/** Loads the user themes and keeps the selected theme, or a draft, on the window. */
export function useAppearance(prefs?: Prefs): Appearance {
  /** `undefined` until the first list, so a user theme does not flash the default. */
  const [list, setList] = useState<ThemeList>();
  const [draft, setDraft] = useState<Theme>();
  const [systemDark, setSystemDark] = useState(() => matchMedia(DARK).matches);

  const reload = () => listThemes().then(setList);

  useEffect(() => {
    reload().catch(() => setList({ themes: [], skipped: [] }));
    const query = matchMedia(DARK);
    const change = (event: MediaQueryListEvent) => setSystemDark(event.matches);
    query.addEventListener("change", change);
    return () => query.removeEventListener("change", change);
  }, []);

  const themes = [...BUILT_INS, ...(list?.themes ?? []).filter((t) => !isBuiltIn(t.id))];
  const selected = prefs && list && resolveTheme(prefs, themes, systemDark);

  useEffect(() => {
    const theme = draft ?? selected;
    if (!theme) return;
    applyTheme(theme);
    if (draft) return;
    try {
      localStorage.setItem(CACHE, JSON.stringify(theme));
    } catch {
      // The cache only prevents a flash at the next start.
    }
  }, [draft, selected]);

  return { themes, theme: draft ?? selected ?? active, skipped: list?.skipped ?? [], reload, preview: setDraft };
}
