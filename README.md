# zwinger

zwinger runs AI coding agents (Claude Code, Codex, pi, Gemini CLI) in **cages**: small Linux microVMs on your Mac. Each cage belongs to a **project**, a folder on your Mac.
An agent in a cage sees only the folders that you mount and reaches only the hosts that you allow. Your
subscription logins stay on the Mac: the cage's proxy adds them to requests to the provider, and the agent sees a
placeholder only.

## Install

Needs a Mac with Apple silicon and macOS 14 or newer.

1. Download the `.dmg` from the [latest release](https://github.com/driif/zwinger/releases/latest).
2. Drag zwinger to Applications.
3. The app is not signed with an Apple Developer ID yet. Open it the first time with a right-click on `zwinger.app`,
   then **Open**. On macOS 15 and later, open it once, then select **Open Anyway** in
   **System Settings > Privacy & Security**. Or run `xattr -dr com.apple.quarantine /Applications/zwinger.app`.

zwinger installs its VM runtime itself and updates itself. It needs no Homebrew, Docker or `~/.microsandbox`.

## First steps

The first start shows four steps:

1. zwinger checks the Mac (macOS version, virtualization, free disk space) and installs the VM runtime.
2. Create your first project: pick the project folder. The agent works in it as `/workspace`.
3. Add an agent: zwinger finds the agents that are installed on the Mac and adds one with your login.
4. Start the cage and open the agent with ⌘P, or a shell with ⌘T.

⌘K shows all actions. ⌘, opens Settings: agents on this Mac, agent runtimes, updates, storage.

## Screenshots

- First run: system checks
- Project overview with the map
- Agent terminal
- Network activity with a blocked host
- Task run room

## Security model

- Each cage is a Linux microVM with its own kernel (microsandbox, libkrun on Hypervisor.framework).
  See [ADR 0001](docs/adr/0001-shields-are-linux-microvms.md).
- Network: deny all, an allow list, or the public internet, per project. The app shows the hosts that the agent looks up and the ones that the cage blocks.
- Secrets and host logins are never in the VM. The proxy injects them for their hosts only.
  See [ADR 0002](docs/adr/0002-host-agent-login-is-proxy-injected.md).
- The app window has a strict Content Security Policy and minimal permissions. See [docs/security.md](docs/security.md).

## Build from source

Needs Rust (stable), Node.js 24 and pnpm.

```sh
pnpm install
pnpm tauri dev
```

The gates:

```sh
pnpm build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

How to make a release: [docs/release.md](docs/release.md).

## License

[Apache 2.0](LICENSE)
