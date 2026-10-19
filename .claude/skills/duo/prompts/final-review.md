# Final review of the full change

You are `{{ME}}`. The plan, the implementation, the review and the revision are complete. Read the **full** change as one unit.

The earlier reviews found local defects in a diff. This review asks: does the finished change do what the task asks, and is it consistent? A change can pass all local checks but move away from the task. A change can also have a seam where the revision meets the original code.

`{{PEER}}` reviews the same change independently and does not see your answer. Your agreement is the strongest signal to the user only because neither model sees the other's review.

This step is **read-only**.

- You can read files and run `git diff`, `git log`, `rg` and the gates.
- Do not modify, delete, move, stage or commit files.

## Context

- `{{TASK}}` — the request. Compare the change with this, not with the plan.
- `{{PLAN}}` — the agreed plan.
- `{{HISTORY}}` — the review and the revision.
- `{{RULES}}` — the project conventions.
- `{{WORKDIR}}` — run all commands from this directory.

Gates: `pnpm build`. For changes in `src-tauri/`, also
`cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` and
`cargo test --manifest-path src-tauri/Cargo.toml`.

## Find these defects

- A part of the task that the change does not do.
- A revision that contradicts the plan, or code that mixes two approaches.
- A seam: two methods for the same function now exist together.
- A gate that fails.
- A change that does more than the task asks. This is a defect, the same as a change that does less.

Do not report findings that `{{HISTORY}}` shows as fixed. Do not report style preferences.

## Output

Return **only** a JSON object in a fenced ```json block:

- `ship` — `true` if you accept this change as complete.
- `blocking` — array of `{what, why, where}`. If this array has an entry, `ship` is `false`.
- `residual` — array of real, non-blocking items for the user to know.
- `task_met` — one or two sentences: does the change do what `{{TASK}}` asks? Name each part that it does not do.

## Runner limit

Run all commands in the FOREGROUND and wait for them to complete. Do not use `run_in_background`. Do not start background agents. This session ends when you stop, and the background work is lost.
