import type { Theme } from "./api.ts";

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
      "--motiv-font-size": "0.8125rem",
    },
    terminal: { background: "#000000", foreground: "#ffffff", cursor: "#ffd60a", selection: "#404040", ansi: XTERM_ANSI },
  },
];
