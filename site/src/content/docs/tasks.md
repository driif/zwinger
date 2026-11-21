---
title: Tasks
description: Two agents plan, build and review a task together.
---

A **task** is a request that two agents of a session plan, build and review together. The agents run headless in the
[cage](/cages/), not in a terminal. They do not use a Mac shell (⌘T), which runs on the Mac with no sandbox, or a
VM shell (⌘⇧T).

## Create a task

A task has these fields:

- **Title** and **Request**.
- Two agents of different kinds (pi, Claude Code, Codex), each with an optional model. The first agent decides a
  tiebreak. Gemini CLI is also available if the cage uses Gemini CLI from the Mac.
- **Gates**: shell lines that check the result, for example `pnpm build`.
- **Working dir**: `/workspace`, or a folder below it or below a mount. In a VM with more projects, `/workspace` is the
  project's folder `/workspace/<slug>`.

A change to a task needs no restart.

## Run

A run is one execution of a task. It has these stages:

1. The two agents write plans in parallel. They do not see the plan of the other agent.
2. Up to 2 turns where the agents compare their plans.
3. A tiebreak by the first agent, if the agents do not agree.
4. The implementation.
5. The run executes the gates.
6. The other agent reviews the implementation and the gate results.
7. If the review says `revise`, the implementer gets one revision. The run then executes the gates again.
8. The two agents review the change in parallel, each with the latest gate results. The result is `ship` only if both reviewers say so.

If an agent fails, the run continues in a degraded mode, and the room shows it. A degraded result is never shown as an
agreement.

A task needs a cage. A session with no cage cannot start a task: attach a cage, or set a default cage for its workspace.

In the stages that must not change files, the run compares the Git state of its folder before and after. A change
is a violation. Outside a Git work tree, this check is not possible, and the run tells you. A run never commits or
pushes.

The Git check and the change view cover the session folder, even when the Git root is above it. If the working dir is
in a mount, they cover the working dir in its Git repository instead. They exclude the run's round copy. If the VM
contains more than one session, the room shows a note when the run starts: the folder that the Git check covers, and
that other sessions in the VM can see the task's files. The task form names those sessions.

## Room

The room is the live view of a run: the stages, the messages, the position of each agent, the disagreements, the
verdicts and the degraded agents. During a run you can:

- write a message. Each subsequent stage includes it.
- pause after the current stage, and resume.
- select the implementer before the implementation starts.
- answer open questions. The run waits for the answers.
- cancel the run.

**Gates** shows the latest command output and exit codes. The timeline keeps a summary of each completed gate run.

**Show the diff** shows the change of the run: the commits since the run started, then the diff. During a run, it
reads the working dir in the VM. After the implementation and after the revision, the run saves the change as
`diff.txt` in its round. When the run has ended, the room shows this saved diff, also when the cage is stopped. A
cancelled stage saves no diff. If git fails after the revision, `diff.txt` says so and keeps the last saved diff. For a run with no saved diff, the room reads the working dir if the cage runs.

A cage stop cancels its runs. A run does not continue after an app restart.

**Open round folder…** shows a round folder from your Mac in the room. You can read it, but you cannot change it.

## Share a run over A2A

A tool on your Mac can read a run and send advisory messages to it with the
[A2A protocol](https://a2a-protocol.org/) 1.0. Sharing is off by default.

1. Open the room of a run that started in this app session.
2. Select **Share over A2A**. The room shows the **A2A share** card: the URL, the task ID, the token and a `curl`
   example. Each field has a copy button.
3. Give these details to the tool. To end the access, select **Stop sharing**.

The share is local only. It listens on `127.0.0.1` with a port that macOS assigns. Other computers and the cages cannot
connect to it. The token is in memory only: zwinger does not write it to a file or a log. The share ends when you stop
it, when you remove the run's task or project, and when you quit zwinger. After a restart, sharing is off. A new share
has a new port and a new token, so old details do not open it. A round folder that you open from your Mac cannot be
shared.

The share uses the HTTP+JSON binding of A2A 1.0 with polling. Each request, except for the Agent Card, needs the
headers `Authorization: Bearer <token>` and `A2A-Version: 1.0`.

| Request | Result |
|---|---|
| `GET /.well-known/agent-card.json` | The Agent Card. It names no run and needs no token. |
| `GET /tasks/<task ID>?historyLength=<n>` | The run as an A2A task: its state and its messages, oldest first. |
| `GET /tasks` | A list with the shared run only. |
| `POST /message:send` | An advisory message to the run. |

Each message in the task history has a stable `messageId` and two parts: the Markdown text, and a data part with
`from`, `to`, `kind`, `stage`, `at` and `body`. The `body` is the same text as the first part. For example:

```json
{"data": {"from": "a2a", "to": "all", "kind": "external", "stage": "plan", "at": "2026-11-21T09:30:00.000Z",
          "body": "The cache also needs a fix."}, "mediaType": "application/json"}
```

The task state is `TASK_STATE_WORKING` while the run works or is paused,
`TASK_STATE_INPUT_REQUIRED` while it waits for your answers, and then `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED` or
`TASK_STATE_CANCELED`. An ended run stays readable until you stop sharing.

To send an advisory message, use the task ID of the share and text parts only:

```sh
curl -X POST "$URL/message:send" \
  -H 'A2A-Version: 1.0' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/a2a+json' \
  -d '{"message": {"messageId": "note-1", "role": "ROLE_USER", "taskId": "'"$TASK"'",
       "parts": [{"text": "The cache also needs a fix."}]}}'
```

The reply is a message that says `applied` (the round has the message) or `queued` (the run applies it at its next
step). The room shows the message from `a2a` with the label **external · advisory**. The prompts of the following
stages show it in a separate section with a lower priority than your messages and the plan. An advisory message
cannot answer open questions, select the implementer, cancel the run or approve a result. It counts against the same
limits as your messages: 16 KiB per message, and 32 messages and 64 KiB per run. If you send the same `messageId` with
the same text again, the run does not add it again. The same `messageId` with a different text is an error. An ended
run refuses each message, also a repeated one. If the run cannot write a message to its record, it refuses that message
and each message after it, also a retry with the same `messageId`, with `UnsupportedOperationError`. The run then
ends with `TASK_STATE_FAILED`.

Streaming, push notifications, cancel and the extended Agent Card are not available. They return
`UnsupportedOperationError` or `PushNotificationNotSupportedError`.

## Records

zwinger writes the files of each run to its app data on the Mac, under the session ID. The cage cannot write there,
so an agent cannot change a verdict. At each stage, zwinger also writes a copy to
`.zwinger/tasks/<task>/<run>/` in the session folder. The harness setting does not move this copy. A run starts only
if zwinger can write it.
