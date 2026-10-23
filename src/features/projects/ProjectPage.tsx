import { Button, Card, CardBody, CardHeader, CopyField, ElasticTabs, SpringCheckbox } from "@driif/motiv";
import { Bot, Code, Info, Laptop, Play, Plus, Settings, Square, TriangleAlert } from "lucide-react";
import { lazy, Suspense, useEffect, useState } from "react";
import { NetEvent } from "../network/api";
import {
  Detection,
  listFolders,
  putHostAgent,
  removeGuestLogin,
  removeHostAgent,
} from "../settings/api";
import { confirmPaths } from "./api";
import { StartProgress } from "./StartProgress";
import { RunSummary } from "../tasks/api";
import {
  AGENT_NAMES,
  AgentKind,
  PaneNode,
  SessionKind,
  ProjectView,
} from "../../shared/api";
import { time } from "../settings/HostAgentsSettings";
import { decide, useCatalog } from "../network/network";
import type { Intent, Pane, Run, Runtimes, TabView, View } from "../../App";
import { AppView } from "../apps/AppView";
import { NetworkTab } from "../network/NetworkTab";
import { SettingsTab } from "../settings/SettingsTab";
import { TaskDialog } from "../tasks/TaskDialog";
import { TerminalTab } from "../terminal";
import { Notice } from "../../shared/Notice";
import { tildePath } from "../../shared/paths";
import { LaunchAction, launchActions } from "../launcher/actions";
import { Editors } from "../launcher/api";
import { launchMenu, SplitLaunch } from "../launcher/SplitLaunch";

// React Flow and the run room are large and not on the first screen of every project.
const ProjectMap = lazy(() => import("./ProjectMap").then((m) => ({ default: m.ProjectMap })));
const TaskRoom = lazy(() => import("../tasks/TaskRoom").then((m) => ({ default: m.TaskRoom })));

/** The provider host of each host login. pi's depends on its login. */
const LOGIN_HOSTS: Partial<Record<AgentKind, string>> = {
  claude: "api.anthropic.com",
  codex: "chatgpt.com",
  gemini: "cloudcode-pa.googleapis.com",
};

