# zwinger domain

**Shield**: the unit of isolation. One shield is one host folder, one microVM, its mounts and its network policy. The folder is mounted read-write at `/workspace` in the VM. Nothing else from the host is visible: no host env vars, no credentials, no host home.

**Mount**: an extra host folder that the user adds to a shield explicitly. It has an absolute guest path that does not overlap `/workspace`. It is read-only by default.

**Network policy**: egress is denied by default. The user adds allowed domains (the allow list); `*.example.com` allows subdomains. In allow-list mode DNS resolves only listed names. "Allow all" means the public internet only. The host machine, the LAN, loopback, link-local and cloud metadata addresses are blocked in every mode. Ingress is always denied. Mount and network changes apply on the next shield start.

**Session**: one interactive terminal inside a running shield's VM. Kinds: `shell` and `pi` (the agent). There are no host (unsandboxed) sessions.
