---
status: accepted
---

# Guest path per session

A VM with one session mounts its folder at `/workspace`, as before. A VM with more than one session mounts each session folder at `/workspace/<slug>`. The slug is made once, when the session is made: the folder basename in kebab case, unique over all sessions. A rename of the session or a move to a different workspace or cage does not change it. The display name is never a path, because agents keep their history by folder (`claude -c`, `codex resume --last`) and a path that changes loses it.

## Considered Options

- **Always `/workspace/<slug>`.** One rule for all VMs, but each existing session loses its agent history and about 57 code references to `/workspace` change for the common case of one session per VM.
- **`/workspace/<display name>`.** A rename would move the folder in the guest.

## Consequences

- When a VM goes from one session to more, the first session moves from `/workspace` to `/workspace/<slug>`, and agent history of the old path is not found. The app says so before that start.
- Mounts must not overlap `/workspace` or any `/workspace/<slug>`. The overlap and identity checks run per session folder.
- The OSC 7 map, "Open in editor", the harness and the run workdir resolve a guest path below `/workspace/<slug>` to that session's folder.
