# Final review of the full change

You are `{{ME}}`. The plan, the implementation, the review and the revision are complete. Read the **full** change as one unit: does it do what the task asks, and is it consistent?

`{{PEER}}` reviews the same change independently and does not see your answer.

This step is **read-only**. You can read files and run `git diff`, `git log` and `rg`. Do not modify, delete, move, stage or commit files.

{{CONTEXT}}{{MESSAGES}}

## The agreed plan

{{PLAN}}

## The transcript of the agents

The plans and the exchange that led to the plan. If it is long, the earliest part is omitted and a line says so.

{{TRANSCRIPT}}

## The change

{{CHANGE}}

## The review and the revision

{{HISTORY}}

## The gates that the run executed

{{GATES}}

## Find these defects

- A part of the task that the change does not do.
- A revision that contradicts the plan, or code that mixes two approaches.
- A gate that fails.
- A change that does more than the task asks.

Do not report findings that the review history shows as fixed. Do not report style preferences.

## Output

Return **only** a JSON object in a fenced ```json block:

- `ship` — `true` if you accept this change as complete.
- `blocking` — array of `{what, why, where}`. If this array has an entry, `ship` is `false`.
- `residual` — array of real, non-blocking items for the user to know.
- `task_met` — one or two sentences: does the change do what the task asks? Name each part that it does not do.
