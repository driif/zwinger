---
status: accepted
---

# A host agent's subscription login is proxy-injected

A shield reuses the user's subscription login of a host agent (Claude Code, Codex, pi, Gemini) without giving the token to the agent. The agent in the shield gets a login file or variable with placeholders only. The shield's network proxy puts in the real token for the agent's provider hosts only, and zwinger refreshes the token on the Mac, so a refresh response never reaches the guest. A prompt-injected agent therefore cannot read or send the user's subscription token. The cost is per-agent work: each agent's login format and refresh flow must be checked before it is supported.

## Considered Options

- **Copied login**: a private copy of the login in the shield state. It works with every agent at once, but the agent can read the token and send it to any reachable host. Rejected by the user on 2026-10-21.

## Consequences

- An agent whose login cannot be split this way (for example, a token that must sign requests in the guest) is not supported as a host agent until a safe way exists.
- Token refresh is an app (host) responsibility. A refresh by the agent itself must be blocked or answered by the proxy.
