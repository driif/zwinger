import {
  Button,
  Card,
  CardBody,
  CardHeader,
  CopyField,
  ElasticTabs,
  FloatingField,
  SegmentedControl,
  SpringCheckbox,
} from "@driif/motiv";
import { Bot, Info, Laptop, Play, Plus, Settings, Square, TriangleAlert } from "lucide-react";
import { CSSProperties, lazy, Suspense, useEffect, useState } from "react";
import { NetEvent } from "../network/api";
import {
  Detection,
  putHostAgent,
  removeGuestLogin,
  removeHostAgent,
} from "../settings/api";
import { confirmPaths, setClaudeToken } from "./api";
import { expiresSoon } from "./tokenExpiry";
import { putRule } from "../network/api";
import { StartProgress } from "./StartProgress";
import { useProjectFolders } from "./useProjectFolders";
import { RunSummary } from "../tasks/api";
import {
  AGENT_NAMES,
  AgentKind,
  PaneNode,
  CageView,
  CredentialSource,
  VmCage,
  SessionView,
  TerminalKind,
} from "../../shared/api";
import { libraryCredentials } from "../settings/api";
import { time } from "../settings/HostAgentsSettings";
import { decide, ruleMatches, useCatalog } from "../network/network";
import type { Intent, Pane, Run, Runtimes, TabView, View } from "../../App";
import { NetworkTab } from "../network/NetworkTab";
import { SettingsTab } from "../settings/SettingsTab";
import { TaskDialog } from "../tasks/TaskDialog";
import { TerminalTab } from "../terminal";
import { Notice } from "../../shared/Notice";
import { tildePath } from "../../shared/paths";
import { LaunchAction, launchActions, projectEditors } from "../launcher/actions";
import { Editors } from "../launcher/api";
import { launchMenu, SplitLaunch } from "../launcher/SplitLaunch";
import { EditorIcon } from "../launcher/EditorIcon";

// React Flow and the run room are large and not on the first screen of every project.
const ProjectMap = lazy(() => import("./ProjectMap").then((m) => ({ default: m.ProjectMap })));
const TaskRoom = lazy(() => import("../tasks/TaskRoom").then((m) => ({ default: m.TaskRoom })));

/** The provider host of each host login. pi's depends on its login. */
const LOGIN_HOSTS: Partial<Record<AgentKind, string>> = {
  claude: "api.anthropic.com",
  codex: "chatgpt.com",
  gemini: "cloudcode-pa.googleapis.com",
};

const CLAUDE_TOKEN = "CLAUDE_CODE_OAUTH_TOKEN";

