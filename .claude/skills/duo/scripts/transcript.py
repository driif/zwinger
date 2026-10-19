#!/usr/bin/env python3
"""Round bookkeeping for duo-round.sh: the shared transcript, convergence, the
settled plan, the open questions, and the manifest.

Kept out of the shell because every one of these reads JSON the models wrote,
and a model that omits a key must degrade rather than crash the round.
"""
import json
import pathlib
import sys

WHO = ("opus", "sol")


def load(path):
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except Exception:
        return None


def block(title, body):
    return f"### {title}\n\n{body}\n"


def cmd_init(round_dir):
    """The blind first drafts, as the opening of the transcript."""
    out = ["## Opening plans (written blind)\n"]
    for who in WHO:
        d = load(round_dir / "turns" / f"plan-{who}.json")
        if not d:
            out.append(block(f"{who} — no plan", "_This model did not produce a plan._"))
            continue
        out.append(block(
            f"{who} — opening plan",
            f"**Plan.** {d.get('plan','')}\n\n"
            f"**Claims implementer:** {d.get('claim','?')} — {d.get('claim_reason','')}\n\n"
            f"**Touches:** {', '.join(d.get('touches') or []) or 'none stated'}\n\n"
            f"**Risks:** {'; '.join(d.get('risks') or []) or 'none stated'}",
        ))
    return "\n".join(out)


def cmd_turn(round_dir, turn):
    out = [f"## Exchange — turn {turn}\n"]
    for who in WHO:
        d = load(round_dir / "turns" / f"t{turn}-{who}.json")
        if not d:
            out.append(block(f"{who} — no reply", "_This model did not reply this turn._"))
            continue
        conceded = "; ".join(d.get("conceded") or []) or "nothing"
        out.append(block(
            f"{who} — turn {turn}",
            f"{d.get('message','')}\n\n"
            f"**Implementer:** {d.get('implementer','?')} · "
            f"**Agreed:** {bool(d.get('agreed'))} · **Conceded:** {conceded}",
        ))
    return "\n".join(out)


def cmd_converged(round_dir, turn):
    """Both agreed, on the same name. Anything else is not convergence."""
    picks = []
    for who in WHO:
        d = load(round_dir / "turns" / f"t{turn}-{who}.json")
        if not d or not d.get("agreed"):
            return ""
        pick = d.get("implementer")
        if pick not in WHO:
            return ""
        picks.append(pick)
    return picks[0] if len(set(picks)) == 1 else ""


def _latest_plan(round_dir, who):
    """That model's most recent plan: last exchange turn first, else its draft."""
    turns = sorted(round_dir.glob(f"turns/t*-{who}.json"),
                   key=lambda p: int(p.name.split("-")[0][1:]), reverse=True)
    for path in turns:
        d = load(path)
        if d and d.get("plan"):
            return d["plan"]
    d = load(round_dir / "turns" / f"plan-{who}.json")
    return (d or {}).get("plan", "")


def cmd_plan(round_dir, implementer):
    """A tiebreak plan wins; otherwise the implementer's own latest."""
    tb = load(round_dir / "tiebreak.json")
    if tb and tb.get("plan"):
        return tb["plan"]
    plan = _latest_plan(round_dir, implementer)
    if not plan:
        raise SystemExit(1)
    return plan


def cmd_unknowns(round_dir):
    """Questions still open at the end, deduped, in first-seen order."""
    tb = load(round_dir / "tiebreak.json")
    if tb is not None:
        items = tb.get("open_questions") or []
    else:
        items = []
        for who in WHO:
            turns = sorted(round_dir.glob(f"turns/t*-{who}.json"),
                           key=lambda p: int(p.name.split("-")[0][1:]), reverse=True)
            src = load(turns[0]) if turns else load(round_dir / "turns" / f"plan-{who}.json")
            items += (src or {}).get("unknowns") or []
    seen, out = set(), []
    for q in items:
        q = str(q).strip()
        if q and q.lower() not in seen:
            seen.add(q.lower()); out.append(q)
    return "\n".join(out)


def cmd_history(round_dir):
    out = []
    cr = load(round_dir / "crossreview.json")
    if cr:
        lines = [f"**Verdict:** {cr.get('verdict','?')}", "", cr.get("message", "")]
        for f in cr.get("findings") or []:
            lines.append(f"- [{f.get('severity','?')}] {f.get('file','')}:{f.get('line','')} "
                         f"— {f.get('what','')}")
        out.append(block("Cross review", "\n".join(lines)))
    rv = load(round_dir / "revision.json")
    if rv:
        lines = [rv.get("message", ""), ""]
        for f in rv.get("fixed") or []:
            lines.append(f"- fixed: {f.get('finding','')} → {f.get('what_changed','')}")
        for f in rv.get("rebuttals") or []:
            lines.append(f"- declined: {f.get('finding','')} → {f.get('why_not','')}")
        out.append(block("Revision", "\n".join(lines)))
    return "\n".join(out) or "_No review or revision happened._"


def _backlog(round_dir):
    """Out-of-scope ideas from every stage, deduped, in first-seen order."""
    paths = sorted(round_dir.glob("turns/plan-*.json"))
    paths += sorted(round_dir.glob("turns/t*-*.json"),
                    key=lambda p: (int(p.name.split("-")[0][1:]), p.name))
    paths += [round_dir / "implementation.json"]
    seen, out = set(), []
    for path in paths:
        items = (load(path) or {}).get("backlog")
        if not isinstance(items, list):
            continue
        for item in items:
            item = str(item).strip()
            if item and item.lower() not in seen:
                seen.add(item.lower()); out.append(item)
    return out


def cmd_manifest(round_dir, slug, round_name, implementer, live, verdict):
    finals = {w: load(round_dir / f"final-{w}.json") for w in WHO}
    reported = [w for w, d in finals.items() if d]
    # Both must have read it. One reviewer saying ship on a degraded run is the
    # same mistake as reading an empty report as a clean one.
    ship = len(reported) == len(WHO) and all(finals[w].get("ship") for w in reported)
    blocking = [b for w in reported for b in (finals[w].get("blocking") or [])]
    return json.dumps({
        "task": slug,
        "round": round_name,
        "implementer": implementer,
        "reviewer": "sol" if implementer == "opus" else "opus",
        "planned": live.split(),
        "degraded": [w for w in WHO if w not in live.split()],
        "crossreview_verdict": verdict,
        "final_reviewers": reported,
        "ship": ship,
        "ship_basis": ("both reviewers" if len(reported) == len(WHO)
                       else f"withheld — only {', '.join(reported) or 'nobody'} reviewed"),
        "blocking": blocking,
        "task_met": {w: finals[w].get("task_met") for w in reported},
        "backlog": _backlog(round_dir),
    }, indent=2)


def main():
    cmd, round_dir = sys.argv[1], pathlib.Path(sys.argv[2])
    rest = sys.argv[3:]
    fn = {
        "init": lambda: cmd_init(round_dir),
        "turn": lambda: cmd_turn(round_dir, rest[0]),
        "converged": lambda: cmd_converged(round_dir, rest[0]),
        "plan": lambda: cmd_plan(round_dir, rest[0]),
        "unknowns": lambda: cmd_unknowns(round_dir),
        "history": lambda: cmd_history(round_dir),
        "manifest": lambda: cmd_manifest(round_dir, *rest),
    }[cmd]
    print(fn())


main()
