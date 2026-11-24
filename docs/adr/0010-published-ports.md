---
status: accepted
---

# Published ports

An app that runs in a VM, such as a dev server, had no way to the Mac: ingress was always denied. A cage can now publish a guest TCP port on `127.0.0.1:<host port>` of this Mac. The user adds each port by hand and turns it on or off; a cage has none by default, and older configs have none. This amends ADR 0003: microsandbox's outer wall now also allows ingress TCP from the Mac's loopback to the published guest ports, and nothing else.

- microsandbox publishes the port (`SandboxBuilder::port_bind`) with the bind address `127.0.0.1`, never `0.0.0.0`. Other computers cannot connect.
- A cage has at most 16 ports. A guest port and a host port each occur once in a cage.
- A save checks that each new or turned-on host port is free on the Mac, unless a running VM of the cage publishes it now.
- Each VM start checks each port again. A port that is in use, or that is the proxy port of the VM, is skipped with a notice; the VM still starts. The other VMs of the cage (own VMs) get the same ports, so only the first VM that starts gets a port.
- microsandbox 0.7.8 publishes ports only at VM create. A change applies on the next start of each VM, and its status shows a pending restart.
- The listener ends with the VM.

## Considered Options

- **An app-side forwarder** (zwinger listens on the Mac and dials the guest): it could apply a change without a restart, but zwinger then owns one more security-critical relay, and it needs a path into the guest that the outer wall denies. Rejected: microsandbox already has a loopback listener with an ingress policy.
- **Bind on all addresses**, so a phone on the LAN can test the app. Rejected: it exposes the VM to the network. A user who needs that can run their own forwarder on the Mac.
- **No ports** (ADR 0003 as it was). Rejected: users would open the cage in worse ways to test a web app.

## Consequences

- Each process of each user on this Mac can connect to a published port, also a web page in a browser (DNS rebinding). The UI and the docs warn, and the port is off until the user turns it on.
- A published port does not go through the proxy: it is not in the network log, and Offline and the rules do not apply to it. The guest's replies go back on the same connection; the guest gets no new egress.
- A port that is in use at a start is not published until the next start.
