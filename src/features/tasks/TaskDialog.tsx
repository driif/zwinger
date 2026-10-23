import { Button, Card, CardBody, CardFooter, CardHeader, FloatingField, SegmentedControl, Textarea } from "@driif/motiv";
import { FormEvent, useState } from "react";
import { putTask } from "./api";
import { AGENT_NAMES, AgentKind, ProjectView, TaskAgent } from "../../shared/api";
import { errorMessage } from "../../shared/errorMessage";

const KINDS: AgentKind[] = ["claude", "codex", "pi"];
/** Gemini CLI runs only from this Mac. */
const kinds = (project: ProjectView): AgentKind[] =>
  project.hostAgents.some((a) => a.kind === "gemini") ? [...KINDS, "gemini"] : KINDS;

interface Props {
  project: ProjectView;
  /** No `taskId` adds a task. */
  taskId?: string;
  onDone: (taskId?: string) => Promise<void>;
}

/** The form of a new or changed task. */
export function TaskDialog({ project, taskId, onDone }: Props) {
  const task = project.tasks.find((t) => t.id === taskId);
  const [title, setTitle] = useState(task?.title ?? "");
  const [request, setRequest] = useState(task?.request ?? "");
  const [agents, setAgents] = useState<[TaskAgent, TaskAgent]>(
    task?.agents ?? [
      { kind: "claude", model: null },
      { kind: "codex", model: null },
    ],
  );
  const [gates, setGates] = useState(task?.gates.join("\n") ?? "");
  const [workdir, setWorkdir] = useState(task?.workdir ?? project.harness ?? "/workspace");
  const [error, setError] = useState<string>();
  const [saving, setSaving] = useState(false);
  const sameKind = agents[0].kind === agents[1].kind;

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
      const id = await putTask(project.id, {
        id: taskId,
        title,
        request,
        agents,
        gates: gates.split("\n"),
        workdir,
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
          description="Two agents plan the task, agree on one implementer, build it, run the gates and review the change. The run never commits or pushes."
        />
        <CardBody className="stack">
          <FloatingField label="Title" value={title} maxLength={64} required onChange={(e) => setTitle(e.target.value)} />
          <Textarea label="Request" rows={4} maxRows={16} value={request} required onChange={(e) => setRequest(e.target.value)} />
          {([0, 1] as const).map((index) => (
            <div key={index} className="row">
              <SegmentedControl
                label={index === 0 ? "First agent (decides a tiebreak)" : "Second agent"}
                options={kinds(project).map((kind) => ({ value: kind, label: AGENT_NAMES[kind] }))}
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
          {sameKind && <p className="danger">Choose two agents of different kinds.</p>}
          <Textarea
            label="Gates"
            hint="One shell line each, such as pnpm build. They run in the working dir after the implementation."
            rows={2}
            maxRows={8}
            value={gates}
            onChange={(e) => setGates(e.target.value)}
          />
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
          <Button type="submit" loading={saving} disabled={sameKind}>
            {task ? "Save" : "Create task"}
          </Button>
        </CardFooter>
      </Card>
    </form>
  );
}
