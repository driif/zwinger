# Security

The security model of the shields is in the ADRs:

- [0001 Shields are Linux microVMs](adr/0001-shields-are-linux-microvms.md)
- [0002 A host agent's subscription login is proxy-injected](adr/0002-host-agent-login-is-proxy-injected.md)

This file covers the app itself: the webview and its permissions.

## Content Security Policy

`app.security.csp` in `src-tauri/tauri.conf.json` applies to the release build. The webview loads only the files of
the app and talks only to the Rust backend. It loads nothing from the network.

| directive | value | for |
|---|---|---|
| `default-src` | `'self'` | All other resource types: only the files of the app bundle. |
| `script-src` | `'self'` | The app scripts. No inline script, no `eval` and no WebAssembly. |
| `style-src` | `'self' 'unsafe-inline'` | The app stylesheet, and the `<style>` elements of the terminal (xterm.js), which sets its colors and cell size in them. Tauri adds no nonce or hash to `style-src` (`dangerousDisableAssetCspModification`), because a nonce makes the browser ignore `'unsafe-inline'`. An inline style can change the look only; it cannot run code. |
| `img-src` | `'self' data:` | Images of the app and small images that the build inlines as `data:` URLs. |
| `font-src` | `'self' data:` | The fonts of motiv, also when the build inlines a small one as a `data:` URL. |
| `connect-src` | `ipc: http://ipc.localhost` | `ipc:` and `http://ipc.localhost` carry the Tauri commands, events and channels. |
| `object-src` | `'none'` | No plugins. |
| `base-uri` | `'none'` | No `<base>` element can change where relative URLs point. |
| `form-action` | `'none'` | No form submits to a URL. |
| `frame-ancestors` | `'none'` | No page can embed the app. |

`devCsp` is `null`: `pnpm tauri dev` loads from the Vite dev server, which needs inline styles and a WebSocket for
hot reload. Check a CSP change with `pnpm tauri build` and open the terminal, the shield map and a task run.

## Capabilities

`src-tauri/capabilities/default.json` gives the webview only:

| permission | for |
|---|---|
| `core:default` | Commands, events, channels, the app version and the window. |
| `opener:default` | Open `http(s)` links in the default browser, such as the links in About. |
| `dialog:allow-open` | The folder picker for a new shield or a mount. |

The updater and the restart are not webview permissions. The webview calls the app commands `check_update` and
`install_update`. Rust installs the update and restarts only when no shield runs, so a script in the webview cannot
restart the app while a shield works. There is no shell and no file system plugin: every file operation is an app
command that checks its paths.
