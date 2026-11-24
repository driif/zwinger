# zwinger roadmap

zwinger is a Mac app that runs AI coding agents in Linux microVMs (cages), next to the user's own terminals.
The terms are in `CONTEXT.md`. The decisions are in `docs/adr/` and `docs/designs/`. They win over this page.

## Done

- Cages: microVM, mounts, egress through the app proxy, credential injection (placeholders only), network Ask/Allow/Block.
- Workspaces → sessions sidebar like agterm; terminals as tabs on the session page; pane layout.
- Agents (Claude Code, Codex, pi, Gemini CLI, Antigravity) as their own TUIs in a VM, host logins, agent profiles, no restart for a new agent.
- Task runs with two agents and the run room; Git commit/PR after approval; A2A share.
- Themes, zoom, density; docs site.

## Next

- Production: signed and notarized release, auto-update, adaptive icon (#98), onboarding polish.
- Harness folder for skills and plugins (#12). Published app ports (#32).
- Open work is in GitHub issues: label `bug` first, then the promoted `backlog` issues.
