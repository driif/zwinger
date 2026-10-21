# zwinger domain

**Shield**: the unit of isolation. One shield is one host folder, one microVM, its mounts and its network policy. The folder is mounted read-write at `/workspace` in the VM. Nothing else from the host is visible: no host env vars, no credentials, no host home.

**Mount**: an extra host folder that the user adds to a shield explicitly. It has an absolute guest path that does not overlap `/workspace`. It is read-only by default.

**Network policy**: egress is denied by default. The user adds allowed domains (the allow list); `*.example.com` allows subdomains. In allow-list mode DNS resolves only listed names. "Allow all" means the public internet only. The host machine, the LAN, loopback, link-local and cloud metadata addresses are blocked in every mode. Ingress is always denied. Mount and network changes apply on the next shield start.

**Session**: one interactive terminal inside a running shield's VM. Kinds: `shell` and `pi` (pi in a terminal). There are no host (unsandboxed) sessions.

**Agent**: the pi process of a running shield that the Agents tab talks to (`pi --mode rpc`, no terminal). A shield has at most one agent. It starts in the harness, else in `/workspace`. It runs the pi that zwinger bundles, mounted read-only at `/opt/zwinger/pi`; the shield needs no registry access. A shield stop ends the agent.

**Chat**: one pi session file of a shield. The chats are in the app data dir (`shields/<id>/sessions`), mounted read-write with the rest of the pi state at `/opt/zwinger/state`. They stay after a stop, so an old chat can be opened again. A shield delete removes them. The guest can write these files, so the app reads them without following links.

**Secret**: a named credential for a shield, such as `OPENAI_API_KEY`, with a list of exact hosts (no wildcards) and the request places where it is used: named header fields (such as `authorization`), query and body. Only the named header fields get the value, so a host that echoes other headers cannot show it to the agent. The agent sees only a placeholder (`$MSB_<NAME>$`) in the variable. The network proxy replaces the placeholder with the value only in requests to the secret's hosts, after a TLS identity check. A host must also be reachable under the network policy. The value is in the macOS Keychain (service `zwinger`, account `<shield-id>/<name>`), never in `shields.json`, and the UI can write it but not read it. At app start, Keychain values that no configured secret uses are deleted. While the shield runs, the value is also in the microsandbox run config of the VM. After a crash it can stay there until the next app start reaps the VM. A value change applies on the next shield start.
