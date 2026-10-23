---
title: Get started
description: Install zwinger, create a project and open an agent.
---

## Requirements

- A Mac with Apple silicon.
- macOS 14 or newer.

zwinger installs its VM runtime itself and updates itself. It does not need Homebrew, Docker or `~/.microsandbox`.

## Install

1. Download the `.dmg` from the [latest release](https://github.com/driif/zwinger/releases/latest).
2. Drag zwinger to Applications.
3. Open the app the first time. The app does not have an Apple Developer ID signature yet, so macOS blocks it. Use one
   of these procedures:
   - In Finder, right-click `zwinger.app` in Applications, select **Open**, then select **Open** again.
   - On macOS 15 and later: open the app once, then go to **System Settings > Privacy & Security** and select
     **Open Anyway**.
   - In Terminal: `xattr -dr com.apple.quarantine /Applications/zwinger.app`

Updates that the app installs itself do not need this step again.

## First steps

The first start shows five steps:

1. **Check this Mac**: zwinger checks the Mac (macOS version, virtualization, free disk space) and installs the VM
   runtime.
2. **Create a project**: select or drop the project folder. The agent works in it as `/workspace`. See
   [Projects](/projects/).
3. **Add an agent**: optional. Add an [agent](/agents/) that zwinger finds on the Mac, with your login. pi is built in.
4. **Your editor**: the default editor for **Open in editor**.
5. **Done**: start the [cage](/cages/), then press ⌘P for an agent or ⌘T for a shell.

Steps 2 to 4 have **Skip**.

## Two kinds of terminal

zwinger has terminals on the Mac and terminals in the cage. Know the difference:

- A **Mac shell** (⌘T) runs your login shell on the Mac, in the project folder. It is not in the cage and has no
  sandbox. It does not need a cage start.
- A **VM shell** (⌘⇧T) and an **agent session** (⌘P) run in the cage. They see only the mounts and reach only the
  allowed hosts. A VM shell starts in `/workspace`. An agent session starts in the harness folder of the project if it
  has one, else in `/workspace`.

zwinger never starts an agent in a Mac shell.

## Next

- ⌘K shows all actions. See [Shortcuts](/shortcuts/).
- ⌘, opens Settings: agents on this Mac, runtimes, appearance, terminal, editors, updates, storage.
