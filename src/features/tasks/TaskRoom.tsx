import { Button, Card, CardBody, CardHeader, CopyField, SegmentedControl, StepProgress, Textarea } from "@driif/motiv";
import { ArrowUp, Cable, FileDiff, Pause, Pencil, Play, RotateCcw, Square, Unplug } from "lucide-react";
import { FormEvent, useEffect, useRef, useState } from "react";
import {
  controlTaskRun,
  isLive,
  Room,
  resumeTaskRun,
  RoomSource,
  RunControl,
  RunShare,
  RunStatus,
  RunSummary,
  shareTaskRun,
  startTaskRun,
  stopTaskRunShare,
  taskRunShare,
  unwatchRoom,
  runDiff,
  watchRoom,
} from "./api";
import { AGENT_NAMES, SessionView, Task } from "../../shared/api";
import { chatReducer, ChatState, initialChat } from "./agentChat";
import { latestRequest } from "./diffRequest";
import { Disagreements, elapsed, Gates, iconOf, LiveEntry, Positions, Timeline, Verdicts } from "./roomParts";
import { participantLabel } from "./roomLabels";
import { errorMessage } from "../../shared/errorMessage";

/** An in-app task, or a duo round folder on the host that the room only reads. */
type Props =
  | { kind: "folder"; path: string }
  | {
      kind: "task";
      session: SessionView;
      /** The session's VM runs. */
      cageRunning: boolean;
      task: Task;
      runs: RunSummary[];
      /** No `run` shows the latest run. */
      run?: string;
      onRun: (run: string) => void;
      onRunsChange: () => void;
      onEdit: () => void;
      onStartCage: () => void;
    };

