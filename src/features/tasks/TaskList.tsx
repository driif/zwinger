import { Button, IconButton } from "@driif/motiv";
import { Ellipsis, ListChecks, Pencil, Play, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import type { SessionView } from "../../shared/api";
import type { Run, View } from "../../App";
import { ActionMenu } from "../../shared/ActionMenu";
import { isLive, removeTask, RunSummary, startTaskRun } from "./api";

/** The `NoCage` error text: a task needs a cage. */
const NO_CAGE_TASKS = "this session has no cage; pick a cage for it or a default cage for its workspace";

/** The badge of a task: its latest run. */
function runBadge(run: RunSummary | undefined) {
  if (!run) return undefined;
  if (run.state === "running") return run.stage ?? "running";
  if (run.state === "done") return run.ship ? "ship" : "no ship";
  return run.state;
}

interface Props {
  session: SessionView;
  /** True while the VM of the session's cage runs. */
  cageRunning: boolean;
  /** The runs of the session's tasks. */
  runs: RunSummary[];
  run: Run;
  onView: (view: View) => void;
  onRunsChange: () => void;
}

/** The tasks of a session, each with its latest run and its actions. */
export function TaskList({ session, cageRunning, runs, run, onView, onRunsChange }: Props) {
  const [menu, setMenu] = useState<string>();
  const sid = session.id;
  return (
    <div className="tab-body">
      <div className="row">
        <Button size="sm" variant="outline" iconLeft={<Plus size={14} />} onClick={() => onView({ kind: "taskEdit" })}>
          New task
        </Button>
      </div>
      {session.tasks.length === 0 ? (
        <p className="muted">No tasks yet. A task lets two agents plan, build and review a change together.</p>
      ) : (
        <ul className="task-list">
          {session.tasks.map((task) => {
            const taskRuns = runs.filter((r) => r.taskId === task.id);
            const latest = taskRuns[taskRuns.length - 1];
            const live = taskRuns.some((r) => isLive(r.state));
            const badge = runBadge(latest);
            return (
              <li key={task.id} className="row" onContextMenu={(event) => {
                event.preventDefault();
                setMenu(task.id);
              }}>
                <Button
                  size="sm"
                  variant="ghost"
                  className="grow"
                  iconLeft={<ListChecks size={14} className={live ? "ok" : undefined} />}
                  title={task.request}
                  onClick={() => onView({ kind: "task", taskId: task.id })}
                >
                  {task.title}
                </Button>
                {badge && <span className={latest?.state === "done" && !latest.ship ? "danger" : "muted"}>{badge}</span>}
                <ActionMenu
                  open={menu === task.id}
                  onOpenChange={(open) => setMenu(open ? task.id : undefined)}
                  trigger={<IconButton size="sm" icon={<Ellipsis size={12} />} label={`Actions for ${task.title}`} />}
                  actions={[
                    {
                      label: "Start run",
                      icon: <Play size={14} />,
                      disabled: !cageRunning || live,
                      disabledReason: session.effectiveCage ? undefined : NO_CAGE_TASKS,
                      onSelect: () =>
                        run(async () => {
                          const next = await startTaskRun(sid, task.id);
                          onView({ kind: "task", taskId: task.id, run: next });
                          onRunsChange();
                        }),
                    },
                    { label: "Edit", icon: <Pencil size={14} />, onSelect: () => onView({ kind: "taskEdit", taskId: task.id }) },
                    {
                      label: "Delete",
                      icon: <Trash2 size={14} />,
                      disabled: live,
                      confirm: `Delete the task ${task.title} and its runs?`,
                      onSelect: () => run(() => removeTask(sid, task.id).then(onRunsChange)),
                    },
                  ]}
                />
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
