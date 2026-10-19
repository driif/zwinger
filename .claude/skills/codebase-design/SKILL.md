---
name: codebase-design
description: Use to design or improve a module interface, find deepening opportunities, place a seam, or make code verifiable or AI-navigable. Also use when a skill needs the deep-module vocabulary.
---

# Codebase Design

Design **deep modules**: much behavior behind a small interface at a clean seam, verified through it.

## Glossary (use exactly)

- **Module**: anything with an interface and an implementation: function, hook, component, Rust module, Tauri command. _Avoid_: unit, component, service.
- **Interface**: all a caller must know: types, invariants, ordering, error modes, configuration, performance. Not only a TS `interface`, props or Rust `trait`. _Avoid_: API, signature.
- **Implementation**: the code inside. (**Adapter** is a seam role.)
- **Depth**: behavior a caller or check can exercise per unit of interface learned. Not a line ratio. **Shallow**: interface almost as complex as the implementation.
- **Seam** (Feathers): where behavior changes without in-place edits; the interface is there. Placement is a separate decision. _Avoid_: boundary (DDD).
- **Adapter**: a concrete thing satisfying an interface at a seam.
- **Leverage**: caller benefit: one implementation serves N call sites and M checks.
- **Locality**: maintainer benefit: changes, bugs and checks stay in one place.

## Principles

- **Depth belongs to the interface.** A deep module can have small parts and unexposed **internal seams**.
- **Deletion test.** Imagine the module deleted. Complexity disappears: pass-through. Complexity reappears across callers: useful.
- **The interface is the verification surface.** Callers and checks cross the same seam. Verification past it means a wrong module shape.
- **One adapter = hypothetical seam; two = real.** A seam costs until a second adapter justifies it. If one is only possible, show options and ask; do not add indirection.

## Verifiability

1. **Accept dependencies; do not make them.** `sortRows(rows, sort)`, not one that reads `localStorage`.
2. **Return results, not side effects.** `resolveTheme(pref, systemDark): "light" | "dark"`, not a DOM-changing `void`.
3. **Small surface.** Fewer entry points and parameters mean fewer test paths.

## More

- Deepen and verify a cluster: [DEEPENING.md](DEEPENING.md).
- Parallel alternative interfaces: [DESIGN-IT-TWICE.md](DESIGN-IT-TWICE.md).
