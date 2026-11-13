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

## One project at a time

A cage runs for one project at a time. If two projects use the same cage, the start of the second project is refused
while the VM runs for the first: "cage X runs for session Y; stop it first".

## What the cage sees

A cage start boots the VM with the project folder, the mounts and the [network policy](/network/). Nothing else from
the Mac is in the cage:

- no environment variables of the Mac
- no credentials
- no home folder of the Mac

Credentials and logins from the Mac are never in the VM. The proxy of the cage adds them to the requests to their
hosts only.

## Terminals in the cage

A VM shell (⌘⇧T) and an agent session (⌘P) run in the cage. A VM shell starts in `/workspace`. An agent session starts
in the harness folder of the project if it has one, else in `/workspace`. A Mac shell (⌘T) runs on the Mac, not in
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
