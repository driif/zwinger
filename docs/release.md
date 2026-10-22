# Release

zwinger ships as an unsigned (ad-hoc signed) `.dmg` for Apple silicon on GitHub Releases. The app updates itself from
`https://github.com/driif/zwinger/releases/latest/download/latest.json`. Each update is signed with the minisign key
of the project; the app has the public key in `src-tauri/tauri.conf.json` and refuses an update with a bad signature.

## Cut a release

1. Set the same version in `package.json`, `src-tauri/Cargo.toml` and `src-tauri/tauri.conf.json`.
2. Commit, then tag and push:

   ```sh
   git tag v0.2.0
   git push origin main v0.2.0
   ```

3. The `Release` workflow (`.github/workflows/release.yml`) builds on a `macos-14` (arm64) runner and makes a
   **draft** release with the `.dmg`, the updater archive (`.app.tar.gz` and its `.sig`) and `latest.json`.
4. Check the draft, then publish it. Apps see the update only after you publish: `releases/latest` ignores drafts.

The workflow needs two repository secrets:

| secret | value |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | the private minisign key (the file content). It stays on the maintainer's Mac and in this secret, never in the repository. |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | its password |

## Build locally

```sh
pnpm install
export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/zwinger.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=...
pnpm tauri build
```

The output is in `src-tauri/target/release/bundle/`: `dmg/zwinger_<version>_aarch64.dmg` and
`macos/zwinger.app.tar.gz` with `zwinger.app.tar.gz.sig`.

## Check the runtime entitlement

The VM runtime (`msb` and `libkrunfw`) is inside the zwinger binary. At the first start, zwinger writes it to its
runtime home `~/.zwinger/microsandbox` (mode 0700). `msb` must keep the `com.apple.security.hypervisor` entitlement, or
no shield starts. After the first start of a new build, check it:

```sh
codesign -d --entitlements - ~/.zwinger/microsandbox/bin/msb
```

The output must contain `com.apple.security.hypervisor` with the value `true`. The bundle signing (`signingIdentity`)
does not change this file, because the file is not in the bundle.

## Runtime home

The runtime home is short because microsandbox puts the sockets of each VM below it, at
`<home>/run/sandboxes/<24 hex>/control.sock`, and a macOS socket path must be shorter than 104 bytes. With
`/Users/<name>/.zwinger/microsandbox`, the user name can have at most 22 bytes. A longer name fails the "VM folder" check
at start. Shields, preferences and agent runtimes stay in the app data folder
(`~/Library/Application Support/com.driif.zwinger`).

Earlier builds used `<app data>/microsandbox` or `~/.microsandbox`. At each start, zwinger stops and removes its own
VMs (`zwinger-*`) in these homes. A failure there is logged and does not block the start. **Settings > Storage** shows
the old homes with their size:

- `<app data>/microsandbox`: only zwinger used it. Remove deletes the folder.
- `~/.microsandbox`: other microsandbox tools can use it. Remove deletes only zwinger's VMs and its image
  (`node:22-slim`, if no other sandbox uses it), and keeps the folder and all other data.

If `MSB_PATH` is set (a developer runtime) and that pair does not work, the start fails with a message that names the
path. zwinger does not replace a developer runtime. A damaged runtime in the runtime home is installed again during the
same start.

## First start of an unsigned app

macOS blocks an app that is not signed with a Developer ID. Users open it once in one of these ways:

- In Finder, right-click `zwinger.app` in Applications, select **Open**, then select **Open** again.
- On macOS 15 and later: open the app once, then go to **System Settings > Privacy & Security** and select
  **Open Anyway**.
- In Terminal: `xattr -dr com.apple.quarantine /Applications/zwinger.app`

Updates that the app installs itself do not need this step again.

## Change to Developer ID signing later

1. Get a Developer ID Application certificate and export it as a `.p12` file.
2. Add the repository secrets `APPLE_CERTIFICATE` (the `.p12` in base64), `APPLE_CERTIFICATE_PASSWORD`,
   `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (an app-specific password) and `APPLE_TEAM_ID`.
3. In `.github/workflows/release.yml`, remove the `#` from the `APPLE_*` lines.
4. In `src-tauri/tauri.conf.json`, set `bundle.macOS.signingIdentity` to the identity name, or remove it so that
   `APPLE_SIGNING_IDENTITY` applies. tauri-action then signs and notarizes the app.
5. Do the entitlement check above again, then remove the "First start of an unsigned app" section from this file
   and from `README.md`.
