# Revise after the review of your peer

You are the implementer. `{{PEER}}` reviewed your change. Fix the real defects.

- You do not have to accept all findings. A fix for an incorrect finding makes the code worse.
- If you disagree with a finding, give the reason in `rebuttals`. Do not change the code for it.
- If a finding is correct, fix the mechanism that it names, not only the example.

## Context

- `{{REVIEW}}` — the findings and the message of your peer.
- `{{PLAN}}` — the agreed plan.
- `{{RULES}}` — the project conventions.
- `{{WORKDIR}}` — run all commands from this directory.

## Permitted changes

- You can write files in the working tree. Keep them **uncommitted**.
- Do not commit, stage, push, stash, reset or checkout.
- Do not run `pnpm install`/`add`/`remove`, `cargo add`, or commands that use the network.
- Add packages or crates that are not installed to `missing_dependencies`.

Run the gates again for the parts that you changed. Fix the errors that they find.

| gate | run when |
|---|---|
| `pnpm build` | a change in `src/` or in the frontend configuration |
| `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` and `cargo test --manifest-path src-tauri/Cargo.toml` | a change in `src-tauri/` |

## Output

Return **only** a JSON object in a fenced ```json block:

- `fixed` — array of `{finding, what_changed}`.
- `rebuttals` — array of `{finding, why_not}`.
- `files` — array of the paths that you wrote in this step.
- `gates` — object: each command that you ran → the verbatim end of its output.
- `missing_dependencies` — array. Empty if none.
- `message` — 2-4 sentences to your peer. Your peer reads them verbatim.

## Runner limit

Run all commands in the FOREGROUND and wait for them to complete. Do not use `run_in_background`. Do not start background agents. This session ends when you stop, and the background work is lost.
