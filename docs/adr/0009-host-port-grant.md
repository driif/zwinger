---
status: accepted
---

# Host port grant

A cage can reach one port of the Mac's loopback, for a local service such as Ollama. This amends ADR 0003: the always-blocked addresses keep "this computer" closed, except for this one grant. The guest sends plain HTTP to `http://host.microsandbox.internal:<port>` through the proxy. The proxy checks the grant after Offline and the Block rules that name that host, and before the always-blocked addresses. It dials `127.0.0.1:<port>` itself, with no name resolution. An Allow rule never opens the name, and the proxy port is never granted.

The grant is narrow, because the service acts as the Mac user:

- One port per cage, or none. Older configs have none.
- Plain HTTP/1.1 only. TLS in a CONNECT tunnel is closed and logged. An upgrade (WebSocket) is refused.
- Each request must name `http://host.microsandbox.internal:<port>` and no other authority. As for each plain HTTP origin, a bare host means port 80, so it names only a port-80 grant. A written port must use one to five ASCII decimal digits whose value equals the grant, and an authority with userinfo is refused. Each request must use GET, HEAD, POST, PUT, DELETE, OPTIONS or PATCH. An inner CONNECT or another method could make the service a raw tunnel, so the proxy refuses it before a dial.
- No credential and no host login go to the service.
- The proxy checks the live policy before each request and keeps no idle connection to the service, so Remove and Offline block the next request, also on a kept connection.

## Considered Options

- **No grant** (ADR 0003 as it was). Rejected: local model servers are a common need, and users would open the whole cage with `0.0.0.0` binds or a LAN address.
- **Expose the port in microsandbox.** Rejected: the request would skip the proxy, with no log, no Offline and no Block rule.
- **Allow rules for the gateway name.** Rejected: a rule covers all ports, and wildcard rules would open the Mac by accident.

## Consequences

- A response that already streams continues after Remove or Offline. Only the next request is blocked.
- A service that listens only on `::1` or on a LAN address is not reachable.
- HTTPS clients and WebSocket clients fail with `host-port`. The UI shows the `http://` URL.
