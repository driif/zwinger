# zwinger

- Do all work with the `chief` skill. One Opus subagent implements each ticket. Do not use Codex.
- A working app comes first. A bug that blocks a main flow goes before any ticket.
- Decisions in `CONTEXT.md`, `docs/adr/` and `docs/designs/` win over a ticket. A conflict goes to the user.
- Done means seen working: after each batch, run the main flow in the real app (`verify-in-app` skill).
- Go to the user only when you cannot continue.
- Ideas that are out of scope become backlog issues: GitHub issues with the label `backlog`. Do not build them.
- A change to user-visible behaviour updates the matching page in `site/src/content/docs/` in the same change.
- All UI comes from `@driif/motiv`. Load `building-ui-with-motiv` before you do UI work.
- Write responses to humans and agents in English, per ASD-STE100 (Simplified Technical English).
  Be short and direct. Give the conclusion first. Do not add filler.
- Never give credit to Claude, Anthropic, Codex or OpenAI in commits, PRs or issues.
- Skills are in `.claude/skills/`. Create skills locally. Never create them globally.
