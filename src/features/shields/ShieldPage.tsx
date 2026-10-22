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
import { RunSummary } from "../tasks/api";
import {
  AGENT_NAMES,
  AgentKind,
  PaneNode,
  SessionKind,
  ShieldView,
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

// React Flow and the run room are large and not on the first screen of every shield.
const ShieldMap = lazy(() => import("./ShieldMap").then((m) => ({ default: m.ShieldMap })));
const TaskRoom = lazy(() => import("../tasks/TaskRoom").then((m) => ({ default: m.TaskRoom })));

/** The provider host of each host login. pi's depends on its login. */
const LOGIN_HOSTS: Partial<Record<AgentKind, string>> = {
  claude: "api.anthropic.com",
  codex: "chatgpt.com",
  gemini: "cloudcode-pa.googleapis.com",
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
  const catalog = useCatalog();
  const canLogin = agent?.login.state === "found";
  const [profile, setProfile] = useState(current?.profile ?? agent?.profile === true);
  const [login, setLogin] = useState(current?.login ?? canLogin);
  const host = LOGIN_HOSTS[kind];
  const blocked = host === undefined || !decide(shield.network, host, catalog).allowed;
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
  /** Opens the Host agents settings. */
  onHostAgents: () => void;
  /** The runs of the shield's tasks. */
  runs: RunSummary[];
  onRunsChange: () => void;
  /** Reads the shields again. Resolves when the new state is set. */
  onRefresh: () => Promise<unknown>;
  panes: Pane[];
  /** The focused pane of this shield. */
  focused?: string;
  run: Run;
  onToggle: () => void;
  /** `agent` opens the shield's first agent. */
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

// Not keyed by shield: the terminal view keeps the panes of every shield attached.
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
  const { status } = shield;
  const [listed, setListed] = useState<{ id: string; folders: string[] }>();
  const [reloads, setReloads] = useState(0);
  /** The host notices that the user dismissed in the current running period of a shield. */
  const [dismissed, setDismissed] = useState<{ shield: string; notices: string[] }>();
  if (dismissed?.shield === shield.id && status.state !== "running") setDismissed(undefined);
  const hidden = dismissed?.shield === shield.id ? dismissed.notices : [];
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
  const unverified = [...(shield.folderId ? [] : [shield.folder]), ...shield.mounts.filter((m) => !m.id).map((m) => m.host)];
  const app = view.kind === "app" ? shield.apps.find((a) => a.id === view.appId) : undefined;
  const task = view.kind === "task" ? shield.tasks.find((t) => t.id === view.taskId) : undefined;
  const actions = launchActions(shield, editors);
  const newTerminals = actions.filter((a) => a.type === "session");
  const editorActions = actions.filter((a) => a.type === "editor");
  const defaultEditor = editorActions.find((a) => a.editor.id === editors?.default);

  return (
    <section className="page">
      <header className="page-header row">
        <div className="grow">
          <h1 className="page-title">{shield.name}</h1>
          <p className="muted row">
            <span className={`dot ${status.state}`} />
            {status.state === "error" ? <span className="danger">{status.message}</span> : status.state}
            {status.state === "running" && shield.loginExpiresAt !== null && <span>· login valid until {time(shield.loginExpiresAt)}</span>}
          </p>
          <CopyField className="mono" value={shield.folder} display={tildePath(shield.folder, home)} title={shield.folder} />
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
              label: `${shield.hostAgents.some((a) => a.kind === agent.kind) ? "Change" : "Add"} ${AGENT_NAMES[agent.kind]} from this Mac…`,
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
      <div className="notices">
        {unverified.length > 0 && (
          <Notice
            icon={<TriangleAlert size={14} className="warn" />}
            actions={
              <Button size="sm" variant="outline" onClick={() => run(() => confirmPaths(shield.id).then(onRefresh))}>
                Confirm folders
              </Button>
            }
          >
            Not verified since the update: <span className="mono">{unverified.join(", ")}</span>. Check that each one is
            the folder that you added, then confirm it.
          </Notice>
        )}
        {status.state === "running" &&
          shield.hostNotices
            .filter((notice) => !hidden.includes(notice))
            .map((notice) => (
              <Notice
                key={notice}
                icon={<Info size={14} className="muted" />}
                onDismiss={() => setDismissed({ shield: shield.id, notices: [...hidden, notice] })}
              >
                {notice}
              </Notice>
            ))}
        {status.state === "running" &&
          shield.guestLogins.map((agent) => (
            <Notice
              key={agent}
              icon={<TriangleAlert size={14} className="warn" />}
              actions={
                <>
                  <Button size="sm" variant="outline" onClick={() => run(() => removeGuestLogin(shield.id, agent).then(onRefresh))}>
                    Remove it
                  </Button>
                  <Button size="sm" variant="outline" onClick={onHostAgents}>
                    Use my Mac login instead
                  </Button>
                </>
              }
            >
              {AGENT_NAMES[agent]} has its own login in this shield. Its real token is in the shield, where the agent can
              read and send it.
            </Notice>
          ))}
      </div>
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
                {shield.terminals.length === 0 && (
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
                    {shield.hostAgents.length === 0 && detection && detection.agents.length > 0
                      ? "Next: add an agent from this Mac in the New terminal menu, or open the built-in pi agent."
                      : "Next: open an agent, or a shell on this Mac in the shield folder."}
                  </Notice>
                )}
                <Suspense>
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
                </Suspense>
                <dl className="facts">
                  <div>
                    <dt>Agent starts in</dt>
                    <dd className="mono">{shield.harness ?? "/workspace"}</dd>
                  </div>
                  <div>
                    <dt>Network</dt>
                    <dd>
                      {shield.network.offline
                        ? "Offline"
                        : `${shield.network.default === "allow" ? "Allow" : "Block"} everything else, ${shield.network.rules.length} rules`}
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
          <Suspense>
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
              onStartShield={onToggle}
            />
          </Suspense>
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
          focused={focused}
          editor={defaultEditor?.editor.name}
          visible={view.kind === "terminal"}
          {...terminal}
        />
      </div>
    </section>
  );
}
