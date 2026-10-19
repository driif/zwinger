# Write a plan

You and one other model must agree on a plan for a task and on one implementer. This is your **first plan**. You do not see the plan of the other model. You discuss the plans in the next step.

- Write your real opinion, not the plan that you think will win.
- Do not weaken your plan because of a possible counter-argument. This is the only failure in this stage.

**Propose the best solution, not the most obvious solution.**

- Find a good solution: a clearer model, a simpler mechanism, or a better experience for the app user.
- Choose it if it is better than your first idea.
- Stay in the scope of the task: do the request better, not do more than the request.
- Put good ideas outside the scope in `backlog`, not in the plan.

This step is **read-only**.

- You can read files and run `git diff`, `git log`, `rg`, `ls`.
- Do not modify, delete, move, stage or commit files.
- Do not write through a shell redirect.
- Do not run builds, tests, installers or linters.

## Context (paths — read them first)

- `{{TASK}}` — the request.
- `{{RULES}}` — the project conventions. They have priority over your preferences.
- `{{WORKDIR}}` — the repository root (Tauri 2: React + Vite in `src/`, Rust in `src-tauri/`). Run all commands from this directory.

## Output

Be specific: name real files. A plan that you can write without reading the repository has no value.

Return **only** a JSON object in a fenced ```json block:

- `plan` — your solution, 4-8 sentences, with the names of the files that you change.
- `touches` — array of the areas that the plan changes, e.g. `ui`, `state`, `tauri-command`, `rust`, `config`, `build`.
- `risks` — array of possible problems. An empty array means that you found no risks.
- `claim` — `opus` or `sol`: the implementer that you recommend. You can recommend the other model.
- `claim_reason` — one or two sentences. Give a reason from the task, not from general capability.
- `unknowns` — array of questions that only the user can answer.
  - Do not include a question that one more file read can answer.
  - Include a dependency that is not installed. Nobody can install packages during the round.
  - Each entry stops the round. List only questions that block the work.
- `backlog` — array of short strings: good ideas outside the scope of this task. An empty array is permitted.
