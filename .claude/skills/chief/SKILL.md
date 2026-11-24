---
name: chief
description: "Use for each implementation task in this repo (feature, fix or refactor). Do not use when the user asks for a trivial change or for a different workflow."
---

# Chief of staff

You control the work. The user is the client. Make the decisions yourself.
Go to the user only for a product decision that the code does not answer, or for a destructive step.

## Loop

1. **Pick.** User reports first (`gh issue list --label bug`). Then a bug in a main flow (project, cage start, terminal, agent, task run) goes first.
   Then issues with the label `next`, then other tickets by value to the user, not by number. Close tickets that are superseded or of low value, with a one-line reason.
2. **Brief.** Write a short brief in the subagent prompt: the issue text, the files to start from, the done criteria.
   Quote the lines of `CONTEXT.md`, the ADRs and `docs/designs/` that the ticket touches. No quote means it touches none.
   Do not write preflight reports, proof receipts or hash records.
3. **Implement.** One Opus subagent per ticket. At most 3 at a time, only on tickets with no shared files.
   A trivial fix (typo, one line) you do yourself.
   The subagent runs only the tests of its area: `cargo test <module>`, the related `pnpm test` file.
   It does not stage, commit or push.
4. **Commit.** After each ticket, commit only its paths with a Conventional Commit message (`Closes #N`).
   Do not push yet.
5. **Batch review.** After about 5 tickets (or at the end of an epic), start one fresh subagent.
   Give it the commit range and the quoted decisions. It checks, in this order: (1) conformance with the decisions,
   (2) main flow and UX, (3) correctness and security, (4) docs in `site/src/content/docs/` for a user-visible change.
   Edge cases only for data loss or security. It reports only critical and major findings. One fix subagent fixes them.
6. **Gates and push.** Run the gates once per batch, in sequence:
   `pnpm build`, `pnpm test`, `pnpm --filter site build`, `cargo fmt --manifest-path src-tauri/Cargo.toml --check`,
   `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings`, `cargo test --manifest-path src-tauri/Cargo.toml`.
   Then `cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored live_` when cage code changed,
   and the `verify-in-app` skill. Fix failures, then push to the **current branch**. Never create branches. Check CI on the pushed head.
7. **Backlog.** File an issue only for a gap that a user would notice. At most one per ticket, no duplicates.
   Reviewer notes that are not issues go in the report.
8. **Report.** A short report: what shipped, the commits, the review result, the backlog issues.

## Build hygiene

- Use the normal `src-tauri/target` with incremental builds. Do not make a target dir per ticket.
- Parallel tickets in a worktree share one extra target dir: `CARGO_TARGET_DIR=../zwinger-target`.
- Do not keep test temp dirs or build copies in `docs/tasks/`.
- A subagent stops each server or browser that it starts before it reports. It runs Vite only with `--port 5170-5199 --strictPort`.
  Port 1420 belongs to the user's `pnpm tauri dev`.
