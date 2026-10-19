#!/usr/bin/env python3
"""Print the last JSON object in a model's reply, fenced or bare. Exit 1 if none."""
import json, re, sys

text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
for block in reversed(re.findall(r"```(?:json)?\s*\n(.*?)```", text, re.S)):
    try:
        print(json.dumps(json.loads(block), indent=2)); sys.exit(0)
    except ValueError:
        continue
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
