---
name: duo
description: Use on "duo", "/duo <task>", "run this through both models", or when the chief skill delegates. Opus and sol write blind plans, agree on one plan and one implementer, the implementer writes, the other reviews, both do a blind final review. Changes stay uncommitted.
---

# duo — two models plan, one implementer writes, both do a final review

The models are `claude/opus:xhigh` and `codex/gpt-6-sol:xhigh`. A round with two exchange turns makes approximately 11 model calls.

| stage | who | access |
|---|---|---|
| `plan` | both, blind | read-only |
| `exchange` | both, with the transcript | read-only |
| `tiebreak` | opus, only if the models do not agree | read-only |
| `implement` | the implementer | **write** |
| `crossreview` | the other model | read-only |
| `revise` | the implementer, if the verdict is `revise` | **write** |
| `final review` | both, independently | read-only |

**Invariant: the first plans and the final reviews are blind.** A model does not see the other model's output before it writes its own.

The changes stay uncommitted on the **current branch**. The models never commit, stage or push. `.claude/hooks/duo-no-commit.sh` blocks this while `DUO_ROUND` is set; the codex sandbox also blocks it. The caller does the commit and the external review.

## 1. Write the task

1. Make a kebab-case slug for the task.
2. Write `docs/tasks/<slug>/task.md`. Include the request, the constraints that the code does not show, and the applicable gates:
   - `pnpm build` for the frontend.
   - `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` and `cargo test --manifest-path src-tauri/Cargo.toml` for `src-tauri/`.
3. Use two sections: **Asked for** (the user's words, fixed) and **Chosen here** (your inferences, open to challenge).
4. Ask the user first only if the request is ambiguous.
5. Install the dependencies before the round. A round has no network.

The models read only `task.md` and `AGENTS.md`.

## 2. Run

```bash
.claude/skills/duo/scripts/duo-round.sh <slug> <round>   # e.g. 01-initial
```

A round takes 20-50 minutes.

1. **Run the script in the background.** Wait for the notification.
2. Tell the user about each `walkover`, `agent_degraded` and `stopped` event in `events.jsonl` when it occurs.

| exit | meaning | action |
|---|---|---|
| **0** | done | Go to step 3. |
| **2** | tool error | Read `*.err`. |
| **3** | open questions | Answer them from the code. Ask the user only for product decisions. Add the answers to `task.md`. Run again with a **new** round name. |

## 3. Report

- From `transcript.md`: which model conceded which point, and why the models chose that implementer.
- From `manifest.json`: `implementer`, `crossreview_verdict`, `ship`, `ship_basis`, `blocking`, `backlog`.
- If `degraded` is not empty, report it first.
- `ship` is true only when **both** final reviewers set it.

## Round directory

`docs/tasks/<slug>/<round>/` (gitignored): `transcript.md`, `turns/`, `tiebreak.json`, `plan.md`, `implementation.json`, `crossreview.json`, `revision.json`, `final-{opus,sol}.json`, `history.md`, `events.jsonl`, `manifest.json`.

Environment knobs: `DUO_TURNS` (2), `DUO_IDLE_TIMEOUT` (600s), `DUO_HARD_TIMEOUT` (1800s), `DUO_STAGGER` (10s).

## Degraded models

- If a model does not make a plan, it is degraded. The other model continues by **walkover**. Do not call a walkover an agreement.
- If both models do not make a plan, the script exits 2.
- If a model fails during the exchange, the tiebreak decides.
- If a final reviewer fails, `ship` is false.
