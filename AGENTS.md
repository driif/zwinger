# zwinger

- Do all work with the `chief` skill. Claude and Codex work as a pair in `duo`.
  Then a new agent does a review. Then the work is committed and pushed to the current branch.
- Go to the user only when you cannot continue. In a duo round, do only your stage.
- Ideas that are out of scope become backlog issues: GitHub issues with the label `backlog`. Do not build them.
- A change to user-visible behaviour updates the matching page in `site/src/content/docs/` in the same change.
- All UI comes from `@driif/motiv`. Load `building-ui-with-motiv` before you do UI work.
- Write responses to humans and agents in English, per ASD-STE100 (Simplified Technical English).
  Be short and direct. Give the conclusion first. Do not add filler.
- Never give credit to Claude, Anthropic, Codex or OpenAI in commits, PRs or issues.
- Skills are in `.claude/skills/`. Each skill has a Codex entrypoint in `.agents/skills/`.
  Create the two files locally. Never create them globally.

## Codex

In shared skills, these terms have these meanings for Codex:

- `Skill` or `/name`: the related skill in `.agents/skills/`.
- `Agent`: a Codex subagent. If no subagent is available, do the work in sequence.
- `AskUserQuestion`: ask the user directly.