/** The live room of a round: stages, messages, positions, disagreements and verdicts. */
export function TaskRoom(props: Props) {
  const runs = props.kind === "task" ? props.runs : [];
  const run = props.kind === "task" ? (props.run ?? runs[runs.length - 1]?.run) : undefined;
  const source: RoomSource | undefined =
    props.kind === "folder"
      ? { kind: "folder", path: props.path }
      : run
        ? { kind: "run", sessionId: props.session.id, taskId: props.task.id, run }
        : undefined;
  const sourceKey = JSON.stringify(source ?? null);

  const [room, setRoom] = useState<Room>();
  const [status, setStatus] = useState<RunStatus | null>(null);
  const [live, setLive] = useState<Record<string, { stage: string; chat: ChatState }>>({});
  const [error, setError] = useState<string>();
  const [draft, setDraft] = useState("");
  const [diff, setDiff] = useState<string>();
  const diffRequests = useRef(latestRequest()).current;
  const [share, setShare] = useState<RunShare | null>(null);
  const [sharing, setSharing] = useState(false);
  // A share result of an earlier run must not show its credential on this one, also after A→B→A.
  const shareRequests = useRef(latestRequest()).current;
  // A Resume reply belongs to the room that sent it: a later selection or an unmount makes it stale.
  const resumeRequests = useRef(latestRequest()).current;
  const [resuming, setResuming] = useState(false);
  const [now, setNow] = useState(Date.now());

  useEffect(() => resumeRequests.drop, [sourceKey]);

  useEffect(() => {
    setRoom(undefined);
    setStatus(null);
    setLive({});
    setDiff(undefined);
    diffRequests.drop();
    setShare(null);
    setSharing(false);
    shareRequests.drop();
    setResuming(false);
    setError(undefined);
    if (!source) return;
    if (source.kind === "run") {
      const isCurrent = shareRequests.next();
      taskRunShare(source.sessionId, source.taskId, source.run).then(
        (found) => isCurrent() && setShare(found),
        (e) => isCurrent() && setError(errorMessage(e)),
      );
    }
    let token: number | undefined;
    let closed = false;
    watchRoom(source, (event) => {
      if (event.type === "room") {
        setRoom(event.room);
        setStatus(event.run);
        return;
      }
      const { agent, stage, record } = event;
      setLive((current) => {
        const base = current[agent]?.stage === stage ? current[agent].chat : initialChat;
        return { ...current, [agent]: { stage, chat: chatReducer(base, { type: "record", record }) } };
      });
    }).then(
      (t) => {
        if (closed) unwatchRoom(t);
        else token = t;
      },
      (e) => setError(errorMessage(e)),
    );
    return () => {
      closed = true;
      if (token !== undefined) unwatchRoom(token);
    };
  }, [sourceKey]);

  const active = room?.stages.find((s) => s.state === "active");
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [active?.id]);

  const inApp = props.kind === "task" ? props : undefined;
  const cageRunning = inApp?.cageRunning;
  const liveRun = status !== null && isLive(status.state);
  const canStart = inApp && cageRunning && !runs.some((r) => isLive(r.state));
  const summary = runs.find((r) => r.run === run);
  const resumable = summary?.resume && !liveRun ? summary.resume : undefined;
  const resumeBlocked = !resumable
    ? undefined
    : "unavailable" in resumable
      ? `${resumable.unavailable[0].toUpperCase()}${resumable.unavailable.slice(1)}.`
      : !cageRunning
        ? "Start the cage first"
        : !canStart
          ? "Another run of this task is live"
          : undefined;

  const act = (work: () => Promise<unknown>) => {
    setError(undefined);
    work().then(
      () => inApp?.onRunsChange(),
      (e) => setError(errorMessage(e)),
    );
  };
  const readDiff = (sessionId: string, taskId: string, run: string) => {
    const isCurrent = diffRequests.next();
    setError(undefined);
    runDiff(sessionId, taskId, run).then(
      (text) => isCurrent() && setDiff(text),
      (e) => isCurrent() && setError(errorMessage(e)),
    );
  };
  const control = (c: RunControl) => inApp && run && act(() => controlTaskRun(inApp.session.id, inApp.task.id, run, c));
  const toggleShare = () => {
    if (!inApp || !run) return;
    const isCurrent = shareRequests.next();
    setError(undefined);
    setSharing(true);
    const ids = [inApp.session.id, inApp.task.id, run] as const;
    (share ? stopTaskRunShare(...ids).then(() => null) : shareTaskRun(...ids)).then(
      (next) => {
        if (!isCurrent()) return;
        setShare(next);
        setSharing(false);
      },
      (e) => {
        if (!isCurrent()) return;
        setError(errorMessage(e));
        setSharing(false);
      },
    );
  };
  const start = () =>
    inApp &&
    act(async () => {
      const next = await startTaskRun(inApp.session.id, inApp.task.id);
      inApp.onRun(next);
    });
  const resume = () => {
    if (!inApp || !run) return;
    const isCurrent = resumeRequests.next();
    setError(undefined);
    setResuming(true);
    // The run list of this task changes also when the user left the room before the reply.
    const refresh = inApp.onRunsChange;
    resumeTaskRun(inApp.session.id, inApp.task.id, run).then(
      (next) => {
        refresh();
        if (!isCurrent()) return;
        setResuming(false);
        inApp.onRun(next);
      },
      (e) => {
        // From task_recovery.rs: a newer run already continues this one.
        if (errorMessage(e).includes("continues this run")) refresh();
        if (!isCurrent()) return;
        setResuming(false);
        setError(errorMessage(e));
      },
    );
  };

  const waiting = status?.state === "waiting";
  const stepInBlocked =
    props.kind === "folder"
      ? "This round runs outside the app, so the room only reads its files. You cannot step in."
      : !run
        ? "Start a run to step in."
        : status === null
          ? "This run is from an earlier app session. It has ended."
          : !liveRun
            ? "The run has ended."
            : undefined;

  function send(event: FormEvent) {
    event.preventDefault();
    const text = draft.trim();
    if (!text) return;
    control({ type: waiting ? "answer" : "message", text });
    setDraft("");
  }

  const steps = room?.stages ?? [];
  // The last stage that ran; skipped stages after an early stop do not count.
  const current = Math.max(0, steps.map((s) => s.state !== "pending" && s.state !== "skipped").lastIndexOf(true));
  const since = active?.started ? elapsed(now - Date.parse(active.started)) : "";
  const first = steps.find((s) => s.started)?.started;
  const total = first ? elapsed((room?.endedAt ? Date.parse(room.endedAt) : now) - Date.parse(first)) : undefined;
  const title = props.kind === "task" ? props.task.title : props.path;

  return (
    <section className="room">
      <header className="row room-header">
        <div className="grow">
          <h2 className="page-title">{title}</h2>
          <p className="muted">
            {props.kind === "folder"
              ? "Duo round folder, read-only"
              : run
                ? `Run ${run}${summary?.continues ? ` · continues run ${summary.continues}` : ""} · ${status?.state ?? runs.find((r) => r.run === run)?.state ?? "loading"}${status?.pausing && status.state === "running" ? " · pauses after this stage" : ""}`
                : "No run yet"}
          </p>
        </div>
        {runs.length > 1 && run && inApp && (
          <SegmentedControl
            label="Run"
            options={runs.map((r) => ({ value: r.run, label: r.run }))}
            value={run}
            onChange={inApp.onRun}
          />
        )}
        {liveRun && status && !status.decided &&
          room?.participants.map((participant) => {
            const agent = participant.id;
            const Icon = iconOf(agent, room);
            const blocked = status.pickable.includes(agent)
              ? undefined
              : status.pickable.length === 0
                ? "The agents have no plan yet"
                : "This agent has no plan (degraded)";
            return (
              <Button
                key={agent}
                size="sm"
                variant={status.picked === agent ? "primary" : "outline"}
                iconLeft={<Icon size={14} />}
                disabled={blocked !== undefined}
                title={blocked ?? participantLabel(participant)}
                onClick={() => control({ type: "pickImplementer", agent })}
              >
                {agent} implements
              </Button>
            );
          })}
        {liveRun && status && (
          <>
            {status.state === "paused" || status.pausing ? (
              <Button size="sm" variant="secondary" iconLeft={<Play size={14} />} onClick={() => control({ type: "resume" })}>
                Resume
              </Button>
            ) : (
              <Button size="sm" variant="secondary" iconLeft={<Pause size={14} />} onClick={() => control({ type: "pause" })}>
                Pause
              </Button>
            )}
            <Button size="sm" variant="danger" iconLeft={<Square size={14} />} onClick={() => control({ type: "cancel" })}>
              Cancel
            </Button>
          </>
        )}
        {inApp && run && status !== null && (
          <Button
            size="sm"
            variant="outline"
            iconLeft={share ? <Unplug size={14} /> : <Cable size={14} />}
            loading={sharing}
            title={share ? undefined : "Let a tool on this Mac read this run and send advisory messages over A2A"}
            onClick={toggleShare}
          >
            {share ? "Stop sharing" : "Share over A2A"}
          </Button>
        )}
        {inApp && (
          <>
            <Button size="sm" variant="ghost" iconLeft={<Pencil size={14} />} onClick={inApp.onEdit}>
              Edit task
            </Button>
            {resumable && (
              <Button
                size="sm"
                variant="secondary"
                iconLeft={<RotateCcw size={14} />}
                disabled={resumeBlocked !== undefined}
                loading={resuming}
                title={
                  resumeBlocked ??
                  ("from" in resumable && resumable.repeats
                    ? `${resumable.label} started before the run stopped. It runs again in full.`
                    : undefined)
                }
                onClick={resume}
              >
                {"from" in resumable ? `Resume from ${resumable.label}` : "Resume"}
              </Button>
            )}
            <Button
              size="sm"
              iconLeft={<Play size={14} />}
              disabled={!canStart}
              title={cageRunning ? undefined : "Start the cage first"}
              onClick={start}
            >
              New run
            </Button>
          </>
        )}
      </header>
      {resumable && "from" in resumable && resumable.recheck && (
        <p className="muted">
          If the files changed or cannot be verified since {resumable.recheck}, {resumable.recheck} and later checks run again first.
        </p>
      )}
      {error && <p className="danger mono">{error}</p>}
      {total && <p className="muted">Total time {total}</p>}
      {room && steps.length > 0 && (
        <StepProgress
          label="Stages"
          steps={steps.map((s) => ({
            id: s.id,
            label: s.state === "skipped" ? `${s.label} (skipped)` : s.label,
            description: s.state === "active" ? `working… ${since}` : s.state === "stopped" ? room.ended : undefined,
          }))}
          current={current}
          completed={room.ended === "done"}
          completedText={room.outcome ? (room.outcome.ship ? "Done: ship" : "Done: no ship") : "Done"}
        />
      )}
      {!source && inApp && (
        <Card variant="outlined" padding="md" className="stack">
          <p>{inApp.task.request}</p>
          <p className="muted">
            {inApp.task.agents
              .map((a, i) => `Agent ${i + 1}: ${AGENT_NAMES[a.kind]} · ${a.model ?? "default model"}`)
              .join(" and ")}{" "}
            work in{" "}
            <span className="mono">{inApp.task.workdir}</span>.{" "}
            {cageRunning ? "Start a run to see them plan." : "Start the cage, then start a run."}
          </p>
          {!cageRunning && (
            <div className="row end">
              <Button size="sm" variant="outline" iconLeft={<Play size={14} />} onClick={inApp.onStartCage}>
                Start cage
              </Button>
            </div>
          )}
        </Card>
      )}
      {room && (
        <div className="room-grid">
          <div className="room-timeline stack">
            <Timeline room={room} />
            {active &&
              room.agents
                .filter((agent) => live[agent]?.stage === active.id)
                .map((agent) => <LiveEntry key={agent} agent={agent} room={room} chat={live[agent].chat} since={since} />)}
            {active && !room.agents.some((agent) => live[agent]?.stage === active.id) && (
              <p className="muted">
                {active.label}: working… {since}
              </p>
            )}
            {room.questions.length > 0 && (
              <Card variant="outlined" padding="sm" className="stack">
                <p className="warn">Open questions. The run waits for your answer.</p>
                <ul className="list">
                  {room.questions.map((q, i) => (
                    <li key={i}>{q}</li>
                  ))}
                </ul>
              </Card>
            )}
            <form className="composer" onSubmit={send}>
              <Textarea
                label={waiting ? "Answer the open questions" : "Message to the agents"}
                hint={stepInBlocked ?? "Every following stage prompt includes it."}
                rows={1}
                maxRows={6}
                value={draft}
                disabled={stepInBlocked !== undefined}
                onChange={(e) => setDraft(e.target.value)}
              />
              <div className="row end">
                <Button type="submit" iconLeft={<ArrowUp size={14} />} disabled={stepInBlocked !== undefined || !draft.trim()}>
                  {waiting ? "Answer" : "Send"}
                </Button>
              </div>
            </form>
          </div>
          <aside className="room-side stack">
            {share && (
              <Card variant="outlined" padding="sm">
                <CardHeader
                  title="A2A share"
                  titleAs="h2"
                  description="Local only. A tool on this Mac that has the token can read this run and send advisory messages to its next stages. The share ends when you stop it, remove the run or quit zwinger."
                />
                <CardBody className="stack">
                  <CopyField className="mono" label="Copy the URL" value={share.url} />
                  <CopyField className="mono" label="Copy the task ID" value={share.taskId} display={`Task ${share.taskId}`} />
                  <CopyField className="mono" label="Copy the token" value={share.token} display="Token ••••••••" />
                  <CopyField
                    className="mono"
                    label="Copy a curl example"
                    value={`curl -H 'A2A-Version: 1.0' -H 'Authorization: Bearer ${share.token}' '${share.url}/tasks/${share.taskId}?historyLength=5'`}
                    display="curl example (has the token)"
                  />
                </CardBody>
              </Card>
            )}
            <Positions room={room} />
            <Disagreements room={room} />
            <Verdicts room={room} />
            <Gates room={room} />
            {inApp && (
              <section className="stack">
                <h2 className="room-heading">Change</h2>
                <Button
                  size="sm"
                  variant="outline"
                  iconLeft={<FileDiff size={14} />}
                  disabled={!cageRunning && liveRun}
                  title={cageRunning || !liveRun ? undefined : "Start the cage to read the working dir"}
                  onClick={() => run && readDiff(inApp.session.id, inApp.task.id, run)}
                >
                  {diff === undefined ? "Show the diff" : "Read the diff again"}
                </Button>
                {diff !== undefined && <pre className="tool-output mono room-diff">{diff || "No change."}</pre>}
              </section>
            )}
          </aside>
        </div>
      )}
    </section>
  );
}
