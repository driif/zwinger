#!/usr/bin/env bash
# One duo round: both models plan, argue it out, one implements, the other
# reviews, the implementer revises, then both read the whole thing.
#
# Usage:  duo-round.sh <task-slug> <round-name>
# Exit:   0 done · 2 tool error · 3 stopped on questions only the user can answer
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd -P)"
SKILL="$ROOT/.claude/skills/duo"
PROMPTS="$SKILL/prompts"
RULES="$ROOT/AGENTS.md"

IDLE_TIMEOUT=${DUO_IDLE_TIMEOUT:-600}
HARD_TIMEOUT=${DUO_HARD_TIMEOUT:-1800}
STAGGER=${DUO_STAGGER:-10}
TURNS=${DUO_TURNS:-2}                      # exchange turns before a tiebreak

slug=${1:-}; round=${2:-}
[ -z "$slug" ] || [ -z "$round" ] && { echo "usage: duo-round.sh <task-slug> <round-name>" >&2; exit 2; }

TASKDIR="$ROOT/docs/tasks/$slug"
ROUND="$TASKDIR/$round"
TASKFILE="$TASKDIR/task.md"

[ -f "$TASKFILE" ] || { echo "no task file at $TASKFILE" >&2; exit 2; }
[ -e "$ROUND/manifest.json" ] && { echo "round '$round' already ran — pick a new name" >&2; exit 2; }
mkdir -p "$ROUND/turns" || exit 2

# Every agent inherits this; .claude/hooks/duo-no-commit.sh blocks git history
# and index writes only while it is set.
export DUO_ROUND="$slug/$round"

EVENTS="$ROUND/events.jsonl"
TRANSCRIPT="$ROUND/transcript.md"
: > "$TRANSCRIPT"
ev() { printf '{"at":"%s","kind":"%s","detail":%s}\n' "$(date -u +%FT%TZ)" "$1" \
  "$(printf '%s' "$2" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" >> "$EVENTS"; }
die() { ev run_failed "$1"; echo "$1" >&2; exit 2; }

peer_of() { [ "$1" = opus ] && echo sol || echo opus; }

# --- watchdog -----------------------------------------------------------------
run_watched() {
  local prompt="$1" out="$2" name="$3"; shift 3
  : > "$out"
  # Bash points a background job's stdin at /dev/null, so the prompt is
  # redirected onto the job rather than onto this function.
  "$@" < "$prompt" >> "$out" 2>"$out.err" &
  local pid=$! start now last age idle
  start=$(date +%s)
  while kill -0 "$pid" 2>/dev/null; do
    sleep 5
    now=$(date +%s); age=$(( now - start ))
    # Neither CLI streams to stdout before it is done — both report progress on
    # stderr — so idleness is the newer of the two files.
    last=$(stat -f %m "$out" "$out.err" 2>/dev/null | sort -n | tail -1)
    last=${last:-$start}
    idle=$(( now - last ))
    if [ "$idle" -ge "$IDLE_TIMEOUT" ]; then
      pkill -9 -P "$pid" 2>/dev/null; kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
      ev agent_stalled "$name idle ${idle}s"; return 124
    fi
    if [ "$age" -ge "$HARD_TIMEOUT" ]; then
      pkill -9 -P "$pid" 2>/dev/null; kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
      ev agent_timeout "$name ran ${age}s"; return 124
    fi
  done
  wait "$pid"; return $?
}

# --- executors ----------------------------------------------------------------
# claude -p prints nothing until it is done, so a long write looked idle and was
# killed; its event stream goes to stderr (what the watchdog sees), the result to stdout.
claude_streamed() {
  claude -p --output-format stream-json --verbose "$@" | tee /dev/stderr |
    python3 -c 'import json,sys
for l in sys.stdin:
    try: e = json.loads(l)
    except ValueError: continue
    if e.get("type") == "result": sys.stdout.write(e.get("result") or "")'
}

run_model() {  # run_model <opus|sol> <promptfile> <outfile> <name> <read|write>
  local who="$1" prompt="$2" out="$3" name="$4" mode="$5"
  if [ "$who" = opus ]; then
    run_watched "$prompt" "$out" "$name" claude_streamed --model opus \
      --permission-mode bypassPermissions --add-dir "$ROOT"
  else
    local sandbox=read-only
    [ "$mode" = write ] && sandbox=workspace-write
    run_watched "$prompt" "$out" "$name" codex exec -m gpt-6-sol -s "$sandbox" \
      -C "$ROOT" -c model_reasoning_effort="xhigh" --skip-git-repo-check -
  fi
}

