---
title: Network
description: Control the hosts that the agent can reach, and give it credentials.
---

All network traffic of a cage goes through the proxy of its project. The VM can reach only the proxy. Connections from
the outside into the cage are always blocked.

## Network policy

Each project has one network policy. Open it with ⌘4.

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

- The value is in the macOS Keychain, never in the project file. The app can write the value but cannot read it.
- The proxy adds a credential only to HTTPS requests.
- A credential on a Block rule is inactive. The proxy does not use it until the rule allows the host again.
- A wildcard host rule (`*.a.io`) cannot have a credential.
- A change of the value applies to the next request.

:::caution
Do not give a credential to a host that sends requests back, for example `httpbin.org`, `postman-echo.com` or an
`echo.*` host. The app shows a warning.
:::

The proxy uses its own certificate authority for HTTPS. Each cage start adds it to the trust store of the cage.
