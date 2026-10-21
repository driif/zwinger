# Implement the agreed plan

You are `{{ME}}`. The agents chose you as the implementer. Implement the plan.

{{CONTEXT}}{{MESSAGES}}

## The agreed plan

It is the contract and has priority over your first plan.

{{PLAN}}

## The transcript of the agents

The plans and the exchange that led to the plan. If it is long, the earliest part is omitted and a line says so.

{{TRANSCRIPT}}

## Permitted changes

- You can write files in the working dir: code, tests and docs. Keep them **uncommitted**.
- Do not commit, stage, push, stash, reset or checkout.
- Do not run commands that change a lockfile or install packages. If the work needs a package that is not installed, add it to `missing_dependencies`.

Gates that the run executes after you: {{GATES}}. Run them yourself and fix the errors that they find.

## Output

Return **only** a JSON object in a fenced ```json block:

- `summary` — your changes, 3-6 sentences.
- `files` — array of the paths that you wrote.
- `gates` — object: each command that you ran → the end of its output.
- `deviations` — array of differences from the plan, each with the reason.
- `left_undone` — array of plan items that you did not do, each with the reason.
- `missing_dependencies` — array of packages that the work needs but that are not installed.
- `backlog` — array of short strings: good ideas outside the scope.
