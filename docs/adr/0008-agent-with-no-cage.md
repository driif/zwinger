---
status: accepted
---

# Agent with no cage

A session with no cage can open an agent. The agent runs on the Mac as the user, so zwinger also works as a plain terminal app. This reverses the rule of ADR 0004 that zwinger never starts an agent on the Mac; host shells stay as ADR 0004 says. Because nothing isolates this agent, it gets a guard: no bypass flags ("Agents run without permission prompts" does not apply), no credentials, no proxy or CA env, and it uses the Mac's own agent config and login. The app shows a warning at each launch, and the tree marks the terminal "no cage". Tasks need a cage.

## Considered Options

- **No agent without a cage** (ADR 0004). Rejected by the user: the user can already type `claude` in a host shell, and the app should work as a terminal app.
- **The same flags as in a cage.** Rejected: the bypass flags are acceptable only because the cage is the sandbox.

## Consequences

- An agent with no cage can read all files of the user, including the credentials file of the file vault (#114). The docs say so.
