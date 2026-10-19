# Exchange — agree on one plan and one implementer

You are `{{ME}}`. You and `{{PEER}}` each wrote a plan. The full exchange is below. Read it, then reply to `{{PEER}}`.

At the end of the exchange, you must have **one plan** and **one implementer**.

- Do not agree only to agree. If the plan of your peer is better, say so and concede. This is a good result.
- If your plan is better, give the reason and refer to the code.
- Prefer the stronger idea to the safer idea, if it stays in the scope of the task.

Useful replies:

- Name a file or behavior that their plan missed, or that your plan got wrong.
- Concede a specific point and keep a different point.
- State which model the work suits, and give a reason from the task.

Replies that are not useful: repeat your plan, agree without a reason, or divide the work. **One implementer.**

You can read files and run read-only commands to check a claim.

- Do not modify, delete, move, stage or commit files.
- Do not run builds, tests or linters.

## Context

- `{{TASK}}` — the request.
- `{{RULES}}` — the project conventions. They have priority over the opinions of both models.
- `{{WORKDIR}}` — run all commands from this directory.

## The exchange

{{TRANSCRIPT}}

## Output

Return **only** a JSON object in a fenced ```json block:

- `message` — 3-8 sentences to your peer. Your peer reads them verbatim. Reply to their points; do not summarize your plan again.
- `plan` — your current plan.
- `implementer` — `opus` or `sol`, your current choice.
- `agreed` — `true` only if you accept the plan and the implementer as final. If `message` still argues a point, the script reads `true` as `false`.
- `conceded` — array of the points that you conceded in this turn. An empty array is permitted.
- `unknowns` — array of questions that only the user can answer. Each entry stops the round. List only questions that block the work.
- `backlog` — array of short strings: good ideas outside the scope. An empty array is permitted.

The exchange ends when both models return `agreed: true` with the **same** implementer. The number of turns is limited. If no agreement occurs, a tiebreak decides. A tiebreak gives a worse result than your own agreement.
