import { Button, Card, CardBody, CardHeader, CommandMenu, ElasticTabs, LiquidSwitch, SpringCheckbox } from "@driif/motiv";
import { Play, RotateCw, Square, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";
import {
  AGENT_NAMES,
  AgentKind,
  Detection,
  GEMINI_NO_LOGIN,
  listFolders,
  NetEvent,
  putHostAgent,
  removeHostAgent,
  RunSummary,
  SessionKind,
  setAutoRenew,
  ShieldView,
} from "./api";
import { time } from "./HostAgentsSettings";
import { reachable } from "./SecretsSection";
import type { Intent, Pane, Run, Runtimes, TabView, View } from "./App";
import { AppView } from "./AppView";
import { NetworkTab } from "./NetworkTab";
import { SettingsTab } from "./SettingsTab";
import { ShieldMap } from "./ShieldMap";
import { TaskDialog } from "./TaskDialog";
import { TaskRoom } from "./TaskRoom";
import { TerminalTab } from "./TerminalTab";

/** The provider host of each host login. pi's depends on its login. Gemini CLI has none. */
const LOGIN_HOSTS: Partial<Record<AgentKind, string>> = {
  claude: "api.anthropic.com",
  codex: "chatgpt.com",
};

/** The step that adds a host agent to the shield, or changes or removes it. */
function AddAgent({ shield, detection, kind, run, onDone }: {
  shield: ShieldView;
  detection: Detection;
  kind: AgentKind;
  run: Run;
  onDone: () => void;
}) {
  const agent = detection.agents.find((a) => a.kind === kind);
  const current = shield.hostAgents.find((a) => a.kind === kind);
  const canLogin = kind !== "gemini" && agent?.login.state === "found";
  const [profile, setProfile] = useState(current?.profile ?? agent?.profile === true);
  const [login, setLogin] = useState(current?.login ?? canLogin);
  const host = LOGIN_HOSTS[kind];
  const blocked = host === undefined || !reachable(shield.network, host);
  const [allow, setAllow] = useState(true);
  if (!agent) return null;
  const name = AGENT_NAMES[kind];
  const save = () =>
    run(() => putHostAgent(shield.id, { kind, profile, login }, login && blocked && allow).then(onDone));

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title={`${current ? "Change" : "Add"} ${name} ${agent.version} from this Mac`}
        titleAs="h2"
        description="The shield uses the agent of this Mac. Changes apply on the next start."
      />
      <CardBody className="stack">
        <SpringCheckbox
          label="Profile, read-only: your skills, instructions and settings. The agent cannot change them in the shield."
          checked={profile}
          disabled={!agent.profile}
          onChange={(e) => setProfile(e.target.checked)}
        />
        <SpringCheckbox
          label={
            kind === "gemini"
              ? GEMINI_NO_LOGIN
              : canLogin
                ? "Subscription login: the proxy adds it to requests to the provider. The agent sees a placeholder only."
                : `Subscription login: ${agent.login.state === "expired" ? "expired; open the agent on this Mac once" : "none on this Mac"}`
          }
          checked={login}
          disabled={!canLogin && !current?.login}
          onChange={(e) => setLogin(e.target.checked)}
        />
        {login && blocked && (
          <SpringCheckbox
            label={host ? `Allow ${host}, the provider host` : "Allow the provider host of the pi login"}
            checked={allow}
            onChange={(e) => setAllow(e.target.checked)}
          />
        )}
        {login && kind === "claude" && (
          <span className="muted">When the shield starts, macOS asks once to allow access to the login: select Allow.</span>
        )}
        <div className="row end">
          {current && (
            <Button variant="danger" onClick={() => run(() => removeHostAgent(shield.id, kind).then(onDone))}>
              Remove
            </Button>
          )}
          <Button variant="ghost" onClick={onDone}>
            Cancel
          </Button>
          <Button onClick={save}>{current ? "Save" : "Add"}</Button>
        </div>
      </CardBody>
    </Card>
  );
}

// Agents and Terminal are no tabs: the tree names each agent and terminal, so tabs would show them twice.
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
  /** A folder from Finder was dropped on the shield boundary of the map. */
  onDropFolder: (path: string) => void;
  runtimes: Runtimes;
  /** The agents on this Mac. */
  detection?: Detection;
  /** The host agent that the Add agent step shows. */
  adding?: AgentKind;
  onAdding: (kind?: AgentKind) => void;
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
  /** `agent` opens the shield's first agent. */
  onNewSession: (kind: SessionKind | "agent") => void;
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
  onDropFolder,
  runtimes,
  detection,
  adding,
  onAdding,
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
  // The tab that shows again after a terminal, app or task view. The tabs stay
  // mounted meanwhile, so a draft in them is kept.
  const [tab, setTab] = useState<TabView>("overview");
  const isTab = view.kind === "overview" || view.kind === "network" || view.kind === "settings";
  if (isTab && view.kind !== tab) setTab(view.kind);

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
            {status.state === "running" && shield.loginExpiresAt !== null && <span>· login valid until {time(shield.loginExpiresAt)}</span>}
            <span className="mono grow" title={shield.folder}>
              {shield.folder}
            </span>
          </p>
        </div>
        {detection && detection.agents.length > 0 && (
          <CommandMenu
            className="model-picker"
            menuLabel="Add agent"
            placeholder="Add agent"
            commands={detection.agents.map((agent) => ({
              id: agent.kind,
              label: `${shield.hostAgents.some((a) => a.kind === agent.kind) ? "Change" : "Add"} ${AGENT_NAMES[agent.kind]} from this Mac`,
              onSelect: () => onAdding(agent.kind),
            }))}
          />
        )}
        <Button
          variant={running ? "secondary" : "primary"}
          iconLeft={running ? <Square size={14} /> : <Play size={14} />}
          loading={status.state === "starting" || status.state === "stopping"}
          onClick={onToggle}
        >
          {running ? "Stop" : "Start"}
        </Button>
      </header>
      {adding && detection && (
        <AddAgent key={adding} shield={shield} detection={detection} kind={adding} run={run} onDone={() => onAdding(undefined)} />
      )}
      {shield.renewDue && (
        <Card variant="outlined" padding="sm" className="banner row">
          <RotateCw size={16} className="warn" />
          <span className="grow">The host login expires soon, and this Mac has a newer one. Restart the shield to use it.</span>
          <Button size="sm" variant="outline" onClick={onRestart}>
            Restart to renew
          </Button>
        </Card>
      )}
      {status.state === "running" &&
        shield.hostNotices.map((notice) => (
          <Card key={notice} variant="outlined" padding="sm" className="banner row">
            <TriangleAlert size={16} className="warn" />
            <span className="grow">{notice}</span>
          </Card>
        ))}
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
                  {shield.hostAgents.length > 0 && (
                    <div>
                      <dt>From this Mac</dt>
                      <dd>
                        {shield.hostAgents
                          .map((a) => [AGENT_NAMES[a.kind], a.profile && "profile", a.login && "login"].filter(Boolean).join(" + "))
                          .join(", ")}
                      </dd>
                    </div>
                  )}
                  {shield.hostAgents.some((a) => a.login) && (
                    <div>
                      <dt>Login renewal</dt>
                      <dd>
                        <LiquidSwitch
                          label="Restart by itself when idle"
                          checked={shield.autoRenew}
                          onChange={(on) => run(() => setAutoRenew(shield.id, on))}
                        />
                      </dd>
                    </div>
                  )}
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
