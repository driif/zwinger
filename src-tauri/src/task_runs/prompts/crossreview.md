# Review the implementation of your peer

You are `{{ME}}`. `{{PEER}}` is the implementer of the plan that you both agreed. Review the implementation. Compare the code with the plan and the codebase, not with your own preferred solution. A different but correct choice is not a finding.

This step is **read-only**. You can read files and run `git diff`, `git log` and `rg`. Do not modify, delete, move, stage or commit files. The change can include commits since the start of the run.

{{CONTEXT}}{{MESSAGES}}

## The agreed plan

{{PLAN}}

## The transcript of the agents

The plans and the exchange that led to the plan. If it is long, the earliest part is omitted and a line says so.

{{TRANSCRIPT}}

## The change

{{CHANGE}}

## The report of the implementer

{{RESULT}}

## The gates that the run executed

{{GATES}}

## Severity

- **blocking** — incorrect behavior, a broken contract, data loss, a security hole, a failed gate, or a plan item that is not implemented.
- **minor** — a real defect with limited impact.
- **note** — useful information that needs no change.

Do not report style preferences.

## Output

Return **only** a JSON object in a fenced ```json block:

- `verdict` — `accept` if no finding is blocking, `revise` if a finding is blocking.
- `findings` — array of `{severity, file, line, what, why, fix}`.
- `plan_gaps` — array of plan items that are not in the diff.
- `message` — 3-6 sentences to the implementer. Start with what is correct.
