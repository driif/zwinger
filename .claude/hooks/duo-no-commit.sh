#!/usr/bin/env bash
# PreToolUse(Bash) guard for duo rounds. duo-round.sh exports DUO_ROUND to the
# agents it starts; inside a round, git history and the index are read-only —
# the chief commits after review.
# Exit 2 sends the message on stderr back to the model as a blocking error.
set -uo pipefail

[ -n "${DUO_ROUND:-}" ] || exit 0

payload=$(cat)
cmd=$(printf '%s' "$payload" | /usr/bin/python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("tool_input",{}).get("command",""))
except Exception: print("")')

# Anything between `git` and the verb is options (`-C <dir>`, `-c k=v`).
printf '%s' "$cmd" | grep -qE '(^|[;&|]|[[:space:]])git[[:space:]]+([^;&|]*[[:space:]]+)?(commit|push|add|stash|rebase|merge|cherry-pick|revert|reset|checkout|switch|restore|am|apply|tag|clean)([[:space:]]|$)' || exit 0

cat >&2 <<MSG
Blocked: duo agents never write git history or the index.

  command: $cmd

Leave changes in the working tree; the chief commits after review.
MSG
exit 2
