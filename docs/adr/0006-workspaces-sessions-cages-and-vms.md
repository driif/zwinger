---
status: accepted
---

# Workspaces, sessions, cages and VMs

A project joined three things: a host folder with its terminals, a sandbox config, and the microVM. The sidebar groups folders like agterm, and one sandbox config must serve more than one folder. So the model has five names: a **workspace** groups sessions, a **session** is a host folder with its terminals and pane layout (was project), a **terminal** is one process (was session), a **cage** is a saved config (mounts, network policy, credentials, host agents, agent profiles), and a **VM** is a running microVM of a cage. By default all sessions of a cage share one VM; the user can start more VMs from the same cage. The guest path `/workspace` stays. This supersedes the names of ADR 0005.

## Key owners

The migration turns each project into one session and one cage, and both keep the old project id. Session ids and cage ids are separate id spaces. So no stored data moves, and `projects.json` stays for rollback.

| Key | Owner | Value |
| --- | --- | --- |
| VM name | VM | `zwinger-<VM key>` |
| Agent state `shields/<…>` | VM | `<VM key>` |
| Wrappers `bin/<…>` | VM | `<VM key>` |
| Profile snapshots `profiles/<…>/<agent>` | VM | `<VM key>` |
| Credential accounts `<…>/<name>` | cage | `<cage id>` |
| Run records `tasks/<…>/<slug>/<run>` | session | `<session id>` |

The **VM key** of the shared VM of a cage is the cage id, so the VM of a migrated project keeps its name and its state. Each further VM of the cage gets `<cage id>-<n>`, with the lowest free `n`, and keeps it while it exists. Each VM writes its own wrappers and profile snapshots, so two VMs of one cage never race on one folder.

## Considered Options

- **One VM per session, the cage only a template.** Best isolation, and no `/workspace/<slug>` or hot mount. Rejected by the user: a shared VM per cage saves RAM. It stays possible as a choice (a session gets its own VM).
- **One VM per cage, never more.** Simple keys, but the user cannot isolate one session from the others of the cage.
- **New ids for the migration.** Clean, but each project would need a move of its VM, state folders and credentials.

## Consequences

- Sessions in one VM can read and write each other's folders, and the git check of a run sees only its own session folder. The docs and the run say so.
- A new session in a running shared VM needs a restart or a hot mount (#120).
- A session that inherits the cage of its workspace changes its mounts, egress and credentials when it moves to a different workspace (#119).
- Whether credentials are per cage or global is open (#114). Until then the cage owns them.
