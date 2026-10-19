---
name: resolving-merge-conflicts
description: Use when a git merge or rebase stops with conflict markers, or when the user asks to resolve conflicts. Resolve each hunk and keep the intent of the two sides.
---

Resolve the conflict. Never use `git merge --abort` or `git rebase --abort` to avoid the conflict.

1. **Find the state.** Find the operation (merge or rebase), the source and target branches, and the conflicting files.
   Read the hunks.
2. **Find the reason for each change.** Read the commit messages and the related PR or issue.
   Record contract changes: Tauri command names and arguments, events, and types shared between `src/` and `src-tauri/`.
   After the resolution, the two sides must agree on these contracts.
3. **Resolve each hunk.**
   - If the two intents can work together, keep the two intents.
   - If the two intents are incompatible, keep the side that agrees with the goal of the merge. Record the trade-off.
   - Do not add new behaviour.
4. **Verify.** Run `pnpm build`. In `src-tauri/`, run `cargo check` and `cargo test`. Fix the errors that the merge caused.
   - Generated output (`dist/`, `src-tauri/target/`, `src-tauri/gen/`): do not merge it manually. Keep one side, then generate the output again.
   - `package.json` and `Cargo.toml`: resolve these files.
   - `pnpm-lock.yaml` (`pnpm install`) and `Cargo.lock`: if generating them again needs network access, let the user do it.
5. **Complete.** Stage the resolved files.
   Run `git merge --continue` or `git rebase --continue`. Repeat until git applies all commits.
