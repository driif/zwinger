---
status: accepted
---

# All shield egress goes through the app proxy

A shield's microVM can reach only one port on the host: zwinger's own proxy for that shield. The proxy decides each request (offline, always-blocked addresses after DNS resolution on the Mac, Block rules, Allow rules, then the default), injects credentials, and logs every request. microsandbox's own network policy is kept only as the outer wall that allows that one port. This gives complete request logs, credentials that the guest never sees (also headers the agent does not send), host-login renewal without a restart, and one simple model for the user: a default plus Allow/Block exceptions, where Block always wins. The cost: a tool that ignores `HTTPS_PROXY` has no network (it fails closed), and the proxy is now security-critical code that zwinger owns.

## Considered Options

- **microsandbox policy and secret placeholders only** (M2–M5): no per-request log for plain HTTP or direct IPs, credentials only where the agent sends a placeholder, a restart for every new token, and a three-mode UI that users found confusing.
- **Ordered first-match rules** (Cloudflare Gateway style): known to cause mistakes when a broad rule shadows a specific one. Rejected for "Block beats Allow".
