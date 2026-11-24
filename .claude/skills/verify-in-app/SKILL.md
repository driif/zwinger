---
name: verify-in-app
description: Use after each batch, and before you tell the user that a UI or main-flow change is done. Runs the real Tauri app and checks it with screenshots.
---

# Verify in the app

Only the orchestrator runs this, in the main tree.

1. If port 1420 is busy (`lsof -tiTCP:1420 -sTCP:LISTEN`), the user runs the app: tell the user what to check and stop here.
2. Start `pnpm tauri dev` in the background. Wait for the line ``Running `target/debug/zwinger` ``.
3. Bring the window to the front: `osascript -e 'tell application "System Events" to set frontmost of (first process whose name is "zwinger") to true'`.
   Take a screenshot: `screencapture -x -o <scratchpad>/app-<step>.png`. Read the PNG.
4. Check the changed screens and the main flow: open a session, start its cage, open an agent, see its prompt.
5. Check the look: one control height per row, one left edge, one font scale, the same as the screens next to it.
6. Stop the dev server and its children. Report each fault as a bug. A fault goes before any ticket.

If the screenshot is black, the terminal app has no Screen Recording permission (System Settings > Privacy & Security). Ask the user once to grant it. Until then, say in the report that the app was not seen. Do not claim that it was seen working.
