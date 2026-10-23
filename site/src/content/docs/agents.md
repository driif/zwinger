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

## Permission prompts

The project setting **Agents run without permission prompts** is on by default. It adds the bypass option of each
agent (for example `--dangerously-skip-permissions` for Claude Code). This is safe only because the cage is the
sandbox: the agent has only its mounts, its allowed hosts and placeholders for its credentials. A change of the
setting applies immediately, also in a running cage.

## Agents from your Mac

zwinger finds the agents that are installed on your Mac. A project can use them in place of its own copy. Turn on one
agent per kind on the project page. The change applies at the next cage start.

The **login** of the agent on your Mac stays on the Mac. The agent in the cage gets a placeholder only. The proxy of
the cage adds the real token to the requests to the provider. zwinger never writes your Keychain or your login files.

- Claude Code: macOS asks one time to allow access to the Keychain item `Claude Code-credentials`.
- If the login on the Mac expires, the cage does not start. Open the agent on the Mac one time to log in again.
- A new login on the Mac applies with no restart.

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
