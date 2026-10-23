---
title: Tasks
description: Two agents plan, build and review a task together.
---

A **task** is a request that two agents of a project plan, build and review together. The agents run headless in the
[cage](/cages/), not in a terminal. They do not use a Mac shell (⌘T), which runs on the Mac with no sandbox, or a
VM shell (⌘⇧T).

## Create a task

A task has these fields:

- **Title** and **Request**.
- Two agents of different kinds (pi, Claude Code, Codex), each with an optional model. The first agent decides a
  tiebreak. Gemini CLI is also available if the project uses Gemini CLI from the Mac.
- **Gates**: shell lines that check the result, for example `pnpm build`.
- **Working dir**: `/workspace`, or a folder below it or below a mount.

A change to a task needs no restart.

## Run

A run is one execution of a task. It has these stages:

1. The two agents write plans in parallel. They do not see the plan of the other agent.
2. Up to 2 turns where the agents compare their plans.
3. A tiebreak by the first agent, if the agents do not agree.
4. The implementation.
5. The gates.
6. A cross review by the other agent.
7. One revision, if the review says `revise`.
8. Two final reviews in parallel. The result is `ship` only if the two reviewers say so.

If an agent fails, the run continues in a degraded mode, and the room shows it. A degraded result is never shown as an
agreement.

In the stages that must not change files, the run compares the git state of the working dir before and after. A change
is a violation. Outside a git work tree, this check is not possible, and the run tells you. A run never commits or
pushes.

## Room

The room is the live view of a run: the stages, the messages, the position of each agent, the disagreements, the
verdicts and the degraded agents. During a run you can:

- write a message. Each subsequent stage includes it.
- pause after the current stage, and resume.
- select the implementer before the implementation starts.
- answer open questions. The run waits for the answers.
- cancel the run.

A cage stop cancels its runs. A run does not continue after an app restart.

**Open round folder…** shows a round folder from your Mac in the room. You can read it, but you cannot change it.

## Records

zwinger writes the files of each run to its app data on the Mac. The cage cannot write there, so an agent cannot
change a verdict. zwinger also writes a copy to `.zwinger/tasks/<task>/<run>/` in the harness folder of the project (else in
`/workspace`) at each stage. A run starts only if zwinger can write this copy.
