# Deepening

Deepen shallow modules by dependency category. Vocabulary: [SKILL.md](SKILL.md).

## Dependency categories

The category sets verification across the seam.

1. **In-process**: pure computation, in-memory state, no I/O (reducers, formatters, parsers, plain Rust functions). Always deepen: merge, then verify through the new interface. No adapter.
2. **Framework-substitutable**: the framework has a stand-in (jsdom for browser APIs; Tauri `mockIPC` for `invoke` in the UI; `tauri::test` mock runtime or temp dirs in Rust). Deepen with an internal seam; add no port to reach it.
3. **Remote but owned (ports & adapters)**: your logic across a process or network (UI ↔ Rust IPC). Define a **port** at the seam. The deep module owns the logic. Production uses the real transport; tests use an in-memory adapter.
4. **True external (mock)**: services or OS facilities you do not control. Inject as a port; tests supply a fake.

## Seam rules

- Add a port only for two justified adapters, usually production and test. One adapter is indirection.
- Keep internal seams internal. Do not widen the interface for a check.

## Verify: replace, do not layer

Repo checks: `pnpm build` runs `tsc`; `cargo check` / `cargo test` in `src-tauri/`; existing tests.

- Verify at the new interface. Assert observable results (return values, rendered output, emitted events, persisted state), not internals.
- When modules merge, move checks from the removed seams to the new interface. Do not keep both layers.
- A check that changes with the implementation goes past the interface.
