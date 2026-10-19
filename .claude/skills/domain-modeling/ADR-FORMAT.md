# ADR Format

ADRs are in `docs/adr/`, numbered sequentially (`0001-slug.md`). Use the highest number plus one. Make the directory with the first ADR.

## Template

```md
# {Short title of the decision}

{1-3 sentences: the context, what we decided, and why.}
```

One paragraph records *that* a decision occurred and *why*. Add these only when necessary:

- **Status** frontmatter (`proposed | accepted | deprecated | superseded by ADR-NNNN`): decisions get changed later.
- **Considered Options**: rejected alternatives are important.
- **Consequences**: downstream effects are not obvious.

## When to offer one

All three must be true:

1. **Hard to reverse.** A later change is expensive.
2. **Surprising without context.** A future reader asks "why this way?"
3. **A real trade-off.** Real alternatives existed; one was selected for specific reasons.

## What qualifies

- **Architectural shape.** Example: "State lives in Rust; the UI is a thin view over Tauri commands."
- **Integration patterns.** Example: "Core pushes updates via events, not UI polling."
- **Technology with lock-in.** Database, storage format, auth, updater, plugins that are slow to replace.
- **Boundary and scope.** Which part owns which data, including exclusions.
- **Intentional deviations from the usual path.** These prevent "fixes" of intentional behavior.
- **Constraints the code does not show.** Compliance, platform, performance contracts.
- **Rejected alternatives that are not obvious.** These prevent repeated proposals.
