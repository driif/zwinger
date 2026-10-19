# CONTEXT.md Format

```md
# {Context Name}

{One or two sentences: what this context is and why it exists.}

## Language

**Invoice**:
A request for payment sent to a customer after delivery.
_Avoid_: Bill, payment request

**Customer**:
A person or organization that places orders.
_Avoid_: Client, buyer, account
```

## Rules

- Use one word per concept. List alternatives under `_Avoid_`.
- Define each term in one or two sentences: what it IS, not what it does.
- Add only project-specific terms, not general programming concepts (timeouts, error types, utilities).
- Use subheadings for clear groups.

## Several contexts

A root `CONTEXT-MAP.md` lists contexts, locations and relationships:

```md
# Context Map

## Contexts

- [UI](./src/CONTEXT.md) — screens and user workflows
- [Core](./src-tauri/CONTEXT.md) — filesystem, processes, persistence

## Relationships

- **UI → Core**: UI calls Tauri commands; Core emits events the UI subscribes to
```

- `CONTEXT-MAP.md` exists: read it.
- Root `CONTEXT.md` only: one context.
- Neither: make a root `CONTEXT.md` at the first resolved term.

With several contexts, find the topic's context. If unclear, ask.
