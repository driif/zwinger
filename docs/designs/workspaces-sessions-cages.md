# Workspaces, sessions, cage profiles and Keychain removal

* **Author:** driif
* **Date:** 2026-11-12

## Overview

The left sidebar becomes a tree like agterm: workspaces (named groups) that hold sessions (one host folder each). The user renames rows, reorders them by drag, and drags sessions between workspaces. A click on a session opens its terminal panes. A session can open a terminal, an agent or the IDE.

A cage profile is a saved config: mounts, network policy and credentials. A session uses its own profile, or the default profile of its workspace. The user starts and stops the cage from the session.

The macOS Keychain stops being used, because each unsigned build makes macOS ask for the password again. Credentials go to a 0600 file. The proxy CA is made new in memory at each app start. The Claude Code login uses a `claude setup-token` token. Apps (saved long-running commands) are removed.

Small separate items: the sidebar scrolls although its content fits; agent status glyphs (active, blocked, done); one text scale and density for all UI.

## Analysis

Seven review lenses (design, codebase accuracy, scope, naive reader, implementation readiness, structure, inventive) read the concept against the code and `CONTEXT.md`.

**Term collisions.** "Session" already means one terminal process in `CONTEXT.md` and in the code (`SessionKind`, `terminal::Session`). The new session is today's Project. "Cage profile" collides with Agent profile and the `profiles/` folder. "Workspace" collides with the guest path `/workspace`. The glossary must change before any model ticket.

**The project id is the key of everything.** It names the VM (`zwinger-<id>`), the credential accounts (`<id>/<name>`) and the folders `shields/`, `bin/`, `profiles/` and `tasks/`. ADR 0005 froze these keys. The safe migration turns each project into one session plus one cage profile, and the profile keeps the old project id. Then no data moves, and `projects.json` stays for rollback.