/** The step that adds a host agent to the project, or changes or removes it. */
function AddAgent({ project, detection, kind, run, onDone }: {
  project: ProjectView;
  detection: Detection;
  kind: AgentKind;
  run: Run;
  onDone: () => void;
}) {
  const agent = detection.agents.find((a) => a.kind === kind);
  const current = project.hostAgents.find((a) => a.kind === kind);
  const catalog = useCatalog();
  const canLogin = agent?.login.state === "found";
  const [profile, setProfile] = useState(current?.profile ?? agent?.profile === true);
  const [login, setLogin] = useState(current?.login ?? canLogin);
  const host = LOGIN_HOSTS[kind];
  const blocked = host === undefined || !decide(project.network, host, catalog).allowed;
  const [allow, setAllow] = useState(true);
  if (!agent) return null;
  const name = AGENT_NAMES[kind];
  const save = () =>
    run(() => putHostAgent(project.id, { kind, profile, login }, login && blocked && allow).then(onDone));

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title={`${current ? "Change" : "Add"} ${name} ${agent.version} from this Mac`}
        titleAs="h2"
        description="The project uses the agent of this Mac. Changes apply on the next start."
      />
      <CardBody className="stack">
        <SpringCheckbox
          label="Profile, read-only: your skills, instructions and settings. The agent cannot change them in the cage."
          checked={profile}
          disabled={!agent.profile}
          onChange={(e) => setProfile(e.target.checked)}
        />
        <SpringCheckbox
          label={
            canLogin
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
          <span className="muted">When the cage starts, macOS asks once to allow access to the login: select Allow.</span>
        )}
        <div className="row end">
          {current && (
            <Button variant="danger" onClick={() => run(() => removeHostAgent(project.id, kind).then(onDone))}>
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
  project: ProjectView;
  /** The project's network events, oldest first. */
  events: NetEvent[];
  view: View;
  onView: (view: View) => void;
  intent?: Intent;
  onIntentDone: () => void;
  onNetSeen: (seq: number) => void;
  /** A folder from Finder was dropped on the project boundary of the map. */
  onDropFolder: (path: string) => void;
  runtimes: Runtimes;
  /** The agents on this Mac. */
  detection?: Detection;
  /** The host agent that the Add agent step shows. */
  adding?: AgentKind;
  onAdding: (kind?: AgentKind) => void;
  /** Opens the Host agents settings. */
  onHostAgents: () => void;
  /** The runs of the project's tasks. */
  runs: RunSummary[];
  onRunsChange: () => void;
  /** Reads the projects again. Resolves when the new state is set. */
  onRefresh: () => Promise<unknown>;
  panes: Pane[];
  /** The focused pane of this project. */
  focused?: string;
  run: Run;
  onToggle: () => void;
  /** `agent` opens the project's first agent. */
  onNewSession: (kind: SessionKind | "agent") => void;
  /** Opens a dormant or exited terminal again. */
  onReopen: (pane: Pane) => void;
  onFocus: (key: string) => void;
  onLayout: (layout: PaneNode) => void;
  onPaneExit: () => void;
  onPaneError: (error: unknown) => void;
  /** Splits the pane of a terminal with a Mac shell. */
  onSplit: (key: string, direction: "row" | "column") => void;
  /** Opens the terminal's current folder in the default editor. */
  onOpenEditor: (key: string) => void;
  /** Hides the pane of a terminal. Its process keeps running. */
  onHide: (key: string) => void;
  editors?: Editors;
  /** The home folder, shown as `~`. */
  home?: string;
  onLaunch: (action: LaunchAction) => void;
  onEditorSettings: () => void;
}

// Not keyed by project: the terminal view keeps the panes of every project attached.
export function ProjectPage({
  project,
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
  onHostAgents,
  runs,
  onRunsChange,
  onRefresh,
  panes,
  focused,
  run,
  onToggle,
  editors,
  home,
  onLaunch,
  onEditorSettings,
  ...terminal
}: Props) {
  const { status } = project;
  const [listed, setListed] = useState<{ id: string; folders: string[] }>();
  const [reloads, setReloads] = useState(0);
  /** The host notices that the user dismissed in the current running period of a project. */
  const [dismissed, setDismissed] = useState<{ project: string; notices: string[] }>();
  if (dismissed?.project === project.id && status.state !== "running") setDismissed(undefined);
  const hidden = dismissed?.project === project.id ? dismissed.notices : [];
  const folders = listed?.id === project.id ? listed.folders : undefined;
  // The tab that shows again after a terminal, app or task view. The tabs stay
  // mounted meanwhile, so a draft in them is kept.
  const [tab, setTab] = useState<TabView>("overview");
  const isTab = view.kind === "overview" || view.kind === "network" || view.kind === "settings";
  if (isTab && view.kind !== tab) setTab(view.kind);

  // Folders change outside the app, so the list reloads on focus and after each edit.
  useEffect(() => {
    let live = true;
    const id = project.id;
    listFolders(id).then((folders) => live && setListed({ id, folders }), () => {});
    return () => {
      live = false;
    };
  }, [project.id, reloads]);

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
  const sessions = project.sessions.filter((s) => s.exitCode === null).length;
  const jump = (kind: TabView) => onView({ kind });
  const unverified = [...(project.folderId ? [] : [project.folder]), ...project.mounts.filter((m) => !m.id).map((m) => m.host)];
  const app = view.kind === "app" ? project.apps.find((a) => a.id === view.appId) : undefined;
  const task = view.kind === "task" ? project.tasks.find((t) => t.id === view.taskId) : undefined;
  const actions = launchActions(project, editors);
  const newTerminals = actions.filter((a) => a.type === "session");
  const editorActions = actions.filter((a) => a.type === "editor");
  const defaultEditor = editorActions.find((a) => a.editor.id === editors?.default);

  return (
    <section className="page">
      <header className="page-header row">
        <div className="grow">
          <h1 className="page-title">{project.name}</h1>
          <p className="muted row">
            <span className={`dot ${status.state}`} />
            {status.state === "error" ? (
              <span className="danger">{status.message}</span>
            ) : status.state === "starting" ? (
              <StartProgress phase={status.phase} />
            ) : (
              status.state
            )}
            {status.state === "running" && project.loginExpiresAt !== null && <span>· login valid until {time(project.loginExpiresAt)}</span>}
          </p>
          <CopyField className="mono" value={project.folder} display={tildePath(project.folder, home)} title={project.folder} />
        </div>
        <SplitLaunch
          label="New terminal"
          icon={<Laptop size={14} />}
          onClick={() => onLaunch(newTerminals[0])}
          menuLabel="New terminal"
          menu={[
            ...launchMenu(newTerminals, onLaunch),
            // Host agents are set up here, next to the agents that they add.
            ...(detection?.agents ?? []).map((agent) => ({
              label: `${project.hostAgents.some((a) => a.kind === agent.kind) ? "Change" : "Add"} ${AGENT_NAMES[agent.kind]} from this Mac…`,
              icon: <Plus size={14} />,
              run: () => onAdding(agent.kind),
            })),
          ]}
        />
        {editors && (
          <SplitLaunch
            label={defaultEditor ? `Open in ${defaultEditor.editor.name}` : "No editor"}
            icon={<Code size={14} />}
            onClick={defaultEditor && (() => onLaunch(defaultEditor))}
            menuLabel="Editors"
            menu={[
              ...launchMenu(editorActions, onLaunch),
              { label: "Editor settings…", icon: <Settings size={14} />, run: onEditorSettings },
            ]}
          />
        )}
        <Button
          size="sm"
          variant={running ? "secondary" : "primary"}
          iconLeft={status.state === "starting" ? undefined : running ? <Square size={14} /> : <Play size={14} />}
          loading={status.state === "stopping"}
          disabled={status.state === "starting"}
          onClick={onToggle}
        >
          {status.state === "starting" ? <StartProgress phase={status.phase} /> : running ? "Stop cage" : "Start cage"}
        </Button>
      </header>
      {adding && detection && (
        <AddAgent key={adding} project={project} detection={detection} kind={adding} run={run} onDone={() => onAdding(undefined)} />
      )}
      <div className="notices">
        {unverified.length > 0 && (
          <Notice
            icon={<TriangleAlert size={14} className="warn" />}
            actions={
              <Button size="sm" variant="outline" onClick={() => run(() => confirmPaths(project.id).then(onRefresh))}>
                Confirm folders
              </Button>
            }
          >
            Not verified since the update: <span className="mono">{unverified.join(", ")}</span>. Check that each one is
            the folder that you added, then confirm it.
          </Notice>
        )}
        {status.state === "running" &&
          project.hostNotices
            .filter((notice) => !hidden.includes(notice))
            .map((notice) => (
              <Notice
                key={notice}
                icon={<Info size={14} className="muted" />}
                onDismiss={() => setDismissed({ project: project.id, notices: [...hidden, notice] })}
              >
                {notice}
              </Notice>
            ))}
        {status.state === "running" &&
          project.guestLogins.map((agent) => (
            <Notice
              key={agent}
              icon={<TriangleAlert size={14} className="warn" />}
              actions={
                <>
                  <Button size="sm" variant="outline" onClick={() => run(() => removeGuestLogin(project.id, agent).then(onRefresh))}>
                    Remove it
                  </Button>
                  <Button size="sm" variant="outline" onClick={onHostAgents}>
                    Use my Mac login instead
                  </Button>
                </>
              }
            >
              {AGENT_NAMES[agent]} has its own login in this cage. Its real token is in the cage, where the agent can
              read and send it.
            </Notice>
          ))}
      </div>
      <ElasticTabs
          className="tabs"
          hidden={!isTab}
          label="Project"
          tabs={TABS}
          value={tab}
          onChange={(value) => jump(value as TabView)}
          panels={{
            overview: (
              <div className="overview">
                {project.terminals.length === 0 && (
                  <Notice
                    icon={<Info size={14} className="muted" />}
                    actions={
                      <>
                        <Button size="sm" variant="outline" iconLeft={<Laptop size={14} />} onClick={() => terminal.onNewSession({ type: "host" })}>
                          New Mac shell
                        </Button>
                        <Button size="sm" iconLeft={<Bot size={14} />} onClick={() => terminal.onNewSession("agent")}>
                          New agent
                        </Button>
                      </>
                    }
                  >
                    {project.hostAgents.length === 0 && detection && detection.agents.length > 0
                      ? "Next: add an agent from this Mac in the New terminal menu, or open the built-in pi agent."
                      : "Next: open an agent, or a shell on this Mac in the project folder."}
                  </Notice>
                )}
                <Suspense>
                  <ProjectMap
                    key={project.id}
                    project={project}
                    folders={folders}
                    events={events}
                    visible={isTab && tab === "overview"}
                    run={run}
                    onJump={jump}
                    onDropFolder={onDropFolder}
                  />
                </Suspense>
                <dl className="facts">
                  <div>
                    <dt>Agent starts in</dt>
                    <dd className="mono">{project.harness ?? "/workspace"}</dd>
                  </div>
                  <div>
                    <dt>Network</dt>
                    <dd>
                      {project.network.offline
                        ? "Offline"
                        : `${project.network.default === "allow" ? "Allow" : "Block"} everything else, ${project.network.rules.length} rules`}
                    </dd>
                  </div>
                  <div>
                    <dt>Sessions</dt>
                    <dd>{sessions}</dd>
                  </div>
                  <div>
                    <dt>Apps running</dt>
                    <dd>{project.appRuns.filter((r) => r.exitCode === null).length}</dd>
                  </div>
                  {project.hostAgents.length > 0 && (
                    <div>
                      <dt>From this Mac</dt>
                      <dd>
                        {project.hostAgents
                          .map((a) => [AGENT_NAMES[a.kind], a.profile && "profile", a.login && "login"].filter(Boolean).join(" + "))
                          .join(", ")}
                      </dd>
                    </div>
                  )}
                </dl>
              </div>
            ),
            network: (
              <NetworkTab
                key={project.id}
                project={project}
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
                key={project.id}
                project={project}
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
            project={project}
            app={app}
            run={run}
            onExit={terminal.onPaneExit}
            onError={(e) => run(() => Promise.reject(e))}
          />
        </div>
      )}
      {task && view.kind === "task" && (
        <div className="view task-view">
          <Suspense>
            <TaskRoom
              key={task.id}
              kind="task"
              project={project}
              task={task}
              runs={runs.filter((r) => r.taskId === task.id)}
              run={view.run}
              onRun={(run) => onView({ kind: "task", taskId: task.id, run })}
              onRunsChange={onRunsChange}
              onEdit={() => onView({ kind: "taskEdit", taskId: task.id })}
              onStartCage={onToggle}
            />
          </Suspense>
        </div>
      )}
      {view.kind === "taskEdit" && (
        <div className="view task-view">
          <TaskDialog
            key={view.taskId ?? "new"}
            project={project}
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
          project={project}
          panes={panes}
          focused={focused}
          editor={defaultEditor?.editor.name}
          visible={view.kind === "terminal"}
          {...terminal}
        />
      </div>
    </section>
  );
}