/** The step that adds a host agent to the cage, or changes or removes it. */
function AddAgent({ cage, detection, kind, run, onDone, onRefresh }: {
  cage: CageView;
  detection: Detection;
  kind: AgentKind;
  run: Run;
  onDone: () => void;
  onRefresh: () => Promise<unknown>;
}) {
  const agent = detection.agents.find((a) => a.kind === kind);
  const current = cage.hostAgents.find((a) => a.kind === kind);
  const catalog = useCatalog();
  const canLogin = agent?.login.state === "found" || agent?.login.state === "setupToken";
  const [profile, setProfile] = useState(current?.profile ?? agent?.profile === true);
  const [login, setLogin] = useState(current?.login ?? canLogin);
  const host = LOGIN_HOSTS[kind];
  const blocked = host === undefined || !decide(cage.network, host, catalog).allowed;
  const [allow, setAllow] = useState(true);
  const [token, setToken] = useState("");
  const [tokenSource, setTokenSource] = useState<CredentialSource>(cage.claudeToken?.library ? "library" : "local");
  const [libraryToken, setLibraryToken] = useState<boolean>();
  const claude = kind === "claude";
  useEffect(() => {
    if (!claude) return;
    libraryCredentials().then(
      (entries) => setLibraryToken(entries.some((e) => e.name === CLAUDE_TOKEN)),
      () => setLibraryToken(false),
    );
  }, [claude]);
  // A stored library token does not count for the local source: a switch to local needs a pasted token.
  const tokenReady =
    !cage.claudeToken?.library &&
    !!cage.claudeToken?.stored &&
    !!cage.claudeToken.expiresAt &&
    cage.claudeToken.expiresAt > Date.now();
  const allowRule = cage.network.rules.some(
    (rule) => rule.action === "allow" && ruleMatches(rule, "api.anthropic.com", catalog),
  );
  const exactAllow = cage.network.rules.some(
    (rule) => rule.action === "allow" && rule.target.kind === "host" && rule.target.host === "api.anthropic.com",
  );
  const exactBlock = cage.network.rules.some(
    (rule) => rule.action === "block" && rule.target.kind === "host" && rule.target.host === "api.anthropic.com",
  );
  const needsAllow = claude && !exactAllow && (cage.network.default === "block" || exactBlock);
  const blockedOverride = claude && allowRule && !decide(cage.network, "api.anthropic.com", catalog).allowed;
  if (!agent) return null;
  const name = AGENT_NAMES[kind];
  const save = () =>
    run(() => {
      if (claude && login && tokenSource === "library") {
        return setClaudeToken(cage.id, null, "library", false, profile).then(onDone);
      }
      if (claude && login && token.trim()) {
        return setClaudeToken(cage.id, token.trim(), "local", false, profile).then(onDone);
      }
      return putHostAgent(cage.id, { kind, profile, login }, !claude && login && blocked && allow).then(onDone);
    });

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
        {login && blocked && !claude && (
          <SpringCheckbox
            label={host ? `Allow ${host}, the provider host` : "Allow the provider host of the pi login"}
            checked={allow}
            onChange={(e) => setAllow(e.target.checked)}
          />
        )}
        {login && claude && (
          <>
            <SegmentedControl
              label="Setup token"
              options={[
                { value: "local", label: "Paste a token for this cage" },
                { value: "library", label: "Use library token" },
              ]}
              value={tokenSource}
              onChange={(source) => setTokenSource(source as CredentialSource)}
            />
            {tokenSource === "library" ? (
              <span className={libraryToken === false ? "warn" : "muted"}>
                {libraryToken === false
                  ? `The library has no ${CLAUDE_TOKEN}. Add it in Settings > Credentials first.`
                  : `The cage uses ${CLAUDE_TOKEN} from the credential library. A rotation there reaches this cage at once.`}
              </span>
            ) : (
              <>
                <span className="muted">Run this on your Mac and paste the token below.</span>
                <CopyField className="mono" value="claude setup-token" />
                <FloatingField label="Setup token" type="password" value={token} onChange={(e) => setToken(e.target.value)} />
              </>
            )}
            {cage.claudeToken?.expiresAt && (
              <span className={expiresSoon(cage.claudeToken.expiresAt) ? "warn" : "muted"}>
                expires {new Date(cage.claudeToken.expiresAt).toLocaleDateString()}
                {expiresSoon(cage.claudeToken.expiresAt) && " · run claude setup-token again"}
              </span>
            )}
            {needsAllow && (
              <Button
                variant="outline"
                onClick={() =>
                  run(() =>
                    putRule(cage.id, {
                      target: { kind: "host", host: "api.anthropic.com" },
                      action: "allow",
                    }).then(onRefresh),
                  )
                }
              >
                Allow api.anthropic.com
              </Button>
            )}
            {blockedOverride && <span className="warn">A Block rule overrides the Allow rule for api.anthropic.com.</span>}
          </>
        )}
        <div className="row end">
          {current && (
            <Button variant="danger" onClick={() => run(() => removeHostAgent(cage.id, kind).then(onDone))}>
              Remove
            </Button>
          )}
          <Button variant="ghost" onClick={onDone}>
            Cancel
          </Button>
          <Button
            onClick={save}
            disabled={
              login &&
              claude &&
              (needsAllow ||
                (tokenSource === "library" ? libraryToken !== true : !token.trim() && !tokenReady))
            }
          >
            {current ? "Save" : "Add"}
          </Button>
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
  session: SessionView;
  /** The effective cage: the config that the page edits. `null`: no cage. */
  cage: CageView | null;
  /** The cage of the session's VM: the page shows its state. */
  vmCage: VmCage | null;
  /** All cages. */
  cages: CageView[];
  /** The default cage of the session's workspace. */
  defaultCage?: CageView;
  /** The network events of the session's VM, oldest first. */
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
  /** Reads the sessions again. Resolves when the new state is set. */
  onRefresh: () => Promise<unknown>;
  panes: Pane[];
  /** The focused pane of this project. */
  focused?: string;
  run: Run;
  onToggle: () => void;
  /** `agent` opens the project's first agent. */
  onNewSession: (kind: TerminalKind | "agent") => void;
  /** Opens a dormant or exited terminal again. */
  onReopen: (pane: Pane) => void;
  onFocus: (key: string) => void;
  onLayout: (layout: PaneNode) => void;
  onPaneExit: (processId: string, code: number) => void;
  onPaneError: (error: unknown) => void;
  /** Splits the pane of a terminal with a Mac shell. */
  onSplit: (key: string, direction: "row" | "column") => void;
  /** Opens the terminal's current folder in the default editor. */
  onOpenEditor: (key: string) => void;
  /** Opens the project folder in the default editor of the project. It saves no choice. */
  onOpenDefaultEditor: () => void;
  /** Hides the pane of a terminal. Its process keeps running. */
  onHide: (key: string) => void;
  editors?: Editors;
  /** The home folder, shown as `~`. */
  home?: string;
  onLaunch: (action: LaunchAction) => void;
  onEditorSettings: () => void;
  onProjectEditor: (sessionId: string, editor: string | null) => Promise<void>;
  /** Another screen fills the main area. Only the hidden terminals stay mounted, so their titles stay live. */
  covered: boolean;
}