**One VM per profile is the riskiest part.**
- A new session in a running VM needs a hot bind mount (#43 covers only runtimes) or a restart. A restart ends the agents and runs of the other sessions.
- Isolation drops: an agent in session A can write the folder of session B, and the git check of a duo run does not see it.
- The reason "share logins" is mostly false. Host logins and credentials already stay on the Mac, and the proxy injects them into each VM.
- The inventive lens proposes the opposite: the profile is only a template, each session gets its own VM, and an idle VM stops itself. This removes the need for #43, for `/workspace/<slug>` and for the task visibility problem. The cost is RAM per running VM and the boot time.

**An agent on the Mac without a cage reverses ADR 0004.** "Agents run without permission prompts" is on by default, so this agent would get the bypass flags. It also runs as the user, so it can read the 0600 credentials file. If it is kept, it must get no bypass flags, no credentials and no proxy env, and it must show a warning on each launch. It needs its own ADR. The scope lens says cut it: the user can already type `claude` in a Mac shell.

**The guest path.** `/workspace/<session>` must use a stable slug, never the display name. About 57 code references assume that `/workspace` is the project folder. Agents keep history by folder, so `claude -c` and `codex resume` lose history if the path changes. A VM with one session keeps `/workspace`.

**Keychain removal.**
- The root cause is the unsigned build: "Always Allow" is lost with each new signature. A stable self-signed signing identity can stop the prompts by itself. Do it early and measure the result.
- If the app moves old values out of the Keychain, macOS asks one more time for each item. The startup sweep that deletes unused Keychain values must stop, or it keeps asking.
- Any process of the user can read a 0600 file. This is weaker than the Keychain. The docs must say so.
- The check that keeps the app data folder out of the cage (`check_app_data`) runs only at cage start. It must also run when the user adds a mount or a folder, and it must reject parent folders such as `~`.
- In-memory CA: safe, because all VMs stop when the app exits. But on some images the CA is appended to the guest CA bundle, so a new CA at each start piles up. Replace the old CA block instead of appending.
- setup-token: existing Claude host logins need a "needs token" state. The token has no refresh, so the expiry must come from the entry date (about one year).

**Drag is a security action.** A session that inherits its workspace profile gets new mounts, egress and credentials when it is dragged to a different workspace. The app must confirm this, or apply it at the next start, and show the cage on each row.

**Status glyphs.** The guest can reach the host only through the proxy port. A hook can print an OSC escape sequence into the terminal, and the app reads it next to OSC 7. With an agent profile on, `settings.json` is read-only in the guest, so hooks need a merge step or a separate `--settings` file.

**Reuse.** `@dnd-kit/core` and `@dnd-kit/sortable` are already dependencies, and `ProjectRail.tsx` already has sortable code. The `Vault` trait already has Keychain and memory implementations, so a `FileVault` fits.

## Goals vs. Non-Goals

* **In-Scope Goals:**
  * An agterm-style sidebar tree: workspaces, sessions, rename, drag to reorder and to move, default name from the folder basename.
  * Session actions: new terminal, open agent, open in IDE. A click opens the terminal panes.
  * Cage profiles as a saved list, attached to a session or inherited from the workspace.
  * No macOS password prompts in normal use: signing identity, file vault, in-memory CA, Claude setup-token.
  * Remove Apps.
  * Fix the sidebar scroll bug.
  * Agent status glyphs and one UI text scale (separate tickets).
* **Out-of-Scope Non-Goals:**
  * OpenBao (later, as a different `Vault`).
  * agterm features other than the tree: companions, flags, multiple windows, remote sessions, dashboard.
  * A chat UI of our own.

## Alternatives Weighed

* **Keep the Keychain and only sign builds.** Open: the signing ticket measures whether this is enough. The user wants a file vault first, with Keychain and OpenBao as later options.
* **Read the Claude Code login from the Mac Keychain.** Rejected by author: it is the password prompt the user wants to remove. On macOS, Claude Code has no login file.
* **Keep Apps.** Rejected by author: scripts and skills cover them.
* **Store the proxy CA key in the file vault.** Rejected by author: a new CA at each app start needs no storage.
* **Only one VM per session.** Rejected by author: a shared VM per cage saves RAM. One VM per session stays possible as a choice.
* **Cut the agent with no cage.** Rejected by author: the app can also be used as a terminal app, with a warning.
* **Migrate old Keychain values.** Rejected by author: the user enters them again.
* **Add a new drag-and-drop library.** Rejected: dnd-kit is already in the app.

## Key Decisions & Staging for Next Steps

**Decided by the user**
1. Workspaces are groups for organization only. Sessions have a path. The default name is the basename of the folder.
2. A cage is a saved config (mounts, network policy, credentials). A session attaches a cage, or inherits the cage of its workspace.
3. VMs: by default one shared VM per cage, for all its sessions. The user can start more VMs from the same cage, and give a session its own VM.
4. A session can open an agent with no cage. The app warns, and the agent runs on the Mac. Proposed guard for the ticket: no bypass flags and no credentials in that case.
5. Credentials go to a file first. Keychain and OpenBao come later as options. Old Keychain values are not migrated: the user enters them again.
6. The CA is made new at each app start and is not stored.
7. Apps are removed.
8. Tasks are under sessions.
9. Rename freely for a clean structure (dev mode): Workspace (group), Session (folder plus terminals; was Project), Terminal (one process; was Session), Cage (saved config), VM (a running microVM of a cage). The guest path `/workspace` stays.

**Still open (decide in the matching ticket)**
1. Credentials: global, or per cage.
2. Location of the credentials file.
3. Drag of a session while its VM runs: confirm, or apply at the next start.
4. A new session in a running shared VM: restart, or hot mount (#43).

**Proposed tickets, in order**

Independent:
1. The sidebar scrolls although its content fits.
2. A stable self-signed signing identity, with a measurement of the remaining prompts.
3. Remove Apps.

Keychain track:

4. Check the app data path when the user adds a mount or a folder.
5. In-memory proxy CA per app run.
6. File vault behind `Vault`, and removal of the Keychain worker.
7. Claude host login through `claude setup-token`.

Decisions:

8. Glossary and ADRs: terms, VM granularity, key ownership, guest path.

Model and sidebar track:

9. Workspace tree over the existing projects (rename, drag, actions). No cage change.
10. Data model split with no behaviour change, ids kept (10a backend, 10b frontend).
11. Cage profiles UI: attach, inherit, rules for drag and delete.
12. VM granularity, as the ADR decides.
13. #97, rescoped to the new key.
14. Tasks under sessions.
15. Agent status glyphs through hooks and OSC.
16. Agent on the Mac without a cage, only after its ADR.
17. UI audit: one text scale and density from motiv tokens.

**Backlog issues:** #109 to #125 (#97 rescoped by comment).
