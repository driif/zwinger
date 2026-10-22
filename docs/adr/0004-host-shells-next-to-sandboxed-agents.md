---
status: accepted
---

# Host shells next to sandboxed agents

A shield's terminals can now run on the Mac: a host shell is the user's login shell, unsandboxed, in the shield's host folder, and it is the default kind of a new terminal. A VM shell stays as an explicit choice, and agents, apps and task runs still run only in the VM. The user manages files and runs everyday commands on their own computer; the shield isolates only the agents that run with dangerous permissions. A host shell needs no shield start, so it opens at once. The cost: zwinger now starts unsandboxed processes. The UI must show clearly which terminals are on the Mac and which are in the VM, and zwinger never starts an agent in a host shell for the user.

## Considered Options

- **VM only** (until now): every terminal pays the VM start time, and the user cannot use host tools, host credentials or host config in the app's terminal.
- **Host only**: removes the VM shell, which is needed to inspect what an agent sees.
