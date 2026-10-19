---
name: domain-modeling
description: Use to define domain terms or a ubiquitous language, to record an architectural decision (ADR), or when a different skill must update the domain model. Challenges terms and writes them to CONTEXT.md.
---

# Domain Modeling

During design, challenge terms, make edge-case scenarios, and record each term and decision when it resolves. To only read `CONTEXT.md` for vocabulary is not this skill.

## Files

- `CONTEXT.md` at the repo root: the glossary. Format: [CONTEXT-FORMAT.md](./CONTEXT-FORMAT.md).
- `docs/adr/NNNN-slug.md`: decisions. Format: [ADR-FORMAT.md](./ADR-FORMAT.md).
- `CONTEXT-MAP.md`: only if the repo has several contexts, each with its own `CONTEXT.md` and `docs/adr/`. Example: the React UI in `src/` and the Rust core in `src-tauri/` use different languages.

Make each file when it first has content.

## During the session

- **Compare with the glossary.** If a term conflicts with `CONTEXT.md`, say so: "The glossary defines 'cancellation' as X, but you mean Y. Which is correct?"
- **Make vague terms precise.** For a vague or overloaded word, propose one canonical term: "'account': the Customer or the User?"
- **Test with scenarios.** Make concrete edge cases that set concept boundaries.
- **Compare with the code.** Examine the code for each user claim about behavior. Report contradictions.
- **Update `CONTEXT.md` immediately** when each term resolves. Write only glossary entries, not implementation details, specifications or notes.
- **Offer an ADR only if** a decision is hard to reverse, surprising without context, *and* a real trade-off.
- **Control the IPC seam.** Tauri command names, event names and `invoke` types are shared TypeScript and Rust vocabulary. Record one name per concept in `CONTEXT.md` before the sides diverge.
