---
name: chief
description: Use for each implementation task in this repo (feature, fix or refactor). Do not use when the user asks for a trivial change or for a different workflow.
---

# Chief of staff

You control the work. The user is the client. Make the decisions yourself. Go to the user only for the conditions in "Escalation".
Each task uses the same loop.

**Do not use this skill when `DUO_ROUND` is set.** In a duo round, do only your stage.

## Loop

1. **Triage.** For a typo, a one-line fix or a question, do the task immediately. For all other tasks, do steps 2–8.
2. **Brief.** Make a slug for the task. Write `docs/tasks/<slug>/task.md` with these sections:
   - *Asked for*: the words of the user.
   - *Chosen here*: your decisions. Other agents can challenge them.
   - *Done*: the necessary behaviour and the gates.

   Install the necessary dependencies now. Codex has no network access.
3. **Pair.** Use the `duo` skill. Run `.claude/skills/duo/scripts/duo-round.sh <slug> 01` in the background.
   - On exit 3, answer the open questions from the code and `AGENTS.md`.
   - Ask the user only about product decisions.
   - Then run the round again with a new round name.
4. **Review.** Start a new subagent that did not see the round. Give it `task.md` and `git diff`.
   Tell it to report only critical and major findings: correctness, security, or task not done.
5. **Fix once.** If the review has findings, do these steps:
   1. Add the findings to `task.md` in the section *Review findings*.
   2. Run one more duo round (`02-fix`).
   3. Stop the loop. Do not do more fix rounds.
6. **Ship.** Run the gates yourself. If all gates pass, do these steps:
   1. Commit with a Conventional Commit message.
   2. Push to the **current branch**.

   Never create branches.
7. **Backlog.** Collect backlog items from these sources:
   - the `backlog` field in `manifest.json`
   - review findings that are out of scope
   - your own ideas

   Do not file duplicates. Find existing backlog issues with `gh issue list --label backlog`.
   Make one backlog issue for each item, with a body of 2–4 lines:
   `gh issue create --label backlog --title … --body …`
8. **Report.** Write a short report with these items:
   - what you shipped
   - the commit
   - the implementer
   - the review result
   - the backlog issues that you filed

## Escalation

Go to the user only in these conditions:

- An open question needs a product decision, and the code does not give the answer.
- The gates fail after the fix round.
- The duo tool fails two times (exit 2).
- The next step is destructive, or the next step does more than a push to the current branch.
