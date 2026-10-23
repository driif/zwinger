---
title: Projects
description: A project is a folder on your Mac and its configuration.
---

A **project** is one folder on your Mac and its configuration: the mounts, the network policy, the terminals, the
apps, the tasks and the agents from your Mac. Each project has one [cage](/cages/). The project folder is mounted
read-write at `/workspace` in the cage.

You can create, rename and delete projects. Use ⌘N to create a project.

## Mounts

A mount is an extra folder on your Mac that you add to a project. It has an absolute path in the cage. This path must
not overlap `/workspace`. A mount is read-only by default.

zwinger keeps the identity of the project folder and of each mount. Each cage start checks them. If a folder changed
(for example, another folder now has the same path), zwinger does not start the cage. If a folder was missing, you
must confirm it on the project page before the next start.

## Terminals

A project has three kinds of terminal:

| Kind | Where it runs | Folder |
|---|---|---|
| Mac shell | On the Mac, with no sandbox. It does not need a cage start. | The project folder |
| VM shell | In the cage | `/workspace` |
| Agent session | In the cage | The [harness folder](/agents/), else `/workspace` |

The tree shows all terminals of a project in one Terminals group. Drag a terminal to change the order. zwinger saves
the order.

- A terminal that has no process is **dormant**. This occurs after a cage stop or an app restart. **Open** starts a
  new process in the same place.
- A cage stop does not end a Mac shell. A project delete and the app exit end it.
- Only **Close** in the tree removes a terminal and ends its process.

zwinger keeps the last 1 MiB of output of each terminal. When you show the terminal again, for example after a window
reload, you see that output.

## Panes

The terminal view of a project can show terminals side by side. A split goes right (⌘D) or down (⌘⇧D) and opens a Mac
shell in the same folder. Any kinds of terminal can be in the same layout. zwinger saves the layout per project and
restores it after a restart.

When you close a pane, the terminal is hidden but continues to run. To end its process, use **Close** in the tree.

## Editors

zwinger finds the editors on your Mac (Zed, VS Code, Cursor and others) in `/Applications`, `~/Applications` and
`/Applications/Setapp`. Turn editors on and select a default in **Settings > Editors**.

**Open in editor** (⌘⇧E) opens the project folder, or the current folder of the current terminal. In a VM shell, a
folder below `/workspace` or below a mount opens as its folder on the Mac. Other folders open the project folder.

## Apps

An app is a saved, long-running command of a project, for example a dev server. It has a name, a command (one shell
line), a folder (`/workspace` or a folder below it or below a mount) and an autostart setting.

- **Start** runs the command in the cage. **Stop** or a cage stop ends it.
- An autostart app starts after each cage start.
- zwinger does not publish a port of an app to the Mac.
- A change to an app applies immediately.

## Map

The project overview (⌘1) shows a map of the project: the folders, the mounts, the hosts and the agent profiles. You
can drag the nodes. zwinger saves their positions. **Reset layout** clears them.
