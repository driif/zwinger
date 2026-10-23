---
name: changing-motiv
description: Use when a component, prop, variant, token or look must change in @driif/motiv, the UI library of this app.
---

# Changing motiv

motiv is at `~/Documents/dev/my/motiv` (pnpm, workspaces `.` and `site`). Its `AGENTS.md` rules apply there, not the rules of this repo.

1. **Read first**: motiv `AGENTS.md`, then the component in `src/components/<Name>/` (kinetic), `src/drawn/` or `src/matrix/`.
2. **Branch**: `git switch -c duo/<slug>` from an up-to-date `main` in motiv.
3. **Build it in motiv** with its `duo` skill: write `docs/tasks/<slug>/task.md` there and run `.claude/skills/duo/scripts/duo-round.sh <slug> <round>` from the motiv root. Add a changeset (`pnpm changeset`, one consumer-facing line).
4. **Test in this app**: `pnpm add @driif/motiv@link:../motiv` (in `site/` too, if the site uses it). Never commit a `link:` dependency.
5. **Ship in motiv**: all motiv gates pass, then commit, push, open the PR and merge it. A merge to `main` opens the "Version Packages" PR. **Ask the user before you merge it**: that merge publishes to npm.
6. **Use the release**: after npm has the new version, run `pnpm up @driif/motiv@latest` in this repo (and `pnpm --filter site up @driif/motiv@latest`).

Done when the motiv PR is merged, the release is on npm, and this repo uses that version with no `link:` dependency.
