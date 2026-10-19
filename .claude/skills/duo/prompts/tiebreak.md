# Tiebreak

Two models exchanged plans and did not agree on the implementer. Choose the implementer.

**Use only the transcript. Do not open files or run commands.** Your decision is which model is the implementer. Do not check their claims about the code.

## The task

{{TASK}}

## The exchange

{{TRANSCRIPT}}

## Decision rules

1. Prefer the plan that names real files and a real risk to a general plan.
2. A model that conceded a specific point and kept a different point read the exchange. A model that conceded no points, or all points, did not.
3. If the plans are almost equal, prefer the model whose `touches` agree with the areas that the task changes.
4. Do not divide the work. One implementer.

## Output

Return **only** a JSON object in a fenced ```json block:

- `implementer` — `opus` or `sol`.
- `why` — two or three sentences that name the deciding difference. "Argued better" is not a reason.
- `plan` — the plan to build, from the transcript. Use the stronger plan as the base. Add the correct points of the other plan. The plan is the contract for the implementer, so it must be complete without other documents.
- `open_questions` — array of `unknowns` that the user must answer. Each entry stops the round. Include a question only if work without the answer gives an incorrect result.
