# Design It Twice

Use parallel sub-agents to compare interfaces for one deepening candidate. The first idea is rarely the best (Ousterhout). Vocabulary: [SKILL.md](SKILL.md) and `CONTEXT.md`.

## 1. Define the problem

Briefly explain to the user: the interface constraints, the dependencies and their category ([DEEPENING.md](DEEPENING.md)), and a rough code sketch. Then start step 2 immediately.

## 2. Start sub-agents

Start 3+ sub-agents in parallel (`superpowers:dispatching-parallel-agents`). Give each a technical brief (file paths, coupling, dependency category, what is behind the seam, both vocabularies) and a different constraint:

1. Minimize the interface: 1–3 entry points, maximum leverage each.
2. Maximize flexibility: many use cases, extension.
3. Optimize the most common caller: simple default case.
4. (If applicable) Ports & adapters for cross-seam dependencies.

Each returns: the interface (types, invariants, ordering, error modes), a usage example, what is behind the seam, dependency strategy and adapters, and trade-offs.

## 3. Compare and recommend

Show each design separately. Compare in prose: **depth**, **locality** and **seam placement**. Recommend one, or a hybrid of compatible parts. Ask before building.
