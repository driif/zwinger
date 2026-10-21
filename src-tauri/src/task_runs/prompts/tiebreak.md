# Tiebreak

Two agents exchanged plans and did not agree on the implementer. Choose the implementer.

**Use only the transcript. Do not open files or run commands.**

{{CONTEXT}}{{MESSAGES}}

## The exchange

{{TRANSCRIPT}}

## Decision rules

1. Prefer the plan that names real files and a real risk to a general plan.
2. An agent that conceded a specific point and kept a different point read the exchange.
3. Do not divide the work. One implementer.

## Output

Return **only** a JSON object in a fenced ```json block:

- `implementer` — {{AGENTS}}.
- `why` — two or three sentences that name the deciding difference.
- `plan` — the plan to build, from the transcript. It is the contract for the implementer, so it must be complete without other documents.
- `open_questions` — array of questions that the user must answer. Each entry stops the run until the user answers.
