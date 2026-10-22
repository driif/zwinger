import { CSSProperties, lazy, ReactNode, Suspense, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { Button } from "@driif/motiv";
import { Plus, RotateCw, Shield, TriangleAlert } from "lucide-react";
import { listNetEvents, NetEvent, onNetEvent } from "./features/network/api";
import { onStartup, Startup, startupStatus } from "./features/onboarding/api";
import {
  Detection,
  detectHostAgents,
  getPreferences,
  hostAgents,
  InstallProgress,
  installRuntime,
  listRuntimes,
  Prefs,
  RuntimeName,
  RuntimeView,
  setPreferences,
} from "./features/settings/api";
import {
  createShield,
  listShields,
  onGuestLogin,
  resolveFolder,
  startShield,
  stopShield,
  takeAppNotices,
} from "./features/shields/api";
import {
  isLive,
  isRoundFolder,
  listTaskRuns,
  RunSummary,
} from "./features/tasks/api";
import {
  closeTerminal,
  openSession,
  reopenTerminal,
  reorderTerminals,
} from "./features/terminal/api";
import { checkUpdate, UpdateInfo } from "./features/updates/api";
import {
  AGENT_NAMES,
  AgentKind,
  INVALID_ORDER,
  SessionKind,
  ShieldView,
} from "./shared/api";
import { AppSettings } from "./features/settings/AppSettings";
import { CommandBar } from "./shared/CommandBar";
import { Onboarding, StartupScreen } from "./features/onboarding/Onboarding";
import { ShieldPage } from "./features/shields/ShieldPage";
import { agentsOf, ShieldRail } from "./features/shields/ShieldRail";
import { fittedSize } from "./features/terminal/TerminalPane";
import { UpdateBanner } from "./features/updates/UpdateBanner";
import { Appearance, useAppearance } from "./features/appearance/themes";
import { clampRail, RailHandle } from "./features/shields/RailHandle";
import { Notice } from "./shared/Notice";
import { ShortcutAction, useShortcuts } from "./shared/useShortcuts";
import { errorCode, errorMessage, isBackendError } from "./shared/errorMessage";
import { LOST_TITLE, ReloadScreen, useBackendLost } from "./shared/Recover";

const TaskRoom = lazy(() => import("./features/tasks/TaskRoom").then((m) => ({ default: m.TaskRoom })));

/** The same size as the ring in the backend. */
const NET_RING = 2000;

export type TabView = "overview" | "network" | "settings";

/** What the main area shows for the selected shield. */
export type View =
  | { kind: TabView }
  /** No `key` shows the empty terminal view. */
  | { kind: "terminal"; key?: string }
  | { kind: "app"; appId: string }
  /** No `run` shows the latest run. */
  | { kind: "task"; taskId: string; run?: string }
  /** No `taskId` adds a task. */
  | { kind: "taskEdit"; taskId?: string };

/** A terminal of a shield. `key` is the terminal id. No `sessionId`: the terminal is dormant. */
export interface Pane {
  key: string;
  shieldId: string;
  kind: SessionKind;
  sessionId?: string;
}

export type Run = (action: () => Promise<unknown>) => Promise<unknown>;

/** A ⌘K action that a tab completes, such as focusing a field. `at` makes a repeated action new. */
export interface Intent {
  kind: "rename" | "mount" | "harness" | "delete" | "addRule" | "app";
  at: number;
  /** For `mount`: the canonical folder that was dropped on the map. */
  host?: string;
  /** For `mount`: the dropped path, which can be a link to `host`. */
  dropped?: string;
  /** For `app`: the app to edit. No `appId` adds an app. */
  appId?: string;
}

/** A dropped folder: the dropped path and its canonical folder. */
export interface DroppedFolder {
  dropped: string;
  folder: string;
}

/** The runtimes and their installs, for the views that offer Install. */
export interface Runtimes {
  list: RuntimeView[];
  /** The installs that run now. */
  installs: Partial<Record<RuntimeName, InstallProgress>>;
  /** No `version` installs the pinned version. `confirm` accepts a version with no pinned hash. */
  onInstall: (name: RuntimeName, version?: string, confirm?: boolean) => void;
}

/** The order of the start phases. A late reply must not take the phase back. */
const PHASE_ORDER: Record<Startup["phase"], number> = { checking: 0, installing: 1, ready: 2, blocked: 2 };

/** Shows the first-run steps once, then the shields when the runtime is ready. */
export default function App() {
  const [startup, setStartup] = useState<Startup>();
  const [prefs, setPrefs] = useState<Prefs>();
  const [firstShield, setFirstShield] = useState<string>();
  const [error, setError] = useState<string>();
  const [update, setUpdate] = useState<UpdateInfo>();
  const [updateLater, setUpdateLater] = useState(false);
  const lost = useBackendLost();
  const appearance = useAppearance(prefs);
  /** The start needs both of its first calls, so one failure that is not a backend error shows Reload. */
  const [startLost, setStartLost] = useState(false);

  const advance = (next: Startup) =>
    setStartup((current) => (current && PHASE_ORDER[current.phase] > PHASE_ORDER[next.phase] ? current : next));

  const fail = (e: unknown) => (isBackendError(e) ? setError(e.message) : setStartLost(true));

  // The read starts after the listener, so no phase change falls between the two.
  useEffect(() => {
    const stop = onStartup(advance);
    stop.then(startupStatus).then(advance, fail);
    // Here, not in the workspace, so a blocked start can still update. Quiet: offline, the check
    // fails with no message. Check now in Settings shows the error.
    getPreferences().then((loaded) => {
      setPrefs(loaded);
      if (loaded.autoUpdate) checkUpdate().then((found) => setUpdate(found ?? undefined), () => {});
    }, fail);
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  const savePrefs = async (next: Prefs) => {
    await setPreferences(next);
    setPrefs(next);
  };

  if (lost || startLost) return <ReloadScreen title={LOST_TITLE} />;
  if (error) return <p className="empty danger">{error}</p>;
  if (!startup || !prefs) return null;
  const banner = update && !updateLater && <UpdateBanner update={update} onLater={() => setUpdateLater(true)} />;
  if (!prefs.onboarded)
    return (
      <Onboarding
        startup={startup}
        banner={banner}
        onDone={async (shieldId) => {
          // Set first: the workspace reads it once, when it mounts.
          setFirstShield(shieldId);
          await savePrefs({ ...prefs, onboarded: true });
        }}
      />
    );
  if (startup.phase !== "ready") return <StartupScreen startup={startup} banner={banner} />;
  return (
    <Workspace
      firstShield={firstShield}
      prefs={prefs}
      onPrefs={savePrefs}
      banner={banner}
      update={update}
      onUpdate={setUpdate}
      appearance={appearance}
    />
  );
}

interface WorkspaceProps {
  /** The shield that the first-run steps made. */
  firstShield?: string;
  prefs: Prefs;
  onPrefs: (prefs: Prefs) => Promise<void>;
  /** The update banner of the app. */
  banner: ReactNode;
  update?: UpdateInfo;
  onUpdate: (update?: UpdateInfo) => void;
  appearance: Appearance;
}

function Workspace({ firstShield, prefs, onPrefs, banner, update, onUpdate, appearance }: WorkspaceProps) {
  const [shields, setShields] = useState<ShieldView[]>([]);
  const [selectedId, setSelectedId] = useState(firstShield);
  const [view, setView] = useState<View>({ kind: "overview" });
  const [activePanes, setActivePanes] = useState<Record<string, string>>({});
  const [error, setError] = useState<string>();
  const [appNotices, setAppNotices] = useState<string[]>([]);
  const [intent, setIntent] = useState<Intent>();
  const commandBar = useRef<HTMLDivElement>(null);
  const refreshes = useRef(0);
  const [net, setNet] = useState<Record<string, NetEvent[]>>({});
  /** Per shield, the last `seq` that the Network tab showed. */
  const [lastSeen, setLastSeen] = useState<Record<string, number>>({});
  const [listening, setListening] = useState(false);
  const backfilled = useRef(new Set<string>());
  const [runs, setRuns] = useState<Record<string, RunSummary[]>>({});
  /** A duo round folder on the host that the main area shows instead of a shield. */
  const [roundFolder, setRoundFolder] = useState<string>();
  const [dropFolder, setDropFolder] = useState<DroppedFolder>();
  const [runtimeList, setRuntimeList] = useState<RuntimeView[]>([]);
  const [installs, setInstalls] = useState<Runtimes["installs"]>({});
  /** The agents on this Mac. `undefined` while a detection runs. */
  const [detection, setDetection] = useState<Detection>();
  /** The main area shows the app settings instead of a shield. */
  const [showSettings, setShowSettings] = useState(false);
  /** Per shield, the host agent that its Add agent step shows. */
  const [adding, setAdding] = useState<Record<string, AgentKind | undefined>>({});
  const [railWidth, setRailWidth] = useState(() => clampRail(prefs.railWidth));

  // Replies can arrive out of order. Only the reply to the latest request is used. Resolves to the list it read.
  const refresh = () => {
    const request = ++refreshes.current;
    return listShields().then(
      (next) => {
        if (request === refreshes.current) setShields(next);
        return next;
      },
      (e) => {
        if (request === refreshes.current) setError(errorMessage(e));
      },
    );
  };

  const run: Run = (action) => {
    setError(undefined);
    const done = action().then(refresh, (e) => {
      setError(errorMessage(e));
      return refresh();
    });
    refresh();
    return done;
  };

  const reloadRuntimes = () => listRuntimes().then(setRuntimeList, (e) => setError(errorMessage(e)));

  // Detection runs at app start and on request, never in a loop.
  const detect = (fresh: boolean) => {
    setDetection(undefined);
    (fresh ? detectHostAgents() : hostAgents()).then(setDetection, (e) => setError(errorMessage(e)));
  };

  useEffect(() => {
    refresh();
    reloadRuntimes();
    detect(false);
  }, []);

  // An agent login was made in a shield, or it is gone.
  useEffect(() => {
    const stop = onGuestLogin(() => refresh());
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  useEffect(() => {
    takeAppNotices().then(setAppNotices, (e) => setError(errorMessage(e)));
  }, []);

  const runtimes: Runtimes = {
    list: runtimeList,
    installs,
    onInstall: (name, version, confirm) =>
      run(async () => {
        setInstalls((current) => ({ ...current, [name]: { stage: "download", received: 0, total: null } }));
        try {
          await installRuntime(
            name,
            (progress) => setInstalls((current) => ({ ...current, [name]: progress })),
            version ?? null,
            confirm ?? false,
          );
        } finally {
          setInstalls((current) => ({ ...current, [name]: undefined }));
          await reloadRuntimes();
          // The runtime source of a host agent changes.
          if (version) detect(true);
        }
      }),
  };

  // Merged by seq: the live events and the backfill can overlap.
  const addNet = (shieldId: string, events: NetEvent[]) =>
    setNet((current) => {
      const bySeq = new Map((current[shieldId] ?? []).map((e) => [e.seq, e]));
      for (const event of events) bySeq.set(event.seq, event);
      return { ...current, [shieldId]: [...bySeq.values()].sort((a, b) => a.seq - b.seq).slice(-NET_RING) };
    });

  useEffect(() => {
    const stop = onNetEvent((shieldId, event) => addNet(shieldId, [event]));
    stop.then(() => setListening(true));
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  // The backfill starts after the listener, so no event falls between the two.
  useEffect(() => {
    if (!listening) return;
    for (const { id } of shields) {
      if (backfilled.current.has(id)) continue;
      backfilled.current.add(id);
      listNetEvents(id).then((events) => addNet(id, events), () => {});
    }
  }, [shields, listening]);

  const reloadRuns = (id: string) =>
    listTaskRuns(id).then((list) => setRuns((current) => ({ ...current, [id]: list })), () => {});

  const shieldIds = shields.map((s) => s.id).join(" ");
  useEffect(() => {
    for (const { id } of shields) if (!(id in runs)) reloadRuns(id);
  }, [shieldIds]);

  // The tree shows the stage of each live run, so the list polls while one runs.
  const liveRunShields = Object.keys(runs).filter((id) => runs[id].some((r) => isLive(r.state))).join(" ");
  useEffect(() => {
    if (!liveRunShields) return;
    const timer = setInterval(() => liveRunShields.split(" ").forEach(reloadRuns), 2000);
    return () => clearInterval(timer);
  }, [liveRunShields]);

  // An app can exit while no view is attached, so the list polls while one runs.
  const appsRunning = shields.some((s) => s.appRuns.some((r) => r.exitCode === null));
  useEffect(() => {
    if (!appsRunning) return;
    const timer = setInterval(refresh, 2000);
    return () => clearInterval(timer);
  }, [appsRunning]);

  const panes: Pane[] = shields.flatMap((s) =>
    s.terminals.map((t) => ({
      key: t.id,
      shieldId: s.id,
      kind: t.kind,
      sessionId: s.sessions.find((session) => session.terminalId === t.id)?.id,
    })),
  );
  const shield = shields.find((s) => s.id === selectedId) ?? shields[0];
  const shieldPanes = panes.filter((p) => p.shieldId === shield?.id);
  const activePane = shield && activePanes[shield.id];
  const events = (shield && net[shield.id]) ?? [];

  // A deleted app or task has no view.
  useEffect(() => {
    if (view.kind === "app" && shield && !shield.apps.some((a) => a.id === view.appId)) setView({ kind: "overview" });
    if (view.kind === "task" && shield && !shield.tasks.some((t) => t.id === view.taskId)) setView({ kind: "overview" });
  }, [view, shield]);

  const blocked = Object.fromEntries(
    shields.map(({ id }) => [id, (net[id] ?? []).filter((e) => !e.allowed && e.seq > (lastSeen[id] ?? 0)).length]),
  );

  function showPane(key: string | undefined, shieldId = shield?.id) {
    if (!shieldId) return;
    setActivePanes((current) => ({ ...current, [shieldId]: key ?? "" }));
    setView({ kind: "terminal", key });
  }

  /** Items belong to one shield, so another shield shows its Overview instead. */
  function select(id: string) {
    setRoundFolder(undefined);
    setShowSettings(false);
    setSelectedId(id);
    if (id === shield?.id || !["overview", "network", "settings"].includes(view.kind)) setView({ kind: "overview" });
  }

  function openView(shieldId: string, next: View) {
    setRoundFolder(undefined);
    setShowSettings(false);
    setSelectedId(shieldId);
    if (next.kind === "terminal") showPane(next.key, shieldId);
    else setView(next);
  }

  async function addShield(folder: string) {
    setSelectedId(await createShield(folder));
    setView({ kind: "overview" });
  }

  async function openRoundFolder() {
    const folder = await open({ directory: true, title: "Choose a duo round folder, such as docs/tasks/<slug>/<round>" });
    if (folder !== null) setRoundFolder(folder);
  }

  async function newShield() {
    const folder = await open({ directory: true, title: "Choose the shield folder" });
    if (folder !== null) await addShield(folder);
  }

  /** Opens a shell, or the first agent of the shield: host agents come first. */
  async function newSession(kind: SessionKind | "agent", target = shield) {
    if (!target) return;
    // A stopped shield does not know its mounted agents. Read them after the start.
    if (target.status.state !== "running") {
      await startShield(target.id);
      target = (await listShields()).find((s) => s.id === target!.id) ?? target;
    }
    const mounted = agentsOf(target);
    const session: SessionKind = kind === "agent" ? { type: "agent", agent: mounted[0], resume: false } : kind;
    if (session.type === "agent" && !mounted.includes(session.agent))
      throw new Error(`${session.agent ? AGENT_NAMES[session.agent] : "No agent"} is not mounted in this shield.`);
    const { rows, cols } = fittedSize();
    const sessionId = await openSession(target.id, session, rows, cols);
    const terminalId = (await refresh())?.flatMap((s) => s.sessions).find((s) => s.id === sessionId)?.terminalId;
    setSelectedId(target.id);
    if (terminalId) showPane(terminalId, target.id);
  }

  /** Starts a new process for a dormant or exited terminal, in its place. */
  async function reopen(pane: Pane) {
    if (shields.find((s) => s.id === pane.shieldId)?.status.state !== "running") await startShield(pane.shieldId);
    const { rows, cols } = fittedSize();
    await reopenTerminal(pane.shieldId, pane.key, rows, cols);
    await refresh();
    showPane(pane.key, pane.shieldId);
  }

  // A duo round folder dropped anywhere opens its room. Another folder dropped on
  // empty rail space creates a shield. The map has its own listener.
  useEffect(() => {
    const stop = getCurrentWebview().onDragDropEvent(async ({ payload }) => {
      if (payload.type !== "drop" || payload.paths.length === 0) return;
      const { x, y } = payload.position.toLogical(window.devicePixelRatio);
      const element = document.elementFromPoint(x, y);
      const dropped = payload.paths[0];
      if (await isRoundFolder(dropped)) {
        setRoundFolder(dropped);
        return;
      }
      if (!element?.closest("[data-drop=rail]") || element.closest("[role=treeitem]")) return;
      setError(undefined);
      resolveFolder(dropped).then((folder) => setDropFolder({ dropped, folder }), (e) => setError(errorMessage(e)));
    });
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  const dropMount = (dropped: string) =>
    run(async () => {
      const host = await resolveFolder(dropped);
      setView({ kind: "settings" });
      setIntent({ kind: "mount", host, dropped, at: Date.now() });
    });

  function cycle(step: number) {
    if (shieldPanes.length === 0) return;
    const index = shieldPanes.findIndex((p) => p.key === activePane);
    showPane(shieldPanes[(index + step + shieldPanes.length) % shieldPanes.length].key);
  }

  function closePane(key: string) {
    const pane = panes.find((p) => p.key === key);
    if (!pane) return;
    run(() => closeTerminal(pane.shieldId, key));
    const rest = shieldPanes.filter((p) => p !== pane);
    if (key === activePane && view.kind === "terminal") showPane(rest[rest.length - 1]?.key);
    else if (key === activePane) setActivePanes((current) => ({ ...current, [pane.shieldId]: "" }));
  }

  /** Saves the order of the shield's terminals. A terminal that opened or closed during the drag fails the order: the tree then shows the saved order. */
  const moveTerminals = (shieldId: string, ids: string[]) =>
    reorderTerminals(shieldId, ids).then(refresh, (e) => {
      if (errorCode(e) !== INVALID_ORDER) setError(errorMessage(e));
      return refresh();
    });

  const toggle = (shield: ShieldView) =>
    run(() => (shield.status.state === "running" ? stopShield(shield.id) : startShield(shield.id)));

  function startIntent(kind: Intent["kind"], appId?: string) {
    setView({ kind: kind === "addRule" ? "network" : "settings" });
    setIntent({ kind, appId, at: Date.now() });
  }

  const restart = (id: string) => run(() => stopShield(id).then(() => startShield(id)));

  const actions: Record<ShortcutAction, () => void> = {
    // The command menu has no open prop. A click with no pointer opens it and focuses the first item.
    commands: () => commandBar.current?.querySelector("button")?.click(),
    newShield: () => run(newShield),
    newShell: () => run(() => newSession({ type: "shell" })),
    newAgent: () => run(() => newSession("agent")),
    close: () => view.kind === "terminal" && activePane && closePane(activePane),
    next: () => cycle(1),
    previous: () => cycle(-1),
    overview: () => setView({ kind: "overview" }),
    agents: () => {
      const agents = shieldPanes.filter((p) => p.kind.type === "agent");
      if (agents.length > 0) showPane(agents[agents.length - 1].key);
      else run(() => newSession("agent"));
    },
    terminal: () => showPane(activePane || shieldPanes[shieldPanes.length - 1]?.key),
    network: () => setView({ kind: "network" }),
    settings: () => setView({ kind: "settings" }),
    appSettings: () => setShowSettings(true),
  };
  useShortcuts(actions);

  return (
    <div className="app" style={{ "--zw-rail-width": `${railWidth}px` } as CSSProperties}>
      <ShieldRail
        shields={shields}
        selectedId={shield?.id}
        view={view}
        blocked={blocked}
        runs={runs}
        panes={panes}
        run={run}
        onSelect={select}
        onOpen={openView}
        onIntent={(id, kind, appId) => {
          setSelectedId(id);
          startIntent(kind, appId);
        }}
        onRunsChange={reloadRuns}
        onToggle={toggle}
        onNewSession={(id, kind) => run(() => newSession(kind, shields.find((s) => s.id === id)))}
        onClosePane={closePane}
        onReopen={(pane) => run(() => reopen(pane))}
        onMoveTerminals={moveTerminals}
        onNew={actions.newShield}
        onSettings={actions.appSettings}
        dropFolder={dropFolder}
        onDropDone={(create) => {
          setDropFolder(undefined);
          if (create && dropFolder) run(() => addShield(dropFolder.folder));
        }}
      >
        <CommandBar
          ref={commandBar}
          shields={shields}
          shield={shield}
          panes={shieldPanes}
          activePane={activePane}
          actions={actions}
          onSelect={select}
          onShowPane={showPane}
          onView={(next) => {
            setRoundFolder(undefined);
            setView(next);
          }}
          onToggle={toggle}
          onRestart={restart}
          onIntent={startIntent}
          onOpenRound={() => run(openRoundFolder)}
          run={run}
        />
      </ShieldRail>
      <RailHandle
        width={railWidth}
        onResize={setRailWidth}
        onDone={(width) => onPrefs({ ...prefs, railWidth: width }).catch((e) => setError(errorMessage(e)))}
      />
      <main className="main">
        <div className="notices">
          {appNotices.map((notice) => (
            <Notice
              key={notice}
              icon={<TriangleAlert size={14} className="warn" />}
              onDismiss={() => setAppNotices((all) => all.filter((n) => n !== notice))}
            >
              {notice}
            </Notice>
          ))}
          {banner}
          {!showSettings && !roundFolder && shield?.pendingRestart && (
            <Notice
              icon={<RotateCw size={14} className="warn" />}
              actions={
                <Button size="sm" variant="outline" onClick={() => restart(shield.id)}>
                  Restart
                </Button>
              }
            >
              The running shield uses an older config. Restart it to apply the changes.
            </Notice>
          )}
        </div>
        {showSettings ? (
          <AppSettings
            detection={detection}
            shields={shields}
            runtimes={runtimes}
            run={run}
            onRefresh={() => detect(true)}
            onUse={(shieldId, agent) => {
              select(shieldId);
              setAdding((current) => ({ ...current, [shieldId]: agent.kind }));
            }}
            prefs={prefs}
            onPrefs={onPrefs}
            update={update}
            onUpdate={onUpdate}
            appearance={appearance}
          />
        ) : roundFolder ? (
          <section className="page">
            <div className="view task-view">
              <Suspense>
                <TaskRoom key={roundFolder} kind="folder" path={roundFolder} />
              </Suspense>
            </div>
          </section>
        ) : shield ? (
          <ShieldPage
            shield={shield}
            events={events}
            view={view}
            onView={setView}
            intent={intent}
            onIntentDone={() => setIntent(undefined)}
            onNetSeen={(seq) => setLastSeen((current) => ({ ...current, [shield.id]: seq }))}
            onDropFolder={dropMount}
            runtimes={runtimes}
            detection={detection}
            adding={adding[shield.id]}
            onAdding={(kind) => setAdding((current) => ({ ...current, [shield.id]: kind }))}
            onHostAgents={() => setShowSettings(true)}
            runs={runs[shield.id] ?? []}
            onRunsChange={() => reloadRuns(shield.id)}
            onRefresh={refresh}
            panes={panes}
            activePanes={activePanes}
            run={run}
            onToggle={() => toggle(shield)}
            onNewSession={(kind) => run(() => newSession(kind))}
            onReopen={(pane) => run(() => reopen(pane))}
            onPaneExit={refresh}
            onPaneError={(e) => setError(errorMessage(e))}
          />
        ) : (
          <div className="empty">
            <Shield size={40} className="muted" />
            <h1>No shields yet</h1>
            <p className="muted">
              A shield runs an AI agent in a microVM. The agent sees only the folders that you mount and reaches only
              the hosts that you allow.
            </p>
            <Button iconLeft={<Plus size={16} />} onClick={actions.newShield}>
              Create shield
            </Button>
          </div>
        )}
        {error && <p className="status danger mono">{error}</p>}
      </main>
    </div>
  );
}
