# UI text scale audit (#125)

Scope: text sizes, line heights, spacing, and control and row heights in the app and site. All entries below are resolved on motiv 0.31.0. Layout widths, borders, and screen-reader clipping are outside this audit.

## App scale

The body uses `--motiv-font-size`. Secondary text uses `--motiv-font-size-sm`; headings use `--motiv-font-size-lg`. Rows use `--motiv-row-height`, controls use `--motiv-control-height`, and body text uses `--motiv-line-height`. Spacing uses `--motiv-space`. The app no longer defines duplicate text, row, or control height tokens.

| Location | Resolution | Status |
|---|---|---|
| `src/styles/app.css` body | `--motiv-font-size` | Resolved |
| `src/styles/app.css` dot width and height | `--motiv-space` | Resolved |
| `src/styles/app.css` page title | `--motiv-font-size-lg` | Resolved |
| `src/styles/app.css` facts label | `--motiv-font-size-sm` | Resolved |
| `src/features/projects/rail.css` blocked label | `--motiv-font-size-sm` | Resolved |
| `src/features/projects/rail.css` tree rows | `--motiv-font-size` and `--motiv-row-height` | Resolved |
| `src/features/projects/rail.css` path | `--motiv-font-size-sm` | Resolved |
| `src/features/projects/rail.css` state | `--motiv-font-size-sm` | Resolved |
| `src/features/projects/rail.css` rename field | `--motiv-row-height` | Resolved |
| `src/features/tasks/tasks.css` message line height | `--motiv-line-height` | Resolved |
| `src/features/tasks/tasks.css` tool output | `--motiv-font-size-sm` | Resolved |
| `src/features/tasks/tasks.css` room heading | `--motiv-font-size` | Resolved |
| `src/features/tasks/tasks.css` room chip | `--motiv-font-size-sm` | Resolved |
| `src/features/terminal/terminal.css` pane header height | `--motiv-space` | Resolved |
| `src/features/terminal/terminal.css` pane header text | `--motiv-font-size-sm` | Resolved |
| `src/features/appearance/appearance.css` theme swatch | `--motiv-space` | Resolved |
| `src/shared/Markdown.css` code | `--motiv-font-size-sm` | Resolved |
| `src/shared/Markdown.css` headings | `--motiv-font-size-lg` | Resolved |
| `src/features/projects/map.css` canvas text | `--motiv-font-size-sm` | Resolved |
| `src/features/projects/map.css` node height | `--motiv-row-height` | Resolved |
| `src/features/projects/map.css` activity dot | `--motiv-space` | Resolved |
| `src/features/projects/map.css` badge text | `--motiv-font-size-xs` | Resolved |
| `src/features/projects/map.css` badge line height | `--motiv-line-height` | Resolved |
| `src/features/projects/map.css` title | `--motiv-font-size` | Resolved |
| `src/features/projects/map.css` state and caption | `--motiv-font-size-sm` | Resolved |
| `src/features/projects/map.css` gateway | `--motiv-row-height` and `--motiv-space` | Resolved |
| `src/features/projects/map.css` legend | `--motiv-font-size-sm` | Resolved |
| `src/features/projects/map.css` swatch | `--motiv-space` | Resolved |
| `site/src/styles/theme.css` feature gap | `--motiv-space` | Resolved |

The project map reads the computed row height, spacing, and monospace character width to place rows and shorten labels. Existing dragged positions are kept; **Reset layout** restores the automatic positions if they overlap after a size change.

The rail rename field needs a scoped height rule because motiv's TextField still has a 3rem minimum height. The rule sets the field to the current row height so it fits a compact row.

## Layout settings

**Density** sets Default or Compact spacing across the app. **Sidebar font size** sets the rail font to Theme default or 12–15 px. Theme default follows the body size of the selected theme. The settings apply at once and persist in preferences.
