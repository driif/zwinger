---
title: Agents
description: Run Claude Code, Codex, pi or Gemini CLI in a cage.
---

An **agent** is an AI coding CLI that runs in a [cage](/cages/): pi, Claude Code, Codex or Gemini CLI. zwinger has no
chat interface of its own. The agent runs as its own terminal interface, with all its functions.

## Agent sessions

An agent session (⌘P) runs the agent in the cage, in `/workspace`. If the project has a harness folder
(**Settings > Harness** of the project), the agent session starts there. It is different from a Mac shell (⌘T), which runs
on the Mac with no sandbox. zwinger never starts an agent in a Mac shell.

In a VM shell (⌘⇧T), the commands `claude`, `codex`, `pi` and `gemini` are on the `PATH`.

**Continue last session** starts the agent with its continue option (`claude -c`, `codex resume --last`, `pi -c`,
`gemini --resume latest`).

## Agent status

The terminal, session and workspace rows show an agent status glyph while an agent terminal runs:

- **Active:** the agent is working.
- **Blocked:** the agent waits for your answer or permission.
- **Done:** the agent finished its turn and waits for a new prompt.

Point to a glyph to read its status. A session with several agents shows the most urgent state: **blocked > active > done**.
A workspace uses the same rule across its sessions. The glyph disappears when the agent process exits.

For interactive terminals, zwinger adds status hooks to Claude Code and pi. The hooks write status signals to the
terminal; they do not change a read-only agent profile. Claude Code reports all three states. Pi reports active and
done; it does not report blocked. Codex and Gemini CLI do not report status yet, so their rows show no glyph.

## Permission prompts

The project setting **Agents run without permission prompts** is on by default. It adds the bypass option of each
agent (for example `--dangerously-skip-permissions` for Claude Code). This is safe only because the cage is the
sandbox: the agent has only its mounts, its allowed hosts and placeholders for its credentials. A change of the
setting applies immediately, also in a running cage.

## Agents from your Mac

zwinger finds the agents that are installed on your Mac. A project can use them in place of its own copy. Turn on one
agent per kind on the project page. The change applies at the next cage start.

For Claude Code, run `claude setup-token` on your Mac. In **Add agent**, paste the token into the password field and save it. The cage stores the token in its vault. The guest sees only a placeholder in `CLAUDE_CODE_OAUTH_TOKEN`; the proxy sets `authorization: Bearer <token>` only for `api.anthropic.com`. Under default Block, use **Allow api.anthropic.com** before saving. A Block rule can still override that Allow rule.

The Claude token has an estimated 365-day lifetime from when you save it. The project shows its expiry and warns during the last 30 days. Run `claude setup-token` again and replace the stored token before it expires. A cage with Claude host login and no valid token does not start. If you turn the Claude login off or remove Claude Code from the cage, zwinger deletes the stored token and its credential.

Codex, pi and Gemini CLI use their login files on your Mac. The agent in the cage gets placeholders only, and the proxy adds the real token to provider requests. If a file login expires, open that agent on your Mac once to renew it. A new file login applies without a cage restart.

The **profile** is optional, per project and agent. It gives the agent a read-only copy of your configuration, for
example the settings, `CLAUDE.md`, skills, commands, agents and plugins of `~/.claude`. Logins, history, sessions and
projects are never copied. The agent cannot change the profile. The map shows it as `profile (ro)`.

Gemini CLI runs only as an agent from your Mac. zwinger detects Antigravity but does not support it.

## Other logins

An agent can also use an API key as a [credential](/network/#credentials), or its own `/login`. A `/login` in the cage
needs Allow rules for the login hosts of the provider, and it keeps the real token in the cage, where the agent can read
it. zwinger warns when it finds such a login: **Remove it** deletes the login file, and **Use my Mac login instead**
opens the settings for agents from your Mac.

## Runtimes

zwinger includes pi. Install Claude Code and Codex one time in **Settings > Runtimes**, for all projects. zwinger
downloads a pinned version and checks its SHA-512 hash. A runtime that you install while a cage runs needs a cage
restart.
