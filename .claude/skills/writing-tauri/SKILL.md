---
name: writing-tauri
description: Use when you write or change Tauri 2 code. This includes commands, IPC between React and Rust, app state, events, channels, capabilities, permissions, plugins and tauri.conf.json.
---

# Writing Tauri 2

Use only Tauri **2**. Do not use v1 APIs (`allowlist`, `@tauri-apps/api/tauri`).
For the Rust code, also use `rust-best-practices`.

## Commands
- Put each `#[tauri::command]` in a module for its domain.
- Register all commands in one `generate_handler![...]`.
- For errors, use a `thiserror` enum that implements `serde::Serialize`.
  Serialize it as its message or as a `{ kind, message }` object.
- Do not use `Result<T, String>`. Do not use `unwrap()` in commands.
- Async commands take owned arguments (`String`, not `&str`) and return `Result`.
- Use `serde` types for arguments and return values.
- Write the same types in TypeScript in one file, `src/api.ts`.
- In `src/api.ts`, wrap each `invoke` (`import { invoke } from '@tauri-apps/api/core'`).
- Keep commands small: parse the input, call plain Rust functions, then map the errors.
  You can then test the logic without Tauri.

## State
- Call `app.manage(T)` one time in `setup`. Read the state with `State<'_, T>`.
- Use `std::sync::Mutex` for short critical sections that are not async.
- Use `tokio::sync::Mutex` only when you hold the lock across `.await`.
- Never hold a `std::sync::Mutex` lock across `.await`.

## Rust to frontend
- Use `ipc::Channel<T>` to stream the progress or results of a command.
- Use events (`emit`/`listen`) for notifications to the full app.
- Stop each listener (unlisten) when its component unmounts.

## Security
- Put capabilities in `src-tauri/capabilities/*.json`. Give the minimum privilege, with a scope for each window.
- Add only the plugin permissions that you use.
  For example, use `fs:allow-read-text-file` with a scope, not all of `fs:default`.
- Never make a scope wider only to remove an error.
- All data from the webview is untrusted input. Validate it in the command.

## Checks
- `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings`
- `cargo test --manifest-path src-tauri/Cargo.toml`
- `pnpm build`
