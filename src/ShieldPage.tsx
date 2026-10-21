import { Button, ElasticTabs } from "@driif/motiv";
import { Play, Square } from "lucide-react";
import { useEffect, useState } from "react";
import { AgentKind, listFolders, NetEvent, RunSummary, SessionKind, ShieldView } from "./api";
import { AgentsTab } from "./AgentsTab";
import type { ChatAction, Intent, Pane, Run, Runtimes, TabView, View } from "./App";
import { AppView } from "./AppView";
import { NetworkTab } from "./NetworkTab";
import { SettingsTab } from "./SettingsTab";
import { ShieldMap } from "./ShieldMap";
import { TaskDialog } from "./TaskDialog";
import { TaskRoom } from "./TaskRoom";
import { TerminalTab } from "./TerminalTab";

// Agents and Terminal are no tabs: the tree names each chat and terminal, so tabs would show them twice.
const TABS = [
  { value: "overview", label: "Overview" },
  { value: "network", label: "Network" },
  { value: "settings", label: "Settings" },
];

interface Props {
  shield: ShieldView;
  /** The shield's network events, oldest first. */
  events: NetEvent[];
  view: View;
  onView: (view: View) => void;
  intent?: Intent;
  onIntentDone: () => void;
  onNetSeen: (seq: number) => void;
  onChat: (agent: AgentKind, chat?: string) => void;
  onChatsChange: () => void;
  chatAction?: ChatAction;
  onChatActionDone: (work: Promise<unknown>) => void;
  /** A folder from Finder was dropped on the shield boundary of the map. */
  onDropFolder: (path: string) => void;
  runtimes: Runtimes;
  onRestart: () => void;
  /** The runs of the shield's tasks. */
  runs: RunSummary[];
  onRunsChange: () => void;
  /** Reads the shields again. Resolves when the new state is set. */
  onRefresh: () => Promise<unknown>;
  panes: Pane[];
  activePanes: Record<string, string>;
  run: Run;
  onToggle: () => void;
  onNewSession: (kind: SessionKind) => void;
  onPaneOpen: (key: string, sessionId: string) => void;
  onPaneExit: () => void;
  onPaneError: (key: string, error: unknown) => void;
}

