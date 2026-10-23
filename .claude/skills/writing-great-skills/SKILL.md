---
name: writing-great-skills
description: Use to write, edit or prune a skill. Gives vocabulary and principles for predictable skills.
disable-model-invocation: true
---

**Predictability** (the same _process_ each run, not the same output) is the primary skill quality; all levers below serve it. Terms: [GLOSSARY.md](GLOSSARY.md).

## Files

A skill is two files, always in this repo, never global:

- `.claude/skills/<name>/SKILL.md`: the skill.
- `.agents/skills/<name>/SKILL.md`: the Codex entrypoint. Same `name` and `description`; the body only points to the shared skill. A user-invoked skill also gets `.agents/skills/<name>/agents/openai.yaml` with `allow_implicit_invocation: false`.

A rename, a new skill or a deleted skill changes both files.

## Invocation

- **Model-invoked**: its **description** lets the agent and other skills start it. Adds **context load** each turn.
- **User-invoked**: `disable-model-invocation: true`; only a human typing its name starts it. **Cognitive load**, not context load: the human is the index.

Model-invoke only when the agent or a skill must start it. Many user-invoked skills: add a **router skill** naming each.

## Description

Triggers only ("Use when…"). Each word adds context load.

- Put the **leading word** first.
- One trigger per **branch**. Merge synonyms.
- Permit a "when another skill needs…" clause; identity goes in the body.

## Information hierarchy

**Steps** (ordered actions) and **reference** (on demand), ranked:

1. **In-skill steps.** Each ends on a checkable, ideally exhaustive **completion criterion** ("every modified model accounted for"). Vague causes **premature completion**; demanding increases **legwork**, also in flat reference.
2. **In-skill reference.** Flat peer sets are acceptable.
3. **Disclosed reference.** Behind a **context pointer**.

**Progressive disclosure**: inline what each branch needs; disclose what only some need. Pointer _wording_ sets its reliability. **Co-location**: a concept's definition, rules and caveats under one heading.

## When to split

Each split adds load (**granularity**):

- **By invocation**: a distinct leading word or a different skill must start it.
- **By sequence**: visible **post-completion steps** cause a rush.

## Pruning

- **Single source of truth**: each meaning in one location. Cures **duplication**.
- **Relevance**: each line applies to the task. Cures **sediment**.
- **No-op test** per sentence: if it does not change default behavior, delete it. Prefer strong leading words (_relentless_) to **no-op** ones (_thorough_).

## Leading words

Leading words (_fog of war_) anchor execution in the body, invocation in the description. Merge restatements: "fast, deterministic, low-overhead" → _tight_; "a loop you believe in" → _red_.

## Failure modes

- **Premature completion**: make the criterion precise. Split only if it stays vague and rushing occurs.
- **Sprawl**: too long, even if relevant. Disclose or split.
- **Negation**: a prohibition makes the prohibited thing likelier. State the positive target. Keep bans only as paired guardrails.
