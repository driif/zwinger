---
title: Cages
description: A cage is the Linux microVM of a project.
---

A **cage** is the microVM of a [project](/projects/). It is the unit of isolation. Each cage is a Linux microVM with
its own kernel (microsandbox, libkrun on Hypervisor.framework).

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

You start and stop the cage. A start shows its phases: vault, prepare, proxy, boot, guest, wrappers and apps. The
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
