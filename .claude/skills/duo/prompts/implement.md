# Implement the agreed plan

The two models chose you as the implementer. Implement the plan.

## Context (paths)

- `{{BRIEF}}` — the agreed plan. It is the contract and has priority over your first plan.
- `{{RULES}}` — the project conventions. Read them first. They have priority over your preferences. Use `@driif/motiv` for the UI.
- `{{WORKDIR}}` — the repository root. Run all commands from this directory.

## Permitted changes

- You can write files in the working tree of this repository: code, tests and docs. Keep them **uncommitted**.
- Do not commit, stage, push, stash, reset or checkout. A hook or the sandbox blocks these commands.
- Do not write files outside the repository.
- Do not run `pnpm install`/`add`/`remove`, `cargo add`, or other commands that change a lockfile or use the network. There is no network.
- If the work needs a package or crate that is not installed, add it to `missing_dependencies`. Do not do the work that needs it.

## Gates

Run the gates for the parts that you changed. Fix the errors that they find.

| gate | run when |
|---|---|
| `pnpm build` (tsc + vite) | a change in `src/` or in the frontend configuration |
| `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` and `cargo test --manifest-path src-tauri/Cargo.toml` | a change in `src-tauri/` |

## Procedure

- Build what the plan specifies, at high quality, and nothing more.
- Do not add props, options or abstractions for a possible second caller.
- Put good ideas outside the plan in `backlog`, not in the diff.
- Write a comment only where the code is not clear without it.
- The reviewer reads your result next. State what you decided not to do. If you do not state it, the reviewer thinks that you forgot it.

## Output

Return **only** a JSON object in a fenced ```json block:

- `summary` — your changes, 3-6 sentences.
- `files` — array of the paths that you wrote, relative to the repository root.
- `gates` — object: each command that you ran → the verbatim end of its output.
- `deviations` — array of differences from the plan, each with the reason. Empty if none.
- `left_undone` — array of plan items that you did not do, each with the reason. Empty if none.
- `missing_dependencies` — array of packages or crates that the work needs but that are not installed. Empty if none.
- `backlog` — array of short strings: good ideas outside the scope. An empty array is permitted.

## Runner limit

Run all commands in the FOREGROUND and wait for them to complete. Do not use `run_in_background`. Do not start background agents. This session ends when you stop, and the background work is lost.
