---
title: Network
description: Control the hosts that the agent can reach, and give it credentials.
---

All network traffic of a cage goes through the proxy of its project. The VM can reach only the proxy. Connections from
the outside into the cage are always blocked.

## Network policy

The network policy belongs to the cage of a project: the projects of one cage share it. Open it with ⌘4.

- **Default**: Block or Allow all hosts that no rule names.
- **Rules**: each rule names a service of the catalog (for example GitHub or npm, with its domains) or a host, and says
  Allow or Block. A host is `a.io`, or `*.a.io` for the domain and its subdomains.
- **Offline**: blocks all requests now and keeps the rules.

The proxy checks each request in this sequence:

1. Offline.
2. Addresses that are always blocked: this Mac, the local network, loopback, link-local ranges and cloud metadata.
3. Block rules.
4. Allow rules.
5. The default.

A Block rule always has priority over an Allow rule. A rule change applies to the next request, with no restart.

The rules that the network page edits are the rules of the project's current cage. **Test** and **Activity** apply to
the VM that runs for the project. After a cage change, that VM uses its old cage until a restart.

## Activity

The network page groups the requests by host. The newest host is first. For each host it shows:

- the time, allowed or blocked, and the number of requests.
- the latest request: method, path, status, reason and note. If a tool does not use the proxy, the reason is "direct
  connection blocked — this tool ignores the proxy".
- the credentials that the proxy added.
- **Allow** or **Block** for the host, its wildcard or its service.

The log keeps the names of the query parameters, never their values, headers or bodies.

## Credentials

A credential is a named value, for example `GITHUB_TOKEN`, that an Allow rule gives to its hosts. The agent sees only
a placeholder. The proxy puts the real value into the request.

- The value is in the vault file, never in the project file. The app can write the value but cannot show it.
- The proxy adds a credential only to HTTPS requests.
- A credential on a Block rule is inactive. The proxy does not use it until the rule allows the host again.
- A wildcard host rule (`*.a.io`) cannot have a credential.
- A change of the value applies to the next request.
- A Claude Code setup token is stored as `CLAUDE_CODE_OAUTH_TOKEN` with its save date. Its estimated expiry is 365 days after save. The proxy uses it only for `api.anthropic.com` through an exact-host Allow rule. Under default Block, the project offers an **Allow api.anthropic.com** action. Block rules take priority over Allow rules.
- Claude setup tokens have no refresh host. Claude cages no longer block `console.anthropic.com` or `platform.claude.com` for this login.

The vault file is `~/Library/Application Support/com.driif.zwinger/vault/credentials.json`. It holds the values of all
cages. The file has mode 0600 and its folder has mode 0700. The app replaces the file in one atomic step at each
change. If the file or the folder has a wider mode or is a symlink, the app refuses to use the vault and gives the
`chmod` command that fixes it.

:::caution
Any process that runs as your macOS user can read the vault file. The file protects the values from other users and
from the cages, not from other programs of your user.
:::

Older versions kept the values in the macOS Keychain. The app does not move them to the vault file. A credential from
before shows **Not set**: enter its value again. A cage with such a credential does not start until you do.

:::caution
Do not give a credential to a host that sends requests back, for example `httpbin.org`, `postman-echo.com` or an
`echo.*` host. The app shows a warning.
:::

The proxy uses its own certificate authority (CA) for HTTPS. Each app start makes a new CA. Its private key stays
only in the memory of the app: no file and no Keychain item holds it. The app writes only the CA certificate to disk,
and logs its SHA-256 fingerprint once (`proxy CA sha256 …`). Each cage start puts the current CA in the trust store
of the cage and removes the CA of an earlier app start.

**Settings > About** shows the SHA-256 fingerprint of the current CA. To compare it with the CA in a cage, run this
command in the cage. It uses only Node, so it works in the default image (`node:22-slim`):

```sh
node -e "const fs = require('node:fs'), { X509Certificate } = require('node:crypto'); console.log(new X509Certificate(fs.readFileSync('/opt/zwinger/ca/zwinger-ca.pem')).fingerprint256)"
```

If openssl is installed, this command gives the same fingerprint after `sha256 Fingerprint=`:

```sh
openssl x509 -noout -fingerprint -sha256 -in /opt/zwinger/ca/zwinger-ca.pem
```

The cage start writes the new bundle `/etc/ssl/certs/ca-certificates.crt` to a temp file next to it, and then replaces
the bundle in one atomic step. A process that reads the bundle sees the complete old bundle or the complete new bundle.
If the bundle is a symlink, the app replaces the file at the end of the links and keeps the links. The bundle keeps its
file mode. If a step fails, the old bundle stays as it was and the cage shows a notice.

The cage start does not run `update-ca-certificates`, because Debian's version replaces a symlinked bundle with a
file. If openssl or `c_rehash` (Alpine `ca-certificates`) is installed, the cage start sets up CApath trust
automatically: it adds the CA to `/etc/ssl/certs` for clients that read only that folder, as `zwinger-ca.pem` and a hash
link, with the names that `update-ca-certificates` uses. The link of an earlier app start is removed. An image with
neither tool (for example `node:22-slim`) trusts the CA only through the bundle file.

At each app start, the app stops the VMs that the last run left running. If this stop fails, the first cage start
stops them again. While it fails, no cage starts.
