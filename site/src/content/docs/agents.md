---
title: Agents
description: Run Claude Code, Codex, pi, Gemini CLI or Antigravity CLI in a cage, or on your Mac with no cage.
---

An **agent** is an AI coding CLI that runs in a [cage](/cages/): pi, Claude Code, Codex, Gemini CLI or Antigravity
CLI. zwinger has no chat interface of its own. The agent runs as its own terminal interface, with all its functions.

## Agent sessions

The [effective cage](/cages/#attach-or-inherit) of the project sets where an agent runs:

- **A project with a cage** opens the agent in the cage. An agent session (⌘P) runs in `/workspace`. If the project has
  a harness folder (**Settings > Harness** of the project), the agent session starts there.
- **A project with no cage** opens the agent on your Mac. See [Agents with no cage](#agents-with-no-cage).

A Mac shell (⌘T) runs on the Mac with no sandbox. zwinger never types an agent command into a Mac shell.

In a VM shell (⌘⇧T), the commands `claude`, `codex`, `pi`, `gemini` and `agy` are on the `PATH`.

**Continue last session** starts the agent with its continue option (`claude -c`, `codex resume --last`, `pi -c`,
`gemini --resume latest`, `agy -c`). It continues a session of the agent terminals only, never a call of a
[task run](/tasks/#agent-state).

If the agent exits in less than 5 seconds after a continue, with an error or with no turn, it had no earlier
conversation. zwinger then starts the agent again in the same terminal without the continue option, one time only, and
the terminal shows "No earlier conversation; started a new one."

## Agents with no cage

A project with no cage opens the agents that are installed on your Mac: **New Claude Code**, **New Codex**, **New pi**,
**New Gemini CLI**, **New Antigravity CLI** and their **continue last session** actions. zwinger finds Antigravity CLI at
`~/.local/bin/agy`, where its installer puts it, or as `agy` in a folder of `PATH`. An agent that zwinger did not find on your Mac is
disabled, with the reason "*agent* is not installed on this Mac". The agent starts in the project folder, through your
login shell, so it gets the `PATH` of your profile. zwinger uses only an agent binary in an absolute folder of
`PATH`, so a binary in the project folder never runs. pi, Gemini CLI and the npm install of Codex run with the `node`
of their npm folder (for example `/opt/homebrew/bin/node` or the nvm version that has the package). If that folder has
no `node` (bun, pnpm or a Homebrew formula), the agent uses the `node` of your `PATH`.

An agent tab from the time when the project had a cage also starts on your Mac: **Continue last session** continues
the last session of the agent on your Mac, not the session in the VM.

Nothing isolates this agent. It runs as your macOS user and can read all your files. This includes the
[vault file](/network/#credentials) with the credential values of all cages. So zwinger shows a warning before each
open, also before a reopen and **Continue last session**: "*agent* runs on your Mac with your files and your logins.
Nothing isolates it." **Continue** opens the agent, and **Cancel** opens nothing. There is no option to stop the warning.

An agent with no cage gets a guard:

- No bypass option. **Agents run without permission prompts** does not apply, so the agent asks for its permissions.
- No credentials and no placeholders. The agent uses its own config and login on your Mac.
- No proxy and no CA environment variables from zwinger, and no override of the agent's state folder (`HOME`,
  `CODEX_HOME` or `PI_CODING_AGENT_DIR`). Your shell profile can still set proxy variables.

The terminal tab shows a no-cage icon. Point to it to read "No cage: runs on this Mac". Claude Code, pi and Gemini CLI
show the [agent status](#agent-status) glyph; Codex and Antigravity CLI show none. A cage stop does not end the agent; an app exit ends it, as it ends the Mac shells.

zwinger checks the project again at each open. If the project gets a cage, the agent with no cage does not open again:
its tab says "the session has a cage". Open the agent in the cage instead.

Tasks need a cage. A project with no cage cannot run a task.

## Agent status

The terminal tabs and the session and workspace rows show an agent status glyph while an agent runs in a VM terminal. This
includes an agent that you start in a VM shell. An agent with no cage shows it too, see
[Status of agents with no cage](#status-of-agents-with-no-cage):

- **Active:** the agent is working.
- **Blocked:** the agent waits for your answer or permission.
- **Done:** the agent finished its turn and waits for a new prompt.

Point to a glyph to read its status. A session with several agents shows the most urgent state: **blocked > active > done**.
A workspace uses the same rule across its sessions. The glyph disappears when the agent exits. In a VM shell, the
shell stays open and its tab shows no glyph again.

When an agent goes to **blocked** while zwinger is in the background, the dock icon bounces once and its badge shows
the number of blocked agents. The badge goes when you bring the zwinger window to the front.

For interactive terminals, zwinger adds status hooks to each agent. The hooks write status signals to the terminal;
they do not change your agent settings or a read-only agent profile:

- **Claude Code** reports all three states. It is blocked on a permission prompt and on a question
  (`AskUserQuestion`).
- **Codex** reports all three states. Its hooks are in `/etc/codex/hooks.json` in the VM, as system hooks, so Codex
  does not ask you to trust them. If the VM image has its own `/etc/codex/hooks.json`, zwinger keeps it, and Codex
  reports no status. It is blocked on a permission prompt and on a question (`request_user_input`). With **Agents
  run without permission prompts** on, Codex never asks for a permission, but it can still ask a question.
- **Gemini CLI** reports all three states. Its hooks are in a system defaults file, so your own Gemini settings win
  and your own hooks run too. Gemini CLI runs hooks only in a trusted folder. It is blocked on a permission prompt
  and on a question (`ask_user`).
- **Pi** reports all three states. It asks for no permissions; it is blocked while an extension dialog (select,
  confirm, input or editor) waits for you. A custom extension screen does not show blocked.
- **Antigravity CLI** reports no status. Its tab shows no glyph.

A headless call, such as `claude -p`, `codex exec`, `gemini -p` or `agy -p`, reports no status.

### Status of agents with no cage

zwinger never writes the config of your agents on your Mac (`~/.claude`, `~/.codex`, `~/.gemini`, `~/.pi`). It
keeps the status hooks in its own app data folder, and each launch of an agent with no cage names them with an
option or an environment variable. A Mac shell gets no hooks, so an agent that you start in a Mac shell shows no glyph.

- **Claude Code** reports all three states. It starts with `--settings` and the hooks file of zwinger. Claude Code
  adds these hooks to the hooks of your own settings.
- **Pi** reports all three states. It starts with `-e` and the status extension of zwinger, in addition to your own
  extensions.
- **Gemini CLI** reports all three states. `GEMINI_CLI_SYSTEM_DEFAULTS_PATH` points at the system defaults file of
  zwinger, so your own Gemini settings win. Gemini CLI runs hooks only in a trusted folder. If your Mac already has Gemini system defaults
  (`GEMINI_CLI_SYSTEM_DEFAULTS_PATH` is set, a `system-defaults.json` is next to `GEMINI_CLI_SYSTEM_SETTINGS_PATH`, or
  `/Library/Application Support/GeminiCli/system-defaults.json` exists), zwinger does not replace them, and Gemini CLI
  reports no status.
- **Codex** reports no status. Codex runs hooks from a command-line option only after you trust them, or with an
  option that skips the trust check for all hooks. The guard does not allow that option.
- **Antigravity CLI** reports no status.

## Permission prompts

The project setting **Agents run without permission prompts** is on by default. It adds the bypass option of each
agent (for example `--dangerously-skip-permissions` for Claude Code). This is safe only because the cage is the
sandbox: the agent has only its mounts, its allowed hosts and placeholders for its credentials. Antigravity CLI gets
`--dangerously-skip-permissions`, unless you give your own `--mode`. A change of the setting applies immediately, also
in a running cage.

## Agents from your Mac

zwinger finds the agents that are installed on your Mac. A project can use them in place of its own copy. Turn on one
agent per kind on the project page. A change of Claude Code or Codex, with its profile and login, applies at once,
also in a running cage. A change of pi or Gemini CLI applies at the next cage start: the cage mounts their package
from your Mac when it starts, and shows a pending restart until then.

When you use pi from your Mac, the cage checks that its terminal interface starts. It repeats the check if the pi
installation path, pi version, guest image or VM runtime changes. Gemini CLI uses a version check with the same
rules. It also repeats the check when its entry point changes. A failed check is retried at the next start.

For Claude Code, run `claude setup-token` on your Mac. In **Add agent**, paste the token into the password field and save it. The cage stores the token in its vault. To use one token in more than one cage, save it as `CLAUDE_CODE_OAUTH_TOKEN` in **Settings > Credentials**, then pick **Use library token** in **Add agent**. A rotation in the library reaches each cage that uses it. The guest sees only a placeholder in `CLAUDE_CODE_OAUTH_TOKEN`; the proxy sets `authorization: Bearer <token>` only for `api.anthropic.com`. Under default Block, use **Allow api.anthropic.com** before saving. A Block rule can still override that Allow rule.

A Claude setup token is valid for one year after `claude setup-token` makes it. That command shows the exact expiry. zwinger cannot read the expiry from the token, so it shows an estimate: **expires about** one year after you saved the token. Save the token soon after you make it to keep the estimate close. A save of the same token again, also in the credential editor, keeps the first date. The project warns during the last 30 days of the estimate. Run `claude setup-token` again and replace the stored token before it expires. A cage with Claude host login and no valid token still starts, so you can use the other agents and the shell. The proxy does not send a Claude token, and the cage shows a notice. Save a new token in the cage: a running cage gets it at once, and a new `claude` uses it. If you turn the Claude login off or remove Claude Code from the cage, zwinger deletes the stored token and its credential. A library token stays in the library and in the other cages; only this cage detaches it.

Codex, pi and Gemini CLI use their login files on your Mac. The agent in the cage gets placeholders only, and the proxy adds the real token to provider requests. If a file login expires, open that agent on your Mac once to renew it. A new file login applies without a cage restart.

The **profile** is optional, per project and agent. It gives the agent a read-only copy of your configuration, for
example the settings, `CLAUDE.md`, skills, commands, agents and plugins of `~/.claude`. Logins, history, sessions and
projects are never copied. The profile also skips Git metadata (`.git`) in plugins and other folders.
The agent cannot change the profile. The map shows it as `profile (ro)`.

Gemini CLI runs only as an agent from your Mac. Antigravity CLI is not an agent from your Mac for a cage. See
[Antigravity CLI](#antigravity-cli).

### Gemini CLI login check

Gemini CLI checks its cached token at `https://oauth2.googleapis.com/tokeninfo`. The proxy accepts this check only
when it carries the login placeholder. A Block rule for the host still applies. The check has two answer paths:

- If the login on your Mac names its scopes, zwinger answers the check itself, from the scopes and expiry of that
  login. No request goes to Google. Activity shows `login check · answered by zwinger`.
- If the login names no scopes, the proxy sends the original request to Google: the same method, path, query,
  end-to-end header fields and body. The proxy removes hop-by-hop header fields and sets `Host`, as for all forwarded
  requests. The real token replaces the placeholder only in the `authorization` header and in the `access_token`
  query value. Google's status, end-to-end header fields and body come back to the agent. Activity shows
  `login check · sent to Google`. This path works only over HTTPS on port 443; other checks are blocked.

## Other logins

An agent can also use an API key as a [credential](/network/#credentials), or its own `/login`. A `/login` in the cage
needs Allow rules for the login hosts of the provider, and it keeps the real token in the cage, where the agent can read
it. zwinger warns when it finds such a login: **Remove it** deletes the login file, and **Use my Mac login instead**
opens the settings for agents from your Mac.

## Antigravity CLI

Antigravity CLI (`agy`) runs in a cage from its own Linux build. Install it one time in **Settings > Runtimes**. In the
cage it uses the Gemini API with your API key:

1. Add an Allow rule for the **Google Gemini** service to the cage.
2. Add a [credential](/network/#credentials) with the name `GEMINI_API_KEY` to that rule, with your Gemini API key as
   the value.
3. Open **New Antigravity CLI**.

The agent sees only a placeholder in `GEMINI_API_KEY`. The proxy sets the real key in the `x-goog-api-key` header,
only for `generativelanguage.googleapis.com`. Antigravity CLI uses the key only when its settings select the Gemini API.
So at each cage start, zwinger writes `~/.gemini/antigravity-cli/settings.json` with `"modelProvider": "gemini"` in the
agent state of the cage, if that file does not exist. zwinger does not change a settings file that exists.

Limits:

- **No Google sign-in in a cage.** Antigravity CLI keeps its sign-in token where the agent can read it: in the keyring
  of your Mac, or in a file in the cage. Google does not document how to split this login. So zwinger cannot keep the
  token out of the cage, and a cage does not use your Antigravity sign-in or subscription. If you sign in in the cage,
  the real token stays in the cage, where the agent can read and send it. zwinger does not find or remove this login.
- **No agent from your Mac for a cage.** The Antigravity CLI of your Mac runs only in a project with no cage.
  **Settings > Agents on this Mac** shows it with this limit.
- **No tasks.** A task cannot use Antigravity CLI.
- **No status.** See [Agent status](#agent-status).
- **No self-update in the cage.** The runtime is read-only. A new zwinger version can pin a newer version.

## Runtimes

zwinger includes pi. Install Claude Code, Codex and Antigravity CLI one time in **Settings > Runtimes**, for all
projects. zwinger downloads a pinned version and checks its hash: the SHA-512 of the npm package for Claude Code and
Codex, the SHA-256 of the official GitHub release asset for Antigravity CLI. A runtime that you install while a cage
runs is available in the cage at once, with no restart.
