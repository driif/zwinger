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
- **Workspace**: a named group of sessions. It is only for organization.
- **Session**: one shell with a name, a working directory and its scrollback. It is one sidebar row.
- Default session name is the basename of its directory. A rename pins a custom name; an empty name reverts.
- A split is one session with two panes, not two sessions.
- A companion is a session pinned under a parent session. It moves and closes with the parent.

## Sidebar behaviour to copy
- Two levels: workspace rows, then session rows. Each row has a kind icon.
- Only session rows are selectable.
- Rename inline: double-click, or Rename in the menu. Enter commits, Escape cancels.
- Drag a session to a different workspace. It keeps running, with its scrollback.
- Reorder workspaces and sessions by drag.
- Drag folders from Finder onto the tree to open sessions there.
- A bottom bar adds a workspace or a session.

## What zwinger adds
- A cage (config: mounts, egress, credentials) attaches to a session or to a workspace.
  A session uses its own cage, else the cage of its workspace.
- Read `CONTEXT.md` for the zwinger terms. Where agterm and `CONTEXT.md` differ, `CONTEXT.md` wins.
