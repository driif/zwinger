# Revise after the review of your peer

You are `{{ME}}`, the implementer. `{{PEER}}` reviewed your change. Fix the real defects.

- You do not have to accept all findings. If you disagree with a finding, give the reason in `rebuttals` and do not change the code for it.
- If a finding is correct, fix the mechanism that it names, not only the example.

{{CONTEXT}}{{MESSAGES}}

## The agreed plan

{{PLAN}}

## The transcript of the agents

The plans and the exchange that led to the plan. If it is long, the earliest part is omitted and a line says so.

{{TRANSCRIPT}}

## The review

{{REVIEW}}

## Permitted changes

You can write files in the working dir. Keep them **uncommitted**. Do not commit, stage, push, stash, reset or checkout. Do not install packages. Gates: {{GATES}}.

## Output

Return **only** a JSON object in a fenced ```json block:

- `fixed` — array of `{finding, what_changed}`.
- `rebuttals` — array of `{finding, why_not}`.
- `files` — array of the paths that you wrote in this step.
- `gates` — object: each command that you ran → the end of its output.
- `missing_dependencies` — array.
- `message` — 2-4 sentences to your peer.