// Not keyed by shield: the terminal view keeps the panes of every shield mounted.
export function ShieldPage({
  shield,
  events,
  view,
  onView,
  intent,
  onIntentDone,
  onNetSeen,
  onChat,
  onChatsChange,
  chatAction,
  onChatActionDone,
  onDropFolder,
  runtimes,
  onRestart,
  runs,
  onRunsChange,
  onRefresh,
  panes,
  activePanes,
  run,
  onToggle,
  ...terminal
}: Props) {
  const { status } = shield;
  const [listed, setListed] = useState<{ id: string; folders: string[] }>();
  const [reloads, setReloads] = useState(0);
  const folders = listed?.id === shield.id ? listed.folders : undefined;
  // The tab that shows again after a chat, terminal or app view. The tabs stay
  // mounted meanwhile, so a draft in them is kept.
  const [tab, setTab] = useState<TabView>("overview");
  const isTab = view.kind === "overview" || view.kind === "network" || view.kind === "settings";
  if (isTab && view.kind !== tab) setTab(view.kind);
  // Per shield, the agent that the chat view shows. A chat view with no agent keeps it.
  const [agents, setAgents] = useState<Record<string, AgentKind>>({});
  const agent = agents[shield.id] ?? "pi";
  // A rail action on a pi chat needs the pi agent, also while another agent's chat
  // shows. The view returns to that agent when the action is done.
  const wantedAgent = chatAction?.agent === "pi" ? "pi" : view.kind === "chat" ? view.agent : undefined;
  if (wantedAgent && wantedAgent !== agent) setAgents((current) => ({ ...current, [shield.id]: wantedAgent }));

  // Folders change outside the app, so the list reloads on focus and after each edit.
  useEffect(() => {
    let live = true;
    const id = shield.id;
    listFolders(id).then((folders) => live && setListed({ id, folders }), () => {});
    return () => {
      live = false;
    };
  }, [shield.id, reloads]);

  useEffect(() => {
    const reload = () => setReloads((n) => n + 1);
    window.addEventListener("focus", reload);
    return () => window.removeEventListener("focus", reload);
  }, []);

  const edit: Run = (action) =>
    run(() =>
      action().then((result) => {
        setReloads((n) => n + 1);
        return result;
      }),
    );

  const running = status.state === "running" || status.state === "stopping";
  const sessions = shield.sessions.filter((s) => s.exitCode === null).length;
  const jump = (kind: TabView) => onView({ kind });
  const app = view.kind === "app" ? shield.apps.find((a) => a.id === view.appId) : undefined;
  const task = view.kind === "task" ? shield.tasks.find((t) => t.id === view.taskId) : undefined;

  return (
    <section className="page">
      <header className="page-header row">
        <div className="grow">
          <h1 className="page-title">{shield.name}</h1>
          <p className="muted row">
            <span className={`dot ${status.state}`} />
            {status.state === "error" ? <span className="danger">{status.message}</span> : status.state}
            <span className="mono grow" title={shield.folder}>
              {shield.folder}
            </span>
          </p>
        </div>
        <Button
          variant={running ? "secondary" : "primary"}
          iconLeft={running ? <Square size={14} /> : <Play size={14} />}
          loading={status.state === "starting" || status.state === "stopping"}
          onClick={onToggle}
        >
          {running ? "Stop" : "Start"}
        </Button>
      </header>
      <ElasticTabs
          className="tabs"
          hidden={!isTab}
          label="Shield"
          tabs={TABS}
          value={tab}
          onChange={(value) => jump(value as TabView)}
          panels={{
            overview: (
              <div className="overview">
                <ShieldMap
                  key={shield.id}
                  shield={shield}
                  folders={folders}
                  events={events}
                  visible={isTab && tab === "overview"}
                  run={run}
                  onJump={jump}
                  onDropFolder={onDropFolder}
                />
                <dl className="facts">
                  <div>
                    <dt>Agent starts in</dt>
                    <dd className="mono">{shield.harness ?? "/workspace"}</dd>
                  </div>
                  <div>
                    <dt>Network</dt>
                    <dd>
                      {shield.network.mode === "public"
                        ? "Public internet"
                        : shield.network.mode === "deny" || shield.network.domains.length === 0
                          ? "No egress"
                          : `${shield.network.domains.length} allowed hosts`}
                    </dd>
                  </div>
                  <div>
                    <dt>Sessions</dt>
                    <dd>{sessions}</dd>
                  </div>
                  <div>
                    <dt>Apps running</dt>
                    <dd>{shield.appRuns.filter((r) => r.exitCode === null).length}</dd>
                  </div>
                </dl>
              </div>
            ),
            network: (
              <NetworkTab
                key={shield.id}
                shield={shield}
                events={events}
                run={edit}
                intent={tab === "network" ? intent : undefined}
                onIntentDone={onIntentDone}
                active={isTab && tab === "network"}
                onSeen={onNetSeen}
              />
            ),
            settings: (
              <SettingsTab
                key={shield.id}
                shield={shield}
                folders={folders ?? []}
                runtimes={runtimes}
                run={edit}
                intent={tab === "settings" ? intent : undefined}
                onIntentDone={onIntentDone}
              />
            ),
          }}
        />
      {/* Stays mounted, so the chat keeps its state while another view shows. */}
      <div className="view" hidden={view.kind !== "chat"}>
        <AgentsTab
          key={shield.id}
          shield={shield}
          agent={agent}
          chat={view.kind === "chat" ? view.chat : undefined}
          onAgent={(agent) => onView({ kind: "chat", agent })}
          runtimes={runtimes}
          onRestart={onRestart}
          onJump={jump}
          onChat={onChat}
          onChatsChange={onChatsChange}
          chatAction={chatAction}
          onChatActionDone={onChatActionDone}
        />
      </div>
      {app && (
        <div className="view">
          <AppView
            key={app.id}
            shield={shield}
            app={app}
            run={run}
            onExit={terminal.onPaneExit}
            onError={(e) => run(() => Promise.reject(e))}
          />
        </div>
      )}
      {task && view.kind === "task" && (
        <div className="view task-view">
          <TaskRoom
            key={task.id}
            kind="task"
            shield={shield}
            task={task}
            runs={runs.filter((r) => r.taskId === task.id)}
            run={view.run}
            onRun={(run) => onView({ kind: "task", taskId: task.id, run })}
            onRunsChange={onRunsChange}
            onEdit={() => onView({ kind: "taskEdit", taskId: task.id })}
          />
        </div>
      )}
      {view.kind === "taskEdit" && (
        <div className="view task-view">
          <TaskDialog
            key={view.taskId ?? "new"}
            shield={shield}
            taskId={view.taskId}
            onDone={async (taskId) => {
              // The tree and the task view need the saved task.
              await onRefresh();
              onView(taskId ? { kind: "task", taskId } : { kind: "overview" });
            }}
          />
        </div>
      )}
      <div className="view" hidden={view.kind !== "terminal"}>
        <TerminalTab
          shield={shield}
          panes={panes}
          activePane={activePanes[shield.id]}
          visible={view.kind === "terminal"}
          {...terminal}
        />
      </div>
    </section>
  );
}
