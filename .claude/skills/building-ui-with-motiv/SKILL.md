---
name: building-ui-with-motiv
description: Use when adding, changing or styling any UI element in this app, or when a component, prop or variant seems missing from @driif/motiv.
---

# Building UI with motiv

UI comes from `@driif/motiv` (repo `~/Documents/dev/my/motiv`). This app only composes motiv
components, holds app logic and sets `--motiv-*` theme tokens.

A missing control, prop or variant, or a wrong look, is fixed in motiv, not here — even under
time pressure. No app-side control components or CSS for buttons, inputs, switches, etc.

## Workflow

1. Check motiv's exports (`src/index.ts`); use existing props first.
2. Missing → follow motiv's `AGENTS.md`. New public API needs the user's decision first.
3. Try locally with `pnpm add @driif/motiv@link:../motiv`; never commit the `link:`.
4. After release, `pnpm up @driif/motiv@latest`.
