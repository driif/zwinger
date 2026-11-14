# UI text scale audit (#125)

Scope: each literal font-size, line-height, padding, gap and control or row height in `src/styles/`,
`src/features/**/*.css`, `src/shared/Markdown.css` and `site/src/**/*.css`, on motiv 0.30.1.
Line numbers are for the files after this change. A value that already was a `--motiv-*` or `--zw-*` token is not listed.

Out of scope: layout widths (`--zw-rail-width`, `max-width`, `minmax()` columns), 1px borders, the `.sr-only` clip
box and drag hit areas (`.rail-handle` 6px, `.pane-divider` 6px). They are not text or density sizes.

## App text scale

`src/styles/tokens.css` derives the app scale from motiv, with the factors that motiv's own CSS uses:

| token | value | at the 0.8125rem body |
|---|---|---|
| `--zw-text-sm` | `calc(var(--motiv-font-size) * 0.875)` | 11.4px |
| `--zw-text-body` | `var(--motiv-font-size)` | 13px |
| `--zw-text-lg` | `calc(var(--motiv-font-size) * 1.25)` | 16.25px |
| `--zw-rail-row` | `calc(var(--motiv-space) * 7)` (was 1.75rem) | 28px |
| `--zw-icon-button` | `calc(var(--motiv-space) * 6)` (was 1.5rem) | 24px |
| `--zw-icon` | `calc(var(--motiv-space) * 3.5)` (was 14px) | 14px |
| `--zw-control-height` | `calc(var(--motiv-space) * 8)` (was 2rem) | 32px |

At the default `--motiv-space` (0.25rem) the row, control and icon sizes do not change, so the dnd-kit hit areas and
the terminal pane math (`--zw-pane-header` in `PaneLayout.tsx`) keep their pixel values.

Body size: all built-in themes use `--motiv-font-size: 0.8125rem` (High contrast was 0.875rem).

The app's own text and all rail rows (workspace, session, group and item rows) use `--zw-text-body`. Secondary
labels (`.tree-path`, `.tree-state`, captions, chips) use `--zw-text-sm`. motiv components (buttons, fields, menus,
selects) keep their own fixed sizes until phase 2 (see "Missing in motiv"), so they do not yet match the body size.

## Literals

| file:line | value | replacement |
|---|---|---|
| src/styles/app.css:19 | font-size `var(--motiv-font-size)` (`body`) | `--zw-text-body` |
| src/styles/app.css:63 | width 0.5rem (`.dot`) | `calc(var(--motiv-space) * 2)` |
| src/styles/app.css:64 | height 0.5rem (`.dot`) | `calc(var(--motiv-space) * 2)` |
| src/styles/app.css:210 | font-size 1.125rem (`.page-title`) | `--zw-text-lg` |
| src/styles/app.css:261 | font-size 0.75rem (`.facts dt`) | `--zw-text-sm` |
| src/features/projects/rail.css:41 | font-size 0.75rem (`.rail-blocked`) | `--zw-text-sm` |
| src/features/projects/rail.css:91 | font-size 0.75rem (`.tree-row.level-1` workspace rows and `.level-3` group rows) | `--zw-text-body` on `.tree-row`: all rail rows use the body size |
| src/features/projects/rail.css:146 | font-size 0.75rem (`.tree-path`) | `--zw-text-sm` |
| src/features/projects/rail.css:169 | font-size 0.75rem (`.tree-state`) | `--zw-text-sm` |
| src/features/projects/rail.css:189 | height `calc(var(--zw-rail-row) - 4px)` (`.tree-edit`) | `calc(var(--zw-rail-row) - var(--motiv-space))` on `.motiv-float__control`; see `--motiv-control-height` |
| src/features/tasks/tasks.css:6 | line-height 1.5 (`.msg`) | missing: `--motiv-line-height` |
| src/features/tasks/tasks.css:21 | font-size 0.75rem (`.tool-output`) | `--zw-text-sm` |
| src/features/tasks/tasks.css:48 | font-size 0.875rem (`.room-heading`) | `--zw-text-body` |
| src/features/tasks/tasks.css:74 | font-size 0.75rem (`.room-chip`) | `--zw-text-sm` |
| src/features/terminal/terminal.css:13 | `--zw-pane-header` 24px | `calc(var(--motiv-space) * 6)` |
| src/features/terminal/terminal.css:80 | font-size 0.75rem (`.pane-header`) | `--zw-text-sm` |
| src/features/appearance/appearance.css:21 | height 1.25rem (`.theme-swatch > span`) | `calc(var(--motiv-space) * 5)` |
| src/shared/Markdown.css:21 | font-size 0.75rem (`.md code`) | `--zw-text-sm` |
| src/shared/Markdown.css:38 | font-size 1rem (`.md-heading.h1/.h2`) | `--zw-text-lg` |
| src/features/projects/map.css:26 | font-size 12px (`.react-flow`) | missing: Map canvas |
| src/features/projects/map.css:45 | height 28px (`.map-node`) | missing: Map canvas |
| src/features/projects/map.css:117 | height 6px (`.map-activity`) | missing: Map canvas |
| src/features/projects/map.css:126 | font-size 10px (`.map-badge`) | missing: Map canvas |
| src/features/projects/map.css:127 | line-height 16px (`.map-badge`) | missing: Map canvas |
| src/features/projects/map.css:174 | font-size 14px (`.map-title`) | missing: Map canvas |
| src/features/projects/map.css:181 | font-size 11px (`.map-state`, `.map-caption`) | missing: Map canvas |
| src/features/projects/map.css:189 | height 22px (`.map-gateway`) | missing: Map canvas |
| src/features/projects/map.css:209 | font-size 11px (`.map-legend`) | missing: Map canvas |
| src/features/projects/map.css:218 | height 14px (`.map-swatch`) | missing: Map canvas |
| site/src/styles/theme.css:27 | line-height `var(--sl-line-height)` (body) | keep: Starlight token, not a literal |
| site/src/styles/theme.css:39 | gap 1rem (`.features`) | `calc(var(--motiv-space) * 4)` |

