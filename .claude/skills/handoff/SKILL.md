---
name: handoff
description: Use to write a short handoff document from the current conversation. Another agent uses the document to continue the work.
argument-hint: "What will the next session be used for?"
disable-model-invocation: true
---

Write a handoff document. A new agent must be able to continue the work with it.
Save the document in the scratchpad directory of this session, not in the repo tree.
Give the absolute path of the document.

Record the information that the next agent can easily lose:

- **Location of the work**: the branch, the changed files and areas (`src/`, `src-tauri/`), and changes to the Tauri IPC contract (command names, arguments, events).
- **Status**: the completed work, the next work and the blocked work.
- **Recommended skills** for the next agent, for example `/grilling`, `/domain-modeling`, `/codebase-design`.

Refer to existing artifacts (plans, ADRs, `CONTEXT.md`, commits, diffs) by path or URL. Do not copy their content.

Remove secrets: tokens, passwords, signing keys and personal data (PII).

If the user gives arguments, they are the focus of the next session. Write the document for that focus.