jsonblock() { python3 "$SKILL/scripts/jsonblock.py" "$1"; }

compose() {  # compose <template> <out> KEY=literal | KEY=@file ...
  local tpl="$1" out="$2"; shift 2
  cp "$tpl" "$out"
  local kv
  for kv in "$@"; do
    python3 - "$out" "${kv%%=*}" "${kv#*=}" <<'PY'
import sys
path, key, val = sys.argv[1], sys.argv[2], sys.argv[3]
if val.startswith('@'):
    val = open(val[1:], encoding='utf-8', errors='replace').read()
# Read before opening for write: 'w' truncates, so doing both in one expression
# reads back an empty file.
text = open(path, encoding='utf-8').read()
open(path, 'w', encoding='utf-8').write(text.replace('{{%s}}' % key, val))
PY
  done
}

# --- stage 1: blind first drafts ----------------------------------------------
ev stage "plan"
compose "$PROMPTS/plan.md" "$ROUND/plan.prompt.md" "TASK=$TASKFILE" "RULES=$RULES" "WORKDIR=$ROOT"

run_model opus "$ROUND/plan.prompt.md" "$ROUND/turns/plan-opus.raw" plan-opus read & fp=$!
sleep "$STAGGER"
run_model sol "$ROUND/plan.prompt.md" "$ROUND/turns/plan-sol.raw" plan-sol read & ap=$!
wait $fp; frc=$?; wait $ap; arc=$?

live=""
for who in opus sol; do
  rc=$frc; [ "$who" = sol ] && rc=$arc
  if [ "$rc" -eq 0 ] && jsonblock "$ROUND/turns/plan-$who.raw" > "$ROUND/turns/plan-$who.json" 2>/dev/null \
     && [ -s "$ROUND/turns/plan-$who.json" ]; then
    ev agent_done "$who planned"; live="$live $who"
  else
    rm -f "$ROUND/turns/plan-$who.json"; ev agent_degraded "$who plan failed rc=$rc"
  fi
done
live=${live# }
[ -z "$live" ] && die "both models failed to plan — see $ROUND/turns/*.err"

python3 "$SKILL/scripts/transcript.py" init "$ROUND" >> "$TRANSCRIPT" || die "transcript init failed"

implementer=""; agreed_plan=""
if [ "$live" != "opus sol" ]; then
  # Walkover: nothing was exchanged, so there is no agreement to record.
  implementer="$live"; ev walkover "$live proceeds unopposed"
else
  # --- stage 2: exchange ------------------------------------------------------
  turn=1
  while [ "$turn" -le "$TURNS" ]; do
    ev stage "exchange turn $turn"
    for who in opus sol; do
      compose "$PROMPTS/exchange.md" "$ROUND/turns/t$turn-$who.prompt.md" \
        "ME=$who" "PEER=$(peer_of "$who")" "TASK=$TASKFILE" "RULES=$RULES" \
        "WORKDIR=$ROOT" "TRANSCRIPT=@$TRANSCRIPT"
    done
    run_model opus "$ROUND/turns/t$turn-opus.prompt.md" "$ROUND/turns/t$turn-opus.raw" "t$turn-opus" read & fp=$!
    sleep "$STAGGER"
    run_model sol "$ROUND/turns/t$turn-sol.prompt.md" "$ROUND/turns/t$turn-sol.raw" "t$turn-sol" read & ap=$!
    wait $fp; wait $ap
    for who in opus sol; do
      jsonblock "$ROUND/turns/t$turn-$who.raw" > "$ROUND/turns/t$turn-$who.json" 2>/dev/null || \
        ev agent_degraded "$who turn $turn returned no JSON"
    done
    python3 "$SKILL/scripts/transcript.py" turn "$ROUND" "$turn" >> "$TRANSCRIPT"
    implementer=$(python3 "$SKILL/scripts/transcript.py" converged "$ROUND" "$turn")
    if [ -n "$implementer" ]; then ev converged "turn $turn on $implementer"; break; fi
    ev no_agreement "after turn $turn"
    turn=$(( turn + 1 ))
  done

  # --- stage 3: tiebreak ------------------------------------------------------
  if [ -z "$implementer" ]; then
    ev stage "tiebreak"
    compose "$PROMPTS/tiebreak.md" "$ROUND/tiebreak.prompt.md" "TASK=@$TASKFILE" "TRANSCRIPT=@$TRANSCRIPT"
    run_model opus "$ROUND/tiebreak.prompt.md" "$ROUND/tiebreak.raw" tiebreak read
    jsonblock "$ROUND/tiebreak.raw" > "$ROUND/tiebreak.json" 2>/dev/null || die "tiebreak produced nothing"
    implementer=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["implementer"])' "$ROUND/tiebreak.json")
    ev decision "tiebreak chose $implementer"
  fi
