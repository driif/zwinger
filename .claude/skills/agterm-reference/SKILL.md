---
name: agterm-reference
description: Use when you design or build the zwinger sidebar, workspaces, sessions, drag and drop, rename or session layout, and agterm is the model for it.
---

# agterm reference

agterm is the model for the zwinger sidebar. Copy its behaviour, not its code (Swift, AppKit).

## Sources
- Local clone: `~/Documents/dev/my/agterm`. If it is missing, use https://github.com/umputun/agterm.
- User guide: `site/docs.html`, section "Workspaces & sessions".
- Sidebar internals: `ARCHITECTURE.md`, section "Sidebar (NSOutlineView)".
- Commands: `site/commands.html`.

## The model
agterm and zwinger use the same words for different things:

| agterm | zwinger |
| --- | --- |
| Workspace: a named group of sessions | Workspace: the same |
| Session: one shell, one sidebar row | Terminal (one process); the zwinger session is one level up |
| (none) | Session: one host folder with its terminals and pane layout; one sidebar row |
| A split: one session, two panes | Pane layout of one session |

- A zwinger session row is like an agterm session row: default name from the folder basename, a rename pins a custom name, an empty name reverts.
- A zwinger session holds many terminals. Split panes show terminals of one session.
- A companion is a session pinned under a parent session. zwinger has no companions.

## Sidebar behaviour to copy
- Two levels: workspace rows, then session rows. Each row has a kind icon.
- Only session rows are selectable.
- Rename inline: double-click, or Rename in the menu. Enter commits, Escape cancels.
- Drag a session to a different workspace. Its terminals keep running, with their scrollback.
- Reorder workspaces and sessions by drag.
- Drag folders from Finder onto the tree to open sessions there.
- A bottom bar adds a workspace or a session.

## What zwinger adds
- A cage (a saved config: mounts, network policy, credentials) attaches to a session or to a workspace.
  A session uses its own cage, else the cage of its workspace, else no cage.
- A VM is a running microVM of a cage. By default the sessions of a cage share one VM (ADR 0006).
- Read `CONTEXT.md` for the zwinger terms. Where agterm and `CONTEXT.md` differ, `CONTEXT.md` wins.