`src/features/onboarding/onboarding.css`, `src/features/launcher/launcher.css` and `src/shared/actionMenu.css` have no
literal of these kinds.

### Map canvas

The project map places its nodes in pixels: `mapLayout.ts` (`H = 28`, `ROW = 36`, the gateway at `top: 70px`) and
`CHAR = 7.2` in `ProjectMap.tsx` (the width of one 12px mono character). Its font sizes and heights must stay in step
with these numbers, so they stay pixel literals. A scalable map needs the layout to read the sizes from the tokens
(backlog), not a motiv token.

## Missing in motiv

motiv 0.30.1 has one text token (`--motiv-font-size`) and one spacing token (`--motiv-space`). Proposed tokens:

| token | proposed value | use |
|---|---|---|
| `--motiv-font-size-xs` | `calc(var(--motiv-font-size) * 0.75)` | captions, badges |
| `--motiv-font-size-sm` | `calc(var(--motiv-font-size) * 0.875)` | labels, secondary text (replaces `--zw-text-sm`) |
| `--motiv-font-size-lg` | `calc(var(--motiv-font-size) * 1.25)` | section and page headings (replaces `--zw-text-lg`) |
| `--motiv-font-size-xl` | `calc(var(--motiv-font-size) * 1.5)` | page titles |
| `--motiv-line-height` | `1.5` | body text (motiv base sets `body { line-height: 1.5 }` as a literal) |
| `--motiv-control-height` | `calc(var(--motiv-space) * 8)` | buttons, fields, select and command menu launchers; a compact TextField (replaces `--zw-control-height` and the `.tree-edit` override) |
| `--motiv-row-height` | `calc(var(--motiv-space) * 7)` | list, tree and menu rows (replaces `--zw-rail-row`) |
| `[data-density=compact]` | sets `--motiv-space: 0.1875rem`, `--motiv-control-height: calc(var(--motiv-space) * 7)`, `--motiv-row-height: calc(var(--motiv-space) * 6)` | the Default / Compact density |

motiv components use hard-coded rem font sizes (`.875rem` 23 times, `.8125rem` 8 times, `.75rem` 8 times, and more in
`styles.css`), and fixed pixel heights (for example `.motiv-float__control { min-height: 58px }`). A scoped
`--motiv-font-size` (for example on the rail) therefore does not scale them. These sizes must move to the tokens above.

### Settings that wait for motiv

- **Density: Default / Compact.** Needs `[data-density=compact]` and the control and row height tokens. Not added.
- **Sidebar font size.** Needs the motiv components to read `--motiv-font-size` (see above), so that a value scoped to
  the rail scales its controls. Not added.

`preferences.rs` and `appearance.rs` do not change.