fi

python3 "$SKILL/scripts/transcript.py" plan "$ROUND" "$implementer" > "$ROUND/plan.md" || die "could not settle a plan"
ev decision "$implementer implements"

nq=$(python3 "$SKILL/scripts/transcript.py" unknowns "$ROUND" | tee "$ROUND/open-questions.txt" | grep -c . || true)
if [ "$nq" -gt 0 ]; then
  ev stopped "open questions: $nq"
  echo "Stopped — questions only the user can answer:"; sed 's/^/  - /' "$ROUND/open-questions.txt"
  exit 3
fi
rm -f "$ROUND/open-questions.txt"

# --- stage 4: implement -------------------------------------------------------
ev stage "implement"
compose "$PROMPTS/implement.md" "$ROUND/implement.prompt.md" \
  "BRIEF=$ROUND/plan.md" "RULES=$RULES" "WORKDIR=$ROOT"
run_model "$implementer" "$ROUND/implement.prompt.md" "$ROUND/implement.raw" implement write
jsonblock "$ROUND/implement.raw" > "$ROUND/implementation.json" 2>/dev/null || \
  ev agent_degraded "implementer returned no structured result"
ev agent_done "implement by $implementer"

# --- stage 5: cross review ----------------------------------------------------
reviewer=$(peer_of "$implementer")
ev stage "crossreview by $reviewer"
compose "$PROMPTS/crossreview.md" "$ROUND/crossreview.prompt.md" \
  "ME=$reviewer" "PEER=$implementer" "PLAN=$ROUND/plan.md" \
  "RESULT=$ROUND/implementation.json" "RULES=$RULES" "WORKDIR=$ROOT"
run_model "$reviewer" "$ROUND/crossreview.prompt.md" "$ROUND/crossreview.raw" crossreview read
jsonblock "$ROUND/crossreview.raw" > "$ROUND/crossreview.json" 2>/dev/null || \
  ev agent_degraded "$reviewer returned no structured review"

verdict=$(python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1])).get("verdict","accept"))
except Exception: print("accept")' "$ROUND/crossreview.json")
ev crossreview "$verdict"

# --- stage 6: revise ----------------------------------------------------------
if [ "$verdict" = revise ]; then
  ev stage "revise"
  compose "$PROMPTS/revise.md" "$ROUND/revise.prompt.md" \
    "PEER=$reviewer" "REVIEW=$ROUND/crossreview.json" "PLAN=$ROUND/plan.md" \
    "RULES=$RULES" "WORKDIR=$ROOT"
  run_model "$implementer" "$ROUND/revise.prompt.md" "$ROUND/revise.raw" revise write
  jsonblock "$ROUND/revise.raw" > "$ROUND/revision.json" 2>/dev/null || \
    ev agent_degraded "revision returned no structured result"
  ev agent_done "revised by $implementer"
fi

# --- stage 7: both read the whole change --------------------------------------
ev stage "final review"
python3 "$SKILL/scripts/transcript.py" history "$ROUND" > "$ROUND/history.md"
for who in opus sol; do
  compose "$PROMPTS/final-review.md" "$ROUND/final-$who.prompt.md" \
    "ME=$who" "PEER=$(peer_of "$who")" "TASK=$TASKFILE" "PLAN=$ROUND/plan.md" \
    "HISTORY=$ROUND/history.md" "RULES=$RULES" "WORKDIR=$ROOT"
done
run_model opus "$ROUND/final-opus.prompt.md" "$ROUND/final-opus.raw" final-opus read & fp=$!
sleep "$STAGGER"
run_model sol "$ROUND/final-sol.prompt.md" "$ROUND/final-sol.raw" final-sol read & ap=$!
wait $fp; wait $ap
for who in opus sol; do
  jsonblock "$ROUND/final-$who.raw" > "$ROUND/final-$who.json" 2>/dev/null || \
    ev agent_degraded "$who final review returned no JSON"
done

python3 "$SKILL/scripts/transcript.py" manifest "$ROUND" "$slug" "$round" "$implementer" "$live" "$verdict" \
  > "$ROUND/manifest.json" || die "manifest failed"
ev done "$(python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));print("ship="+str(d.get("ship")))' "$ROUND/manifest.json")"
cat "$ROUND/manifest.json"
exit 0
