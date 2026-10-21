# Exchange — agree on one plan and one implementer

You are `{{ME}}`. You and `{{PEER}}` each wrote a plan. The full exchange is below. Read it, then reply to `{{PEER}}`.

At the end of the exchange, you must have **one plan** and **one implementer**.

- Do not agree only to agree. If the plan of your peer is better, say so and concede.
- If your plan is better, give the reason and refer to the code.
- Do not divide the work. **One implementer.**

You can read files and run read-only commands to check a claim. Do not modify, delete, move, stage or commit files. Do not run builds, tests or linters.

{{CONTEXT}}{{MESSAGES}}

## The exchange

{{TRANSCRIPT}}

## Output

Return **only** a JSON object in a fenced ```json block:

- `message` — 3-8 sentences to your peer. Reply to their points; do not summarize your plan again.
- `plan` — your current plan.
- `implementer` — {{AGENTS}}, your current choice.
- `agreed` — `true` only if you accept the plan and the implementer as final.
- `conceded` — array of the points that you conceded in this turn.
- `unknowns` — array of questions that only the user can answer. Each entry stops the run until the user answers.
- `backlog` — array of short strings: good ideas outside the scope.

The exchange ends when both agents return `agreed: true` with the **same** implementer. If no agreement occurs, a tiebreak decides.
