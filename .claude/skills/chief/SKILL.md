---
name: chief
description: "Use for each implementation task in this repo (feature, fix or refactor). Do not use when the user asks for a trivial change or for a different workflow."
---

# Chief of staff

You control the work. The user is the client. Make the decisions yourself.
Go to the user only for a product decision that the code does not answer, or for a destructive step.

## Loop

1. **Pick.** A bug in a main flow (project, cage start, terminal, agent, task run) goes first.
   Then tickets by value to the user, not by number. Close tickets that are superseded or of low value, with a one-line reason.
2. **Brief.** Write a short brief in the subagent prompt: the issue text, the files to start from, the done criteria.
   Do not write preflight reports, proof receipts or hash records.
3. **Implement.** One Opus subagent per ticket. At most 3 at a time, only on tickets with no shared files.
   A trivial fix (typo, one line) you do yourself.
   The subagent runs only the tests of its area: `cargo test <module>`, the related `pnpm test` file.
   It does not stage, commit or push.
4. **Commit.** After each ticket, commit only its paths with a Conventional Commit message (`Closes #N`).
   Do not push yet.
5. **Batch review.** After about 5 tickets (or at the end of an epic), start one fresh subagent.
   Give it the commit range. It reports only critical and major findings: correctness, security, task not done,
   docs in `site/src/content/docs/` missing for a user-visible change. One fix subagent fixes them.
6. **Gates and push.** Run the gates once per batch, in sequence:
   `pnpm build`, `pnpm test`, `pnpm --filter site build`, `cargo fmt --manifest-path src-tauri/Cargo.toml --check`,
   `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings`, `cargo test --manifest-path src-tauri/Cargo.toml`.
   Fix failures, then push to the **current branch**. Never create branches. Check CI on the pushed head.
7. **Backlog.** File out-of-scope ideas as backlog issues, with no duplicates (`gh issue list --label backlog`).
8. **Report.** A short report: what shipped, the commits, the review result, the backlog issues.

## Build hygiene

- Use the normal `src-tauri/target` with incremental builds. Do not make a target dir per ticket.
- Parallel tickets in a worktree share one extra target dir: `CARGO_TARGET_DIR=../zwinger-target`.
- Do not keep test temp dirs or build copies in `docs/tasks/`.
- A subagent stops each server or browser that it starts before it reports. Never use port 1420 in a worktree: it blocks `pnpm tauri dev`.
