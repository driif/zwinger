---
status: accepted
---

# Projects and cages

The word "shield" named two things: the folder with its config, and the microVM. Now a **project** is the folder and its config (mounts, network policy, terminals, apps, tasks, host agents), and its **cage** is the microVM that runs its agents. The user creates, renames and deletes a project, and starts and stops its cage. The code uses the same names: `Project` and `ProjectView` for the config, `start_cage`, `stop_cage`, the event `cage-start-progress` and the status type (`Status` in the Rust `cage` module, `CageStatus` in the frontend) for the VM, and the Rust module `cage` for both, because its manager joins the config and the VMs. The app icon is a cage with a Z.

## Consequences

- Stored data stays where it is. The first start copies `shields.json` to `projects.json` once, when `projects.json` is missing, and keeps the old file, so an older build still finds its data. The agent state stays in `<app data>/shields/<id>` (`LEGACY_STATE_DIR`): a move would need a migration of guest-writable folders.
- Project ids, VM names (`zwinger-<id>`), Keychain accounts (`<id>/<name>`), saved map node ids and the runtime home do not change.
- The older ADRs and task records keep the word "shield".
