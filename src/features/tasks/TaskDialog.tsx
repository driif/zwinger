import { Button, Card, CardBody, CardFooter, CardHeader, FloatingField, SegmentedControl, Textarea } from "@driif/motiv";
import { FormEvent, useState } from "react";
import { putTask } from "./api";
import { AGENT_NAMES, AgentKind, CageView, SessionView, TaskAgent } from "../../shared/api";
import { errorMessage } from "../../shared/errorMessage";

const KINDS: AgentKind[] = ["claude", "codex", "pi"];
/** Gemini CLI runs only from this Mac. */
const kinds = (cage: CageView): AgentKind[] =>
  cage.hostAgents.some((a) => a.kind === "gemini") ? [...KINDS, "gemini"] : KINDS;

interface Props {
  session: SessionView;
  cage: CageView;
  /** No `taskId` adds a task. */
  taskId?: string;
  onDone: (taskId?: string) => Promise<void>;
}

/** The form of a new or changed task. */
export function TaskDialog({ session, cage, taskId, onDone }: Props) {
  const task = session.tasks.find((t) => t.id === taskId);
  const [title, setTitle] = useState(task?.title ?? "");
  const [request, setRequest] = useState(task?.request ?? "");
  const [agents, setAgents] = useState<[TaskAgent, TaskAgent]>(
    task?.agents ?? [
      { kind: "claude", model: null },
      { kind: "codex", model: null },
    ],
  );
  const [gates, setGates] = useState(task?.gates.join("\n") ?? "");
  const [workdir, setWorkdir] = useState(task?.workdir ?? session.harness ?? "/workspace");
  const [turns, setTurns] = useState(String(task?.turns ?? 2));
  const [idleSecs, setIdleSecs] = useState(String(task?.idleSecs ?? 600));
  const [hardSecs, setHardSecs] = useState(String(task?.hardSecs ?? 1800));
  const [independent, setIndependent] = useState<TaskAgent | null>(task?.independent ?? null);
  const [error, setError] = useState<string>();
  const [saving, setSaving] = useState(false);
  const vm = session.vmCage === cage.id ? cage.vms.find((v) => v.key === session.vmKey) : undefined;
  // Names need not be unique: drop only one entry for this session.
  const own = vm?.members.indexOf(session.name);
  const others = vm?.status.state === "running" ? vm.members.filter((_, index) => index !== own) : [];

  const setAgent = (index: 0 | 1, change: Partial<TaskAgent>) =>
    setAgents((current) => {
      const next: [TaskAgent, TaskAgent] = [...current];
      next[index] = { ...next[index], ...change };
      return next;
    });

  async function save(event: FormEvent) {
    event.preventDefault();
    setError(undefined);
    setSaving(true);
    try {
      const id = await putTask(session.id, {
        id: taskId,
        title,
        request,
        agents,
        gates: gates.split("\n"),
        workdir,
        turns: Number(turns),
        idleSecs: Number(idleSecs),
        hardSecs: Number(hardSecs),
        independent,
      });
      await onDone(id);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <form className="tab-body" onSubmit={save}>
      <Card variant="outlined">
        <CardHeader
          title={task ? `Edit ${task.title}` : "New task"}
          description="Two agents, also of one kind with different models, plan the task, agree on one implementer, build it, run the gates and review the change. The run never commits or pushes. After a run that ends with ship, you can review the change and approve a commit or a PR in the room."
        />
        <CardBody className="stack">
          <FloatingField label="Title" value={title} maxLength={64} required onChange={(e) => setTitle(e.target.value)} />
          <Textarea label="Request" rows={4} maxRows={16} value={request} required onChange={(e) => setRequest(e.target.value)} />
          {([0, 1] as const).map((index) => (
            <div key={index} className="row">
              <SegmentedControl
                label={index === 0 ? "Agent 1 (decides a tiebreak)" : "Agent 2"}
                options={kinds(cage).map((kind) => ({ value: kind, label: AGENT_NAMES[kind] }))}
                value={agents[index].kind}
                onChange={(kind) => setAgent(index, { kind: kind as AgentKind })}
              />
              <FloatingField
                className="grow"
                label="Model (empty: the default model)"
                value={agents[index].model ?? ""}
                showValidity={false}
                onChange={(e) => setAgent(index, { model: e.target.value || null })}
              />
            </div>
          ))}
          {others.length > 0 && (
            <p className="muted">
              The VM also runs {others.join(", ")}. The agents of a run can read and write their folders too, and those
              sessions can see this task's files. The Git check of a run covers only{" "}
              {/^\/workspace(\/|$)/.test(workdir) ? session.guestFolder : workdir}.
            </p>
          )}
          <Textarea
            label="Gates"
            hint="One shell line each, such as pnpm build. They run in the working dir after the implementation."
            rows={2}
            maxRows={8}
            value={gates}
            onChange={(e) => setGates(e.target.value)}
          />
          <div className="row">
            <FloatingField
              label="Exchange turns"
              hint="0 to 10. With 0, agent 1 decides the implementer at once."
              type="number"
              inputMode="numeric"
              min={0}
              max={10}
              step={1}
              value={turns}
              required
              onChange={(e) => setTurns(e.target.value)}
            />
            <FloatingField
              label="Idle limit (seconds)"
              hint="1 to 86400, not more than the hard limit. Output starts it again."
              type="number"
              inputMode="numeric"
              min={1}
              max={Math.min(86400, Number(hardSecs) || 86400)}
              step={1}
              value={idleSecs}
              required
              onChange={(e) => setIdleSecs(e.target.value)}
            />
            <FloatingField
              label="Hard limit (seconds)"
              hint="1 to 86400."
              type="number"
              inputMode="numeric"
              min={1}
              max={86400}
              step={1}
              value={hardSecs}
              required
              onChange={(e) => setHardSecs(e.target.value)}
            />
          </div>
          <p className="muted">
            The limits apply to each agent call and each gate command, also to the one retry of a reply with no JSON.
            A stage has no limit of its own.
          </p>
          <div className="row">
            <SegmentedControl
              label="Independent reviewer"
              options={[
                { value: "off", label: "Off" },
                ...kinds(cage).map((kind) => ({ value: kind, label: AGENT_NAMES[kind] })),
              ]}
              value={independent?.kind ?? "off"}
              onChange={(kind) =>
                setIndependent(kind === "off" ? null : { kind: kind as AgentKind, model: independent?.model ?? null })
              }
            />
            {independent && (
              <FloatingField
                className="grow"
                label="Model (empty: the default model)"
                value={independent.model ?? ""}
                showValidity={false}
                onChange={(e) => setIndependent({ ...independent, model: e.target.value || null })}
              />
            )}
          </div>
          {independent && (
            <p className="muted">
              After the two final reviews, a fresh agent reviews the full change, the gates and the review history. It
              does not see the votes of the pair. The run ships only if it approves too.
            </p>
          )}
          <FloatingField
            label="Working dir"
            hint="/workspace or a folder below it or below a mount"
            value={workdir}
            required
            onChange={(e) => setWorkdir(e.target.value)}
          />
          {error && <p className="danger mono">{error}</p>}
        </CardBody>
        <CardFooter className="row end">
          <Button variant="ghost" onClick={() => onDone(taskId)}>
            Cancel
          </Button>
          <Button type="submit" loading={saving}>
            {task ? "Save" : "Create task"}
          </Button>
        </CardFooter>
      </Card>
    </form>
  );
}
