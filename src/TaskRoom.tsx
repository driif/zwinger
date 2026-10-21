import { Button, Card, SegmentedControl, StepProgress, Textarea } from "@driif/motiv";
import { ArrowUp, FileDiff, Pause, Pencil, Play, Square } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import {
  controlTaskRun,
  isLive,
  Room,
  RoomSource,
  RunControl,
  RunStatus,
  RunSummary,
  ShieldView,
  startTaskRun,
  Task,
  unwatchRoom,
  runDiff,
  watchRoom,
} from "./api";
import { chatReducer, ChatState, initialChat } from "./agentChat";
import { Disagreements, elapsed, Gates, LiveEntry, Positions, Timeline, Verdicts } from "./roomParts";

/** An in-app task, or a duo round folder on the host that the room only reads. */
type Props =
  | { kind: "folder"; path: string }
  | {
      kind: "task";
      shield: ShieldView;
      task: Task;
      runs: RunSummary[];
      /** No `run` shows the latest run. */
      run?: string;
      onRun: (run: string) => void;
      onRunsChange: () => void;
      onEdit: () => void;
    };

/** The live room of a round: stages, messages, positions, disagreements and verdicts. */
export function TaskRoom(props: Props) {
  const runs = props.kind === "task" ? props.runs : [];
  const run = props.kind === "task" ? (props.run ?? runs[runs.length - 1]?.run) : undefined;
  const source: RoomSource | undefined =
    props.kind === "folder"
      ? { kind: "folder", path: props.path }
      : run
        ? { kind: "run", shieldId: props.shield.id, taskId: props.task.id, run }
        : undefined;
  const sourceKey = JSON.stringify(source ?? null);

  const [room, setRoom] = useState<Room>();
  const [status, setStatus] = useState<RunStatus | null>(null);
  const [live, setLive] = useState<Record<string, { stage: string; chat: ChatState }>>({});
  const [error, setError] = useState<string>();
  const [draft, setDraft] = useState("");
  const [diff, setDiff] = useState<string>();
  const [now, setNow] = useState(Date.now());

  useEffect(() => {
    setRoom(undefined);
    setStatus(null);
    setLive({});
    setDiff(undefined);
    setError(undefined);
    if (!source) return;
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
      (e) => setError(String(e)),
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
  const shieldRunning = inApp?.shield.status.state === "running";
  const liveRun = status !== null && isLive(status.state);
  const canStart = inApp && shieldRunning && !runs.some((r) => isLive(r.state));

  const act = (work: () => Promise<unknown>) => {
    setError(undefined);
    work().then(
      () => inApp?.onRunsChange(),
      (e) => setError(String(e)),
    );
  };
  const control = (c: RunControl) => inApp && run && act(() => controlTaskRun(inApp.shield.id, inApp.task.id, run, c));
  const start = () =>
    inApp &&
    act(async () => {
      const next = await startTaskRun(inApp.shield.id, inApp.task.id);
      inApp.onRun(next);
    });

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
                ? `Run ${run} · ${status?.state ?? runs.find((r) => r.run === run)?.state ?? "loading"}${status?.pausing && status.state === "running" ? " · pauses after this stage" : ""}`
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
          room?.agents.map((agent) => {
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
                disabled={blocked !== undefined}
                title={blocked}
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
        {inApp && (
          <>
            <Button size="sm" variant="ghost" iconLeft={<Pencil size={14} />} onClick={inApp.onEdit}>
              Edit task
            </Button>
            <Button
              size="sm"
              iconLeft={<Play size={14} />}
              disabled={!canStart}
              title={shieldRunning ? undefined : "Start the shield first"}
              onClick={start}
            >
              New run
            </Button>
          </>
        )}
      </header>
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
            {inApp.task.agents.map((a) => a.kind + (a.model ? ` (${a.model})` : "")).join(" and ")} work in{" "}
            <span className="mono">{inApp.task.workdir}</span>.{" "}
            {shieldRunning ? "Start a run to see them plan." : "Start the shield, then start a run."}
          </p>
        </Card>
      )}
      {room && (
        <div className="room-grid">
          <div className="room-timeline stack">
            <Timeline room={room} />
            {active &&
              room.agents
                .filter((agent) => live[agent]?.stage === active.id)
                .map((agent) => <LiveEntry key={agent} agent={agent} chat={live[agent].chat} since={since} />)}
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
                  disabled={!shieldRunning}
                  title={shieldRunning ? undefined : "Start the shield to read the working dir"}
                  onClick={() => run && act(() => runDiff(inApp.shield.id, inApp.task.id, run).then(setDiff))}
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