// Not keyed by project: the terminal view keeps the panes of every project attached.
export function ProjectPage({
  session,
  cage,
  vmCage,
  cages,
  defaultCage,
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
  onProjectEditor,
  onOpenDefaultEditor,
  covered,
  ...terminal
}: Props) {
  const status = vmCage?.status ?? { state: "stopped" };
  const { folders, reload } = useProjectFolders(session);
  /** The host notices that the user dismissed in the current running period of a project. */
  const [dismissed, setDismissed] = useState<{ session: string; notices: string[] }>();
  if (dismissed?.session === session.id && status.state !== "running") setDismissed(undefined);
  const hidden = dismissed?.session === session.id ? dismissed.notices : [];
  // The tab that shows again after a terminal or task view. The tabs stay
  // mounted meanwhile, so a draft in them is kept.
  const [tab, setTab] = useState<TabView>("overview");
  const isTab = view.kind === "overview" || view.kind === "network" || view.kind === "settings";
  if (isTab && view.kind !== tab) setTab(view.kind);

  const edit: Run = (action) =>
    run(() =>
      action().then((result) => {
        reload();
        return result;
      }),
    );

  const running = status.state === "running" || status.state === "stopping";
  const sessions = session.processes.filter((s) => s.exitCode === null).length;
  const jump = (kind: TabView) => onView({ kind });
  const unverified = [
    ...(session.folderId ? [] : [session.folder]),
    ...(cage?.mounts ?? []).filter((m) => !m.id).map((m) => m.host),
  ];
  const hostAgents = cage?.hostAgents ?? [];
  const noCage = (
    <Notice icon={<Info size={14} className="muted" />} actions={<Button size="sm" variant="outline" onClick={() => jump("settings")}>Pick a cage</Button>}>
      This project has no cage. Its shells and agents run on this Mac, and nothing isolates them. Pick a cage in
      Settings, or a default cage for its workspace.
    </Notice>
  );
  const task = view.kind === "task" ? session.tasks.find((t) => t.id === view.taskId) : undefined;
  const ownEditors = projectEditors(editors, session);
  const actions = launchActions({ session, cage, vmCage }, detection, ownEditors);
  const newTerminals = actions.filter((a) => a.type === "session");
  const editorActions = actions.filter((a) => a.type === "editor");
  const defaultEditor = editorActions.find((a) => a.editor.id === ownEditors?.default);

  return (
    <section className="page" hidden={covered}>
      {!covered && (
        <>
          <header className="page-header row" style={{ "--zw-project-accent": session.accent ?? undefined } as CSSProperties}>
            <div className="grow">
              <h1 className="page-title project-title">{session.name}</h1>
              <p className="muted row">
                <span className={`dot ${status.state}`} />
                {!cage && !vmCage ? (
                  "no cage"
                ) : status.state === "error" ? (
                  <span className="danger">{status.message}</span>
                ) : status.state === "starting" ? (
                  <StartProgress phase={status.phase} />
                ) : (
                  status.state
                )}
                {cage && <span>· cage {cage.name}{session.effectiveCage?.inherited ? " (inherited)" : ""}</span>}
                {!cage && vmCage && <span>· no cage</span>}
                {status.state === "running" && vmCage?.loginExpiresAt != null && <span>· login valid until {time(vmCage.loginExpiresAt)}</span>}
              </p>
              <CopyField className="mono" value={session.folder} display={tildePath(session.folder, home)} title={session.folder} />
            </div>
            <SplitLaunch
              label="New terminal"
              icon={<Laptop size={14} />}
              onClick={() => onLaunch(newTerminals[0])}
              menuLabel="New terminal"
              menu={[
                ...launchMenu(newTerminals, onLaunch),
                // Host agents are set up here, next to the agents that they add.
                ...(cage ? (detection?.agents ?? []) : []).map((agent) => ({
                  label: `${hostAgents.some((a) => a.kind === agent.kind) ? "Change" : "Add"} ${AGENT_NAMES[agent.kind]} from this Mac…`,
                  icon: <Plus size={14} />,
                  run: () => onAdding(agent.kind),
                })),
              ]}
            />
            {editors && (
              <SplitLaunch
                label={defaultEditor ? `Open in ${defaultEditor.editor.name}` : "No editor"}
                icon={<EditorIcon icon={defaultEditor?.editor.icon ?? null} />}
                onClick={defaultEditor && onOpenDefaultEditor}
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
              disabled={(!cage && !running) || status.state === "starting"}
              title={cage || running ? undefined : "This project has no cage."}
              onClick={onToggle}
            >
              {status.state === "starting" ? <StartProgress phase={status.phase} /> : running ? "Stop cage" : "Start cage"}
            </Button>
          </header>
          {adding && detection && cage && (
            <AddAgent
              key={adding}
              cage={cage}
              detection={detection}
              kind={adding}
              run={run}
              onRefresh={onRefresh}
              onDone={() => onAdding(undefined)}
            />
          )}
          <div className="notices">
            {unverified.length > 0 && (
              <Notice
                icon={<TriangleAlert size={14} className="warn" />}
                actions={
                  <Button size="sm" variant="outline" onClick={() => run(() => confirmPaths(session.id).then(onRefresh))}>
                    Confirm folders
                  </Button>
                }
              >
                Not verified since the update: <span className="mono">{unverified.join(", ")}</span>. Check that each one is
                the folder that you added, then confirm it.
              </Notice>
            )}
            {status.state === "running" &&
              vmCage?.hostNotices
                .filter((notice) => !hidden.includes(notice))
                .map((notice) => (
                  <Notice
                    key={notice}
                    icon={<Info size={14} className="muted" />}
                    onDismiss={() => setDismissed({ session: session.id, notices: [...hidden, notice] })}
                  >
                    {notice}
                  </Notice>
                ))}
            {status.state === "running" &&
              vmCage?.guestLogins.map((agent) => (
                <Notice
                  key={agent}
                  icon={<TriangleAlert size={14} className="warn" />}
                  actions={
                    <>
                      <Button size="sm" variant="outline" onClick={() => run(() => removeGuestLogin(vmCage.key, agent).then(onRefresh))}>
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
                    {session.terminals.length === 0 && (
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
                        {hostAgents.length === 0 && detection && detection.agents.length > 0
                          ? "Next: add an agent from this Mac in the New terminal menu, or open the built-in pi agent."
                          : "Next: open an agent, or a shell on this Mac in the project folder."}
                      </Notice>
                    )}
                    {cage ? (
                      <Suspense>
                        <ProjectMap
                          key={session.id}
                          session={session}
                          cage={cage}
                          vmCage={vmCage}
                          folders={folders}
                          events={events}
                          visible={isTab && tab === "overview"}
                          run={run}
                          onJump={jump}
                          onDropFolder={onDropFolder}
                        />
                      </Suspense>
                    ) : (
                      noCage
                    )}
                    <dl className="facts">
                      <div>
                        <dt>Agent starts in</dt>
                        <dd className="mono">{session.effectiveCage ? (session.guestHarness ?? session.harness ?? "/workspace") : session.folder}</dd>
                      </div>
                      {cage && (
                        <div>
                          <dt>Network</dt>
                          <dd>
                            {cage.network.offline
                              ? "Offline"
                              : `${cage.network.default === "allow" ? "Allow" : "Block"} everything else, ${cage.network.rules.length} rules`}
                          </dd>
                        </div>
                      )}
                      <div>
                        <dt>Sessions</dt>
                        <dd>{sessions}</dd>
                      </div>
                      {hostAgents.length > 0 && (
                        <div>
                          <dt>From this Mac</dt>
                          <dd>
                            {hostAgents
                              .map((a) =>
                                [
                                  AGENT_NAMES[a.kind],
                                  a.profile && "profile",
                                  a.login &&
                                    (a.kind === "claude" &&
                                    (!cage?.claudeToken?.stored ||
                                      !cage.claudeToken.expiresAt ||
                                      cage.claudeToken.expiresAt <= Date.now())
                                      ? "needs token"
                                      : "login"),
                                ]
                                  .filter(Boolean)
                                  .join(" + "),
                              )
                              .join(", ")}
                          </dd>
                        </div>
                      )}
                    </dl>
                  </div>
                ),
                network: !cage && !vmCage ? (
                  <div className="tab-body">{noCage}</div>
                ) : (
                  <NetworkTab
                    key={session.id}
                    session={session}
                    cage={cage}
                    vmCage={vmCage}
                    noCage={noCage}
                    events={events}
                    run={edit}
                    intent={tab === "network" ? intent : undefined}
                    onIntentDone={onIntentDone}
                    active={isTab && tab === "network"}
                    onSeen={onNetSeen}
                    onStart={onToggle}
                  />
                ),
                settings: (
                  <SettingsTab
                    onProjectEditor={onProjectEditor}
                    key={session.id}
                    session={session}
                    cages={cages}
                    defaultCage={defaultCage}
                    folders={folders ?? []}
                    editors={editors}
                    runtimes={runtimes}
                    run={edit}
                    intent={tab === "settings" ? intent : undefined}
                    onIntentDone={onIntentDone}
                  />
                ),
              }}
            />
          {!cage && (view.kind === "task" || view.kind === "taskEdit") && <div className="view task-view">{noCage}</div>}
          {task && cage && view.kind === "task" && (
            <div className="view task-view">
              <Suspense>
                <TaskRoom
                  key={task.id}
                  kind="task"
                  session={session}
                  cageRunning={status.state === "running"}
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
          {cage && view.kind === "taskEdit" && (
            <div className="view task-view">
              <TaskDialog
                key={view.taskId ?? "new"}
                session={session}
                cage={cage}
                taskId={view.taskId}
                onDone={async (taskId) => {
                  // The tree and the task view need the saved task.
                  await onRefresh();
                  onView(taskId ? { kind: "task", taskId } : { kind: "overview" });
                }}
              />
            </div>
          )}
        </>
      )}
      <div className="view" hidden={view.kind !== "terminal"}>
        <TerminalTab
          session={session}
          status={status}
          panes={panes}
          focused={focused}
          editor={defaultEditor?.editor.name}
          home={home}
          visible={!covered && view.kind === "terminal"}
          {...terminal}
        />
      </div>
    </section>
  );
}
