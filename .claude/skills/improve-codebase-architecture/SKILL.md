---
name: improve-codebase-architecture
description: Use to find deepening opportunities in one codebase area, report them as visual HTML, then grill the selected one.
disable-model-invocation: true
---

# Improve Codebase Architecture

Find **deepening opportunities**: refactors that make shallow modules deep (understood in one place, verified through the interface).

**Stay in one area per run**: a feature in `src/`, the `src/` ↔ `src-tauri/` IPC layer, or one Rust module.

Use `/codebase-design` vocabulary and principles exactly. `CONTEXT.md` has domain names; `docs/adr/` has closed decisions. `/domain-modeling` owns both.

Checks: `pnpm build` (`tsc` + Vite), `cargo check` / `cargo test` in `src-tauri/`, existing tests. Tauri command name, argument or return-type changes are contract changes on both sides; state this on the card.

## 1. Explore

Read `CONTEXT.md` and related ADRs. Examine it (sub-agents if large). Record friction:

- One concept spans many small modules.
- **Shallow** modules: interface almost as complex as the implementation.
- Helpers extracted for order, with bugs in the calls (no **locality**).
- Coupled modules leaking across seams: UI logic duplicated in Rust, or the reverse.

Apply the deletion test to each suspect.

## 2. Report

Write a self-contained HTML report to `${TMPDIR:-/tmp}/architecture-review-<timestamp>.html`. `open` it and give the absolute path. Follow [HTML-REPORT.md](HTML-REPORT.md): one card per candidate with a before/after diagram, then a **Top recommendation**.

State benefits in `/codebase-design` terms. Name modules in `CONTEXT.md` terms ("the Session resolution module", not "the SessionHandler service"). Flag an ADR conflict with an amber callout only if the friction justifies reopening it.

Propose interfaces only in step 3. Ask: "Which of these would you like to explore?"

## 3. Grill

Run `/grilling` on the selected candidate: constraints, dependencies, module shape, what the seam hides, which checks stay. Record decisions with `/domain-modeling`:

- New concept name or more precise term: update `CONTEXT.md`.
- Rejection for a reason reviewers need later: offer an ADR to prevent re-proposals. Skip temporary or obvious reasons.
- Alternative interfaces: use `/codebase-design` design-it-twice.
