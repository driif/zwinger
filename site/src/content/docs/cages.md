---
title: Cages
description: A cage is the Linux microVM of a project.
---

A **cage** is the sandbox config of a [project](/projects/): its mounts, its [network policy](/network/), its
credentials and its agents. A cage starts as a Linux microVM with its own kernel (microsandbox, libkrun on
Hypervisor.framework). The VM is the unit of isolation.

## The cage list

**Cages** at the bottom of the rail shows all cages, with the projects that use each one.

- **Create cage** adds a cage with no mounts, rules or agents.
- **Rename** changes the name of the cage. The projects keep their names.
- **Edit** shows the mounts and the agent settings of the cage.
- **Delete** removes the cage, its credential values and its agent state. You cannot delete a cage that a project uses,
  that is the default cage of a workspace, or whose VM runs. The button then shows the reason as a tooltip.

## Attach or inherit

In **Settings** of a project, **Cage** selects the cage of the project. **Inherit from workspace** uses the default
cage of its workspace. Set the default cage of a workspace with **Default cage…** in the menu of the workspace. A
project that inherits, in a workspace with no default cage, has no cage: it opens Mac shells only.

A change of the cage applies at the next start. A VM that runs keeps running with its old cage until it stops. The
project page then shows "The VM runs cage X; restart to use Y". If the project has no cage now, the page still shows
**Stop cage** for the old VM.

Edits always go to the current cage of the project: mounts, network rules, offline, logins, agents and settings. A VM
that runs with an old cage does not get these edits: they apply after the restart. The VM status, **Start** and **Stop**, the network test and
the network activity apply to the VM that runs for the project.

## Projects in one VM

All projects of a cage share its VM. The projects in one VM can read and write each other's folders: put a project
in its own cage if it must not see the others.

- **Path rule.** A VM with one project mounts its folder at `/workspace`. A VM with more projects mounts each folder
  at `/workspace/<slug>`. The slug is the folder name in kebab case. It is fixed when you create the project: a
  rename does not change it. A harness folder or a task working dir below `/workspace` moves with its project, so
  `/workspace/src` becomes `/workspace/<slug>/src`.
- **Add a project to a running VM.** A start of a project whose cage runs for other projects asks first: "Restart
  cage X to add Y?" After the restart, the VM terminals of the other projects open again. Their running tasks are
  cancelled. A terminal that does not open again stays closed, and a notice names it. If the VM had one
  project, its folder moves from `/workspace` to `/workspace/<slug>`, and its agents do not find their history of the
  old path. If the VM or its projects changed while the question was open, the restart is refused: try again.
- **Stop.** A stop of a VM with other projects names them and asks first. The stop ends the VM for all of them. If
  the VM or its projects changed while the question was open, the stop is refused: try again.
- **Restart after a cage edit.** **Restart** on the project page restarts the VM with all its projects. It asks first
  when the VM has other projects. Their VM terminals open again, and their running tasks are cancelled. After a
  change of the project's cage, **Restart** stops the old VM and starts the new cage for this project only.
- **Checks.** The start checks each project folder: it must not overlap a mount or a different project folder of the
  VM, it must be the same folder that you added, and it must not overlap the zwinger data folder.
- **Hot mount.** A project cannot join a running VM with no restart yet.

A start of a stopped VM boots it with only the project that starts it. A project that you delete must not run in a
VM with other projects: stop that VM first.

The project row in the sidebar shows the cage icon with the number of projects of a shared VM. Its tooltip names
them.

## What the cage sees

A cage start boots the VM with the project folders, the mounts and the [network policy](/network/). Nothing else from
the Mac is in the cage:

- no environment variables of the Mac
- no credentials
- no home folder of the Mac

Credentials and logins from the Mac are never in the VM. The proxy of the cage adds them to the requests to their
hosts only.

## Terminals in the cage

A VM shell (⌘⇧T) and an agent session (⌘P) run in the cage. A VM shell starts in the project's folder in the VM
(`/workspace`, or `/workspace/<slug>` in a shared VM). An agent session starts in the harness folder of the project if
it has one, else in that folder. A Mac shell (⌘T) runs on the Mac, not in
the cage, and has no sandbox. A cage stop ends the VM shells and the agent sessions, but not the Mac shells.

## Start and stop

You start and stop the cage of a project. A start shows its phases: vault, prepare, proxy, boot, guest and wrappers. The
status of a starting cage names the current phase. **Settings > Last start** of the project shows the time of each
phase.

A start mounts the [agent runtimes](/agents/#runtimes) that are installed at that time. If you install a runtime
later, restart the cage.

## Storage

zwinger keeps its VM runtime in `~/.zwinger/microsandbox`. Your projects, preferences and agent runtimes are in
`~/Library/Application Support/com.driif.zwinger`.

The path of the runtime folder must be short, because macOS limits the length of a socket path. If your user name is
longer than 22 bytes, the "VM folder" check fails at start.

Earlier builds of zwinger used other folders. **Settings > Storage** shows them with their size and can remove the data
of zwinger from them.
