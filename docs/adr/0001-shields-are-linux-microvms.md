---
status: accepted
---

# Shields are Linux microVMs, also on macOS

A shield runs its agents in a Linux microVM (microsandbox, libkrun with Hypervisor.framework on macOS), not in a macOS VM and not in a host sandbox. A Linux microVM boots in about a second, many can run at once, it has its own kernel, and the stack is open source. The user prefers open source and expects the ecosystem to move that way. The cost: native macOS agent binaries (Claude Code, Codex) cannot run in a shield. zwinger uses one cached Linux build of the same version instead, while JS agents (pi, Gemini CLI) run from the user's host install.

## Considered Options

- **macOS VM** (Virtualization.framework): runs the exact host binaries, but Apple allows at most two macOS VMs per Mac at a time, images are many GB, boot takes tens of seconds, and microsandbox does not support it.
- **Host sandbox** (macOS Seatbelt): instant and uses the host binaries and logins, but it shares the host kernel and its rules are hard to make tight. It is not the default. It can come later as an opt-in mode for agents that have no Linux build.
