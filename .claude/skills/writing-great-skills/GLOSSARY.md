# Glossary: Writing Great Skills

Reference for [writing-great-skills](SKILL.md), grouped by axis. Each _failure mode_ is next to its cure.

**Predictability**: the agent behaves the same _way_ each run: same process, not same output (a brainstorming skill predictably diverges). Cost and maintainability follow from it. _Avoid_: consistency, reliability.

## Invocation

- **Model-Invoked**: its **description** lets the agent, other skills and the human start it. Permanent **context load**. A reference-only one can share reference. _Avoid_: ability, tool.
- **User-Invoked**: `disable-model-invocation: true`. Only the human can start it. Zero context load. _Avoid_: procedure, workflow, command.
- **Description**: the machine-readable trigger: a model-invoked skill's one always-loaded **context pointer** and context load source. _Avoid_: frontmatter, summary.
- **Context Pointer**: an in-context reference to out-of-context material and when to reach it. Its wording, not its target, sets when and how reliably it is followed. Weak pointer to necessary material: make the wording precise; inline only if that fails. _Avoid_: link, import.
- **Context Load**: the token and attention cost of always-loaded descriptions. Limits splits into model-invoked skills.
- **Cognitive Load**: what the human must remember about user-invoked skills (the human is the index). Limits splits into user-invoked skills. Spend it where human judgement matters.
- **Router Skill**: a user-invoked skill that lists the others and when to use each. It cannot start them. Cures cognitive load. _Avoid_: dispatcher, menu.
- **Granularity**: how finely skills are divided; each split adds one load. Split **by invocation** where a distinct **leading word** must trigger it; **by sequence** to hide **post-completion steps**. Merged sequences expose later steps. _Avoid_: chunking, modularity.

## Information Hierarchy

Content ranked by when the agent needs it: **steps** in-file, **reference** in-file, disclosed reference. Independent of invocation. Inline reference that must be disclosed hides steps. _Avoid_: structure, layout.

- **Steps**: ordered actions; when present, the primary tier. Each ends on a **completion criterion**. _Avoid_: workflow, instructions.
- **Reference**: material used on demand: definitions, facts, examples, conditional instructions. Primary candidate for **progressive disclosure**. _Avoid_: docs, background.
- **External Reference**: a plain file outside the skill system, available to any skill. The only shared location for two user-invoked skills. _Avoid_: knowledge base.
- **Progressive Disclosure**: reference behind a context pointer, so the top stays readable. **Branching** permits it: disclose what only some branches need. _Avoid_: lazy loading.
- **Co-location**: a concept's definition, rules and caveats under one heading, so each part brings the others. Not duplication: scattering divides a meaning; duplication repeats it. _Avoid_: grouping, cohesion.
- **Sprawl**: _failure mode._ Too long, even with each line relevant and unique. Costs readability, maintainability, tokens. Cure: disclose reference; split by branch or sequence. _Avoid_: bloat, verbosity.

## Steering

- **Branch**: a distinct way to invoke a skill, so runs take different paths. _Avoid_: path, case, fork.
- **Leading Word**: a compact pretrained concept (_Leitwort_) the agent thinks with: _lesson_, _fog of war_, _tracer bullets_. Repeated as a token, never a sentence, it collects a distributed definition. It anchors execution in the body, invocation in the description, most when also in prompts, docs and code. Prefer existing words over coined ones (no priors). _Avoid_: keyword, motif.
- **Completion Criterion**: the condition that marks work done. **Clarity** (is done distinct from not-done?) prevents **premature completion**; **demand** (how much it requires) sets **legwork**, also in flat reference. Best: checkable, exhaustive. _Avoid_: exit condition.
- **Legwork**: the agent's own work in a step (reading, exploring, changing), not given to the user. Strong leading words and demanding criteria increase it; missing demand and premature completion decrease it. _Avoid_: effort, coverage.
- **Post-Completion Steps**: later steps. When visible, they move the agent ahead. _Avoid_: lookahead.
- **Premature Completion**: _failure mode._ Ending a step early because attention moves to completion. Requires steps and a vague criterion. Cure: a precise criterion. If it stays vague and rushing occurs, hide later steps. Hiding works only across a real context boundary (user-invoked hand-off or subagent), not an inline model-invoked call. _Avoid_: rushing.
- **Negation**: _failure mode._ A prohibition makes the prohibited behavior more available. Prompt the positive target. Keep a ban only where no positive phrasing exists, paired with the correct action. _Avoid_: don't-prompting.

## Pruning

- **Single Source of Truth**: each meaning in one location; a behavior change is one edit.
- **Duplication**: _failure mode._ One meaning in many locations. Costs maintenance and tokens; inflates prominence. The accidental opposite of a leading word, which repeats a token, not a meaning. _Avoid_: redundancy.
- **Relevance**: whether a line still applies to the skill's task; lost through explanation or age. Not no-op.
- **Sediment**: _failure mode._ Old layers that collect because additions seem safe, removals risky. _Avoid_: cruft, rot.
- **No-Op**: _failure mode._ A line the model obeys by default. Test: does it change default behavior? Also grades leading words: _be thorough_ is a no-op; _relentless_ is not. Run the skill to resolve disagreements. _Avoid_: restating the obvious.
