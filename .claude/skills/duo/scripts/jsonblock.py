#!/usr/bin/env python3
"""Print the last JSON object in a model's reply, fenced or bare. Exit 1 if none.

Models sometimes put an unescaped double quote inside a string value. If a
block does not parse, the quote that ended the string too early is escaped
and the parse runs again.
"""
import json, os, re, subprocess, sys


def loads(text):
    for _ in range(200):
        try:
            return json.loads(text)
        except json.JSONDecodeError as e:
            if not e.msg.startswith(("Expecting ',' delimiter", "Expecting ':' delimiter",
                                     "Expecting property name", "Extra data")):
                raise
            # The last unescaped quote before the error closed the string too early.
            q = e.pos - 1
            while q >= 0 and not (text[q] == '"' and (q == 0 or text[q - 1] != "\\")):
                q -= 1
            if q <= 0:
                raise
            text = text[:q] + '\\"' + text[q + 1:]
    raise ValueError("too many repairs")


def model_repair(block):
    """Last resort: a small model fixes the JSON syntax and changes no content."""
    if os.environ.get("DUO_JSON_REPAIR", "1") == "0":
        return None
    prompt = ("The text below is meant to be one JSON object, but it is not valid JSON "
              "(probably unescaped double quotes inside string values). Return only the "
              "corrected JSON in a ```json fenced block. Change no words, keys or values; "
              "only fix the escaping.\n\n" + block)
    try:
        out = subprocess.run(["claude", "-p", "--model", "haiku"], input=prompt, text=True,
                             capture_output=True, timeout=180).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    for fixed in reversed(re.findall(r"```(?:json)?\s*\n(.*?)```", out, re.S)):
        try:
            return json.loads(fixed)
        except ValueError:
            continue
    return None


text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
blocks = re.findall(r"```(?:json)?\s*\n(.*?)```", text, re.S)
for block in reversed(blocks):
    try:
        print(json.dumps(loads(block), indent=2)); sys.exit(0)
    except ValueError:
        continue
if blocks:
    fixed = model_repair(blocks[-1])
    if fixed is not None:
        print("jsonblock: repaired by a model", file=sys.stderr)
        print(json.dumps(fixed, indent=2)); sys.exit(0)
# Unfenced: walk back from each closing brace to the matching opener.
for end in range(len(text) - 1, -1, -1):
    if text[end] != "}":
        continue
    depth = 0
    for start in range(end, -1, -1):
        if text[start] == "}": depth += 1
        elif text[start] == "{":
            depth -= 1
            if depth == 0:
                try:
                    print(json.dumps(json.loads(text[start:end + 1]), indent=2)); sys.exit(0)
                except ValueError:
                    break
sys.exit(1)
