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
- Two agents (pi, Claude Code, Codex), each with an optional model. **Agent 1** decides a tiebreak. The two agents can
  be of the same kind, for example Claude Code with `opus` and Claude Code with `sonnet`. The two models are
  independent: they can also be equal, or empty for the default model of the CLI. Gemini CLI is also available if the
  cage uses Gemini CLI from the Mac.
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
in a mount, they cover the working dir in its Git repository instead. They exclude every round copy of the current task, so a continuation does not see the copy of its source. If the VM
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

Each agent of a run has a name. Agents of different kinds use the kind, for example `claude` and `codex`. Two agents of
the same kind use `claude-1` and `claude-2` (or `codex-1`, `pi-1` and so on), in the order of the task. The name never
contains the model. The room shows each agent with the icon of its kind, its kind and its model, or `default model`
if the task gives no model. The default model is the one that the CLI selects; zwinger does not know which model that
is. To select the implementer, use the button with the name of the agent. Only an agent with a plan can be selected.

The room of an earlier run shows the agents and models that the run started with, not the current task. An older run
that recorded its agents and models still shows them. An older run that recorded no models keeps its names and shows
only the kind, with no model details. A duo round folder from outside
the app shows its own names with a generic icon and no model details.

The room updates when a file of the round changes. It watches the round folder, its `turns` folder and the folder
that contains the round. It reads again only the round files that changed, not other files in the folder. A burst of
changes gives one update, at most 0.5 s after the first change. If the round folder or `turns` is removed or replaced,
the room shows what is there and watches the new folder. Each time the room reads the whole folder, it also watches the
folders again. While an agent of an external round writes its live output, the room checks that output about every
0.5 s, because macOS can report no change while the agent keeps the file open. Other round files update by events.
Every 30 seconds the room does a light check: it checks the
two folders and the round files that it shows, and reads the whole folder again only if a folder changed. This finds a
change that macOS did not report. The check also reads new live output. If the app cannot watch the round folder,
`turns` or the folder that contains the round, the room does this check every 2 seconds and tries to watch again. The room reads at most 64 round files and 32 MiB. A file larger than 8 MiB is not read. A full
read of a folder takes longer when the folder has many other files.

If the round folder or `turns` is replaced by a different folder, the room reads nothing from the new folder until it
reads it completely. Then it clears all live output and shows the live output of the new folder. When macOS reports
that a round file was added, removed or renamed, the room reads only that file and the next light check does not read
the whole folder again. A change that macOS did not report in the same moment can then wait until the next change of
the folder. When an in-app run ends, the room keeps its last status.

**Gates** shows the latest command output and exit codes. The timeline keeps a summary of each completed gate run.

**Show the diff** shows the change of the run: the commits since the run started, then the diff. During a run, it
reads the working dir in the VM. After the implementation and after the revision, the run saves a separate diff
and mirrors the latest one as `diff.txt`. When the run has ended, the room shows the diff named by its checkpoint,
also when the cage is stopped. A cancelled stage saves no diff. If git fails after the revision, the saved revision
diff says so and keeps the last saved diff. For a run with no saved diff, the room reads the working dir if the cage runs.
Show the diff reads the working dir that the run recorded when it started, not the current working dir of the task.

A cage stop cancels its runs.

## Resume an ended run

A run that stopped before its end can continue. This is also possible after an app restart. Open the room of the run
and select **Resume from** _stage_. The button names the first stage that runs again.

- The continuation is a new run. Its subtitle says `continues run <NN>`. The stopped run keeps its files and its
  result.
- The continuation uses the title, the request, the agents, the gates and the limits that the stopped run started
  with. A task change after the stop does not change them. An agent with no model uses the default model of its CLI.
- The completed stages do not run again. The room shows them as done, with `(from run <NN>)`. The continuation has
  their results, the decisions, your messages and the advisory messages, and the change of the run starts at the same
  commit.
- The stage that was active when the run stopped runs again from its start. The room shows it with `(repeated)`. If
  the run stopped during the parallel plans, the parallel final reviews or the gates, all of them run again. A stage
  that did not start before the stop has no `(repeated)` mark.
- The gates, the cross review and the final reviews after the last implementation or revision apply only to the files
  that they checked. Before the continuation uses them, zwinger compares the commit, the index and the working files
  with the state that each check saw. If they are different, if zwinger cannot read them, or if the working dir is not
  in Git, the earliest affected check and the checks after it run again. The room shows the earliest check that may
  repeat. The continuation removes their old results, so the room shows these stages as not done until they complete
  again. The stopped run keeps its results. The round copies in `.zwinger/tasks/` do not count as a change.
- If the run stopped after its final reviews but before it recorded its result, the button is **Resume from Result**.
  If the files did not change, no agent and no gate runs again: the continuation records the result of the final
  reviews of the stopped run. If one final reviewer did not approve, the result is no ship.
- The implementation, the revision and the gates are not run exactly once. If the run stopped in one of them, the
  agent or the gate starts again with the files as they are now. zwinger does not reset, check out, clean or apply a
  diff to the working dir.
- Open questions that did not get an answer before the stop are asked again. If zwinger recorded your answer to the
  same questions before the stop, the continuation uses it and does not ask again. A selection of the implementer
  before the stop stays.
- Your messages and the advisory messages of the stopped run count against the limits of the continuation. zwinger
  records each accepted message before it queues it. A message that the run accepted, but did not give to the agents
  before the stop, still counts. It does not reach the agents of the continuation by itself. If an A2A tool sends the
  same `messageId` with the same text to the continuation, the continuation applies it once, with no new charge. The
  same `messageId` with a different text is an error. If the run stopped while it wrote a message to its record, only a
  complete recorded message counts as applied: an accepted message that was not applied stays charged, and zwinger does
  not restore its text.

The button is not available, and shows why, in these cases:

- The cage does not run. Start the cage first. The continuation uses the current folders and mounts of the cage, not
  the ones at the time of the stop.
- Another run of the same task is live. Wait until it ends, or cancel it.
- A newer run already continues this run. Resume the newer run.
- The run started before zwinger saved resume records. Start a new run.
- The resume records of the run are damaged or missing, are from a different app version, or do not agree with the
  run's files. The same applies when the run could not write its record, also if you cancelled it after that. Start a
  new run.
- The run completed all stages and recorded its result.

An A2A share of the stopped run does not move to the continuation. The continuation has a new A2A task ID. To share it,
select **Share over A2A** in its room.

**Open round folder…** shows a round folder from your Mac in the room. You can read it, but you cannot change it. During an active stage, the room shows public live output from the checked-in Duo runner's Claude stream JSON files. It shows Codex JSON records only when the stream starts with a Codex thread or turn record at its beginning. Legacy Codex stderr shows a diagnostic label and fixed notices when a command runs or a patch applies; it never shows command text, output or reasoning. These notices are not agent replies or verdicts. A Codex stream opened in the middle shows only legacy notices. The room reads the latest 128 KiB when you open a long stream, starts at a complete record, and marks skipped history. It reads up to 64 KiB from each selected file per update, and continues with the next 64 KiB at the next update. A live record larger than 32 KiB is skipped with a notice. A tool whose result is in that record shows as done. Long streams can clear earlier live output to bound memory. If one participant's file changes or its live history reaches the limit, only that participant's live output clears. A stage or participant change clears all live output. These changes do not affect the saved round result.

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
the same text again, the run applies it only once and does not charge it again. This is also true in a continuation of
the run. The same `messageId` with a different text is an error. An ended
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
