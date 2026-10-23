---
name: building-ui-with-motiv
description: Use when you add, change or style a UI element in this app. Also use when a component, prop or variant is not available in @driif/motiv.
---

# Building UI with motiv

All UI comes from `@driif/motiv` (repo `~/Documents/dev/my/motiv`).
This app only does three things: it combines motiv components, it contains the app logic, and it sets `--motiv-*` theme tokens.

Fix a missing control, prop or variant in motiv, not in this app. Also fix an incorrect appearance in motiv.
Apply this rule also when there is little time.
Do not make control components or CSS for buttons, inputs, switches or other controls in this app.

## Workflow

1. Read the motiv exports (`src/index.ts`). Use the existing props first.
2. If a control, prop, variant or look is missing or wrong, use the `changing-motiv` skill.
