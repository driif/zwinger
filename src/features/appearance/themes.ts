import { useEffect, useState } from "react";
import { Prefs } from "../settings/api";
import { listThemes, Theme, ThemeList } from "./api";

const XTERM_ANSI = [
  "#1e293b", "#f87171", "#4ade80", "#facc15", "#60a5fa", "#c084fc", "#22d3ee", "#e5e7eb",
  "#64748b", "#fca5a5", "#86efac", "#fde047", "#93c5fd", "#d8b4fe", "#67e8f9", "#ffffff",
];

export const BUILT_INS: Theme[] = [
  {
    id: "zwinger-dark",
    name: "zwinger dark",
    base: "dark",
    tokens: {
      "--motiv-color-primary": "#7dd3fc",
      "--motiv-color-on-primary": "#082f49",
      "--motiv-color-secondary": "#d6af92",
      "--motiv-color-bg": "#28232f",
      "--motiv-color-surface": "#302938",
      "--motiv-color-text": "#e9e0ef",
      "--motiv-color-border": "#756183",
      "--motiv-color-danger": "#efadb7",
      "--motiv-color-success": "#4f8a69",
      "--motiv-color-warning": "#b9793f",
      "--motiv-radius": "0.375rem",
      "--motiv-font-size": "0.8125rem",
    },
    terminal: { background: "#0b1120", foreground: "#e5e7eb", cursor: "#7dd3fc", selection: "#334155", ansi: XTERM_ANSI },
  },
  {
    id: "zwinger-light",
    name: "zwinger light",
    base: "light",
    tokens: {
      "--motiv-color-primary": "#0369a1",
      "--motiv-color-on-primary": "#ffffff",
      "--motiv-color-secondary": "#d4ae96",
      "--motiv-color-bg": "#f2f1f5",
      "--motiv-color-surface": "#ffffff",
      "--motiv-color-text": "#3b3345",
      "--motiv-color-border": "#c0b3cc",
      "--motiv-color-danger": "#a54c5c",
      "--motiv-color-success": "#3f7a59",
      "--motiv-color-warning": "#a5652d",
      "--motiv-radius": "0.375rem",
      "--motiv-font-size": "0.8125rem",
    },
    terminal: {
      background: "#ffffff",
      foreground: "#1f2937",
      cursor: "#0369a1",
      selection: "#bae6fd",
      ansi: [
        "#1f2937", "#b91c1c", "#15803d", "#a16207", "#1d4ed8", "#7e22ce", "#0e7490", "#d1d5db",
        "#4b5563", "#dc2626", "#16a34a", "#ca8a04", "#2563eb", "#9333ea", "#0891b2", "#f9fafb",
      ],
    },
  },
  {
    id: "zwinger-contrast",
    name: "High contrast",
    base: "dark",
    tokens: {
      "--motiv-color-primary": "#ffd60a",
      "--motiv-color-on-primary": "#000000",
      "--motiv-color-secondary": "#ffffff",
      "--motiv-color-bg": "#000000",
      "--motiv-color-surface": "#121212",
      "--motiv-color-text": "#ffffff",
      "--motiv-color-border": "#a3a3a3",
      "--motiv-color-danger": "#ff6b6b",
      "--motiv-color-success": "#4ade80",
      "--motiv-color-warning": "#fbbf24",
      "--motiv-radius": "0.25rem",
      "--motiv-font-size": "0.875rem",
    },
    terminal: { background: "#000000", foreground: "#ffffff", cursor: "#ffd60a", selection: "#404040", ansi: XTERM_ANSI },
  },
];

const CACHE = "zwinger.theme";
const DARK = "(prefers-color-scheme: dark)";

/** The theme on the window now. */
let active = BUILT_INS[0];

/** Sets the base as `data-theme` and the tokens as inline properties of `<html>`, in place of the earlier ones. */
export function applyTheme(theme: Theme) {
  const { style, dataset } = document.documentElement;
  for (const name of [...style]) if (name.startsWith("--motiv-") || name.startsWith("--zw-")) style.removeProperty(name);
  dataset.theme = theme.base;
  for (const [name, value] of Object.entries(theme.tokens)) style.setProperty(name, value);
  active = theme;
}

/** Before the first render: the theme of the last run, so the window does not flash. A bad cache gives the default. */
export function applyCachedTheme() {
  try {
    const cached = localStorage.getItem(CACHE);
    applyTheme(cached ? JSON.parse(cached) : BUILT_INS[0]);
  } catch {
    applyTheme(BUILT_INS[0]);
  }
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
