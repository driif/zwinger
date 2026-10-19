#!/usr/bin/env bash
# PreToolUse(Bash) guard. AGENTS.md forbids crediting Claude or Anthropic in
# anything that lands in project history or on GitHub.
#
# A session-start reminder supplies a Co-Authored-By trailer and states that
# AGENTS.md overrides it. That override has been missed in practice, so this
# enforces it rather than relying on the model to remember.
# Exit 2 sends the message on stderr back to the model as a blocking error.
set -uo pipefail

payload=$(cat)
cmd=$(printf '%s' "$payload" | /usr/bin/python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("tool_input",{}).get("command",""))
except Exception: print("")')
cwd=$(printf '%s' "$payload" | /usr/bin/python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("cwd",""))
except Exception: print("")')

# Only commands that write a message are in scope: a commit, a tag, or the
# gh calls that carry a PR/issue body.
printf '%s' "$cmd" | grep -qE '(^|[;&|]|[[:space:]])(git[[:space:]]+([^;&|]*[[:space:]]+)?(commit|tag|merge|revert|cherry-pick)|gh[[:space:]]+(pr|issue|release))([[:space:]]|$)' || exit 0

# The message may be inline (-m, heredoc) or in a file (-F/--file/--body-file).
text="$cmd"
for f in $(printf '%s' "$cmd" | sed -nE 's/.*(-F|--file|--body-file)[[:space:]]+([^[:space:];&|]+).*/\2/p'); do
  body=$(cd "${cwd:-.}" 2>/dev/null && cat "$f" 2>/dev/null) && text="$text
$body"
done

hit=$(printf '%s' "$text" | grep -niE 'co-authored-by:[[:space:]]*(claude|anthropic|codex|openai)|generated with \[?(claude|codex)|noreply@anthropic\.com|🤖 generated' | head -3)
[ -z "$hit" ] && exit 0

cat >&2 <<MSG
Blocked: AGENTS.md forbids crediting Claude or Anthropic in project history.

$hit

No Co-Authored-By trailer naming Claude or Anthropic, no "Generated with
Claude Code", no mention of either as co-author — in commit messages, tags,
PR titles and descriptions, review comments or issue comments. Remove the
line and run the command again.
MSG
exit 2
