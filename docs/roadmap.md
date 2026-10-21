# zwinger roadmap

zwinger is a desktop app that puts a **shield** over AI agents. It is not a terminal. It sits between the Zed IDE and the Claude / ChatGPT desktop apps: a clear GUI to configure, see and run agents that work with dangerous permissions inside a microVM.

Flow: install the app → create a shield on a folder → mount more folders → add skills / a harness → start agents → they do anything they want, but only inside the shield.

## Concepts

- **Shield**: a microVM (microsandbox) over one host folder (`/workspace`). Can hold many service folders. Egress denied by default.
- **Mount**: an extra host folder, ro or rw, at an explicit guest path.
- **Harness**: a folder for the agents (skills, plugins, instructions, task notes), separate from the code. Agents start in the harness and know where the code is. Like umputun's ralphex / revmux run outside the repo.
- **Network policy**: allow-listed hosts. The app sees the shield's requests and can inject credentials (bearer tokens) per host. The agent only sees placeholders, never host tokens.
- **App**: a saved long-running command in a shield (for example a dev server), started and stopped from the shield tree.
- **Agent**: pi, Claude Code, Codex, Gemini. Each runs as its own TUI in a shield terminal (`claude`, `codex`, `pi`, `gemini` are on PATH in the VM); zwinger does not reimplement agent UIs. Several agents can work on one task together (like the `duo` skill) — headless, shown in the run room.

## Milestones

| # | milestone | content |
|---|---|---|
| M0 | shield foundation | Backend: shield config, microVM, mounts, default-deny egress, PTY sessions. Done. |
| M1 | app shell + shield map | Zed/Claude-app layout. Shield list on the left. Shield home with a visual **shield map** (boundary, folders, harness, mounts, hosts). Tabs: Overview, Agents, Terminal, Network, Settings. ⌘K command menu. |
| M2 | secrets + credential injection | Secrets in the macOS Keychain. Per-host injection with microsandbox secret placeholders (TLS proxy). UI in the Network tab and on the map. |
| M3 | agent chat (pi) | pi in RPC mode inside the shield. Chat UI: streaming, tool calls, abort, model choice, sessions. pi available in the VM without per-start npx. |
| M4 | network activity | Live request log per shield (allowed / denied / injected). One-click allow of a denied host. |
| M4b | canvas + shield tree | Shield map on a draggable canvas (React Flow) with saved layout; drop folders from Finder to mount. Left rail as a tree: shield → Agents / Terminals / Apps, reorder by drag (dnd-kit). **Apps**: saved long-running commands in a shield. |
| M5a | more agents | Claude Code and Codex next to pi (download on demand, mounted read-only). |
| M5b | task runs + run room | Agents plan, implement and review together (duo pattern) inside a shield. **Run room**: live timeline of agent messages, stage bar, each agent's position, disagreements and concessions, verdicts; the user steps in. Also watches external duo round folders. A2A later. |
| M5c | host agents | Use the agents installed on the Mac (Claude Code, Codex, pi, Gemini): JS agents mounted from the host install, native ones as one cached Linux build of the same version; the user's agent profile mounted read-only; subscription login reuse, proxy-injected (ADR 0002). ADR 0001. |
| M5d | native agent terminals | Agent commands on PATH in the VM (wrappers with the shield's state and host login); the Agents group opens the agent's own TUI in a terminal; the custom chat UI is removed. Then a backlog cleanup. |
| M6 | production | Onboarding, settings, error and empty states, app icon, release build, signing and notarization, auto-update, CI. |
