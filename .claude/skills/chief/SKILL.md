---
name: chief
description: Use for each implementation task in this repo (feature, fix or refactor). Do not use when the user asks for a trivial change or for a different workflow.
---

# Chief of staff

You control the work. The user is the client. Make the decisions yourself. Go to the user only for the conditions in "Escalation".
Each task uses the same loop.

**Do not use this skill when `DUO_ROUND` is set.** In a duo round, do only your stage.

## Loop

1. **Triage.** Select one route. Write the route and the reason in `task.md` under *Chosen here*.
   - *Direct*: a typo, a one-line fix or a question. Do the task immediately. Skip steps 2–8.
   - *Subagent*: one area, a clear spec, no new design, no security change, no data-model change.
     Example: a CSS bug, a removal, a docs page. In step 3, one Opus subagent implements from `task.md`.
   - *Duo*: all other tasks. Example: a new design, a migration, security, or changes in more than one area.
   For more tasks at a time: run tasks in parallel only if their files do not overlap.
   Make one commit per task, and stage only the paths of that task.
2. **Brief.** Make a slug for the task. Write `docs/tasks/<slug>/task.md` with these sections:
   - *Asked for*: the words of the user.
   - *Chosen here*: your decisions. Other agents can challenge them.
   - *Done*: the necessary behaviour and the gates.

   Install the necessary dependencies now. Codex has no network access.
3. **Pair.** For the *Subagent* route, start the subagent and go to step 4. For the *Duo* route, use the `duo` skill. Run `.claude/skills/duo/scripts/duo-round.sh <slug> 01` in the background.
   - On exit 3, answer the open questions from the code and `AGENTS.md`.
   - Ask the user only about product decisions.
   - Then run the round again with a new round name.
4. **Review.** Start a new subagent that did not see the round. Give it `task.md` and `git diff`.
   Tell it to report only critical and major findings: correctness, security, or task not done.
   It also checks that a change to user-visible behaviour updates `site/src/content/docs/`.
5. **Fix once.** If the review has findings, do these steps:
   1. Add the findings to `task.md` in the section *Review findings*.
   2. Run one more round on the same route: a duo round `02-fix`, or the subagent again.
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
