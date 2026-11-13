import { CSSProperties, lazy, ReactNode, Suspense, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { homeDir } from "@tauri-apps/api/path";
import { Button, Card } from "@driif/motiv";
import { Plus, RotateCw, ShieldOff, TriangleAlert } from "lucide-react";
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
  createSession,
  listSessions,
  listWorkspaces,
  onAgentState,
  onGuestLogin,
  onStartProgress,
  resolveFolder,
  restartCage,
  restartCageToAdd,
  RestartToAdd,
  startCage,
  stopCage,
  StopShared,
  takeAppNotices,
} from "./features/projects/api";
import {
  isLive,
  isRoundFolder,
  listTaskRuns,
  RunSummary,
} from "./features/tasks/api";
import {
  closeTerminal,
  Direction,
  fittedSize,
  leaves,
  neighbor,
  openSession,
  remove,
  reopenTerminal,
  reorderTerminals,
  replace,
  setLook,
  setPaneLayout,
  split,
  tooDeep,
} from "./features/terminal";
import { checkUpdate, UpdateInfo } from "./features/updates/api";
import {
  AGENT_NAMES,
  AgentKind,
  AgentState,
  INVALID_ORDER,
  PaneNode,
  CageView,
  SessionCage,
  SessionView,
  TerminalKind,
  // The component that shows the sessions is also named Workspace.
  Workspace as WorkspaceGroup,
} from "./shared/api";
import { AppSettings } from "./features/settings/AppSettings";
import { CommandBar } from "./shared/CommandBar";
import { Onboarding, StartupScreen } from "./features/onboarding/Onboarding";
import { ProjectPage } from "./features/projects/ProjectPage";
import { ProjectRail } from "./features/projects/ProjectRail";
import { agentsOf, LaunchAction, macAgent } from "./features/launcher/actions";
import { launchCage, sessionCage } from "./features/cages/sessionCage";
import { detectEditors, Editors, openInEditor, revealSession } from "./features/launcher/api";
import { tildePath } from "./shared/paths";
import { UpdateBanner } from "./features/updates/UpdateBanner";
import { Appearance, useAppearance } from "./features/appearance/themes";
import { clampRail, RailHandle } from "./features/projects/RailHandle";
import { Notice } from "./shared/Notice";
import { ShortcutAction, useShortcuts } from "./shared/useShortcuts";
import { errorCode, errorDetail, errorMessage, isBackendError } from "./shared/errorMessage";
import { LOST_TITLE, ReloadScreen, useBackendLost } from "./shared/Recover";
import { CageMark } from "./shared/CageMark";
import { CagesPage } from "./features/cages/CagesPage";

const TaskRoom = lazy(() => import("./features/tasks/TaskRoom").then((m) => ({ default: m.TaskRoom })));

/** The same size as the ring in the backend. */
const NET_RING = 2000;

export type TabView = "overview" | "network" | "settings";

/** What the main area shows for the selected session. */
export type View =
  | { kind: TabView }
  /** `key` is the focused pane. No `key`: no pane is focused. */
  | { kind: "terminal"; key?: string }
  /** No `run` shows the latest run. */
  | { kind: "task"; taskId: string; run?: string }
  /** No `taskId` adds a task. */
  | { kind: "taskEdit"; taskId?: string };

/** A terminal of a session. `key` is the terminal id. No `processId`: the terminal is dormant or has exited. */
export interface Pane {
  key: string;
  sessionId: string;
  kind: TerminalKind;
  processId?: string;
}

export type Run = (action: () => Promise<unknown>) => Promise<unknown>;

/** The start and stop of a session's VM. Each asks first when it restarts or stops the VM of other sessions. */
export interface VmControl {
  start: (sessionId: string) => Promise<void>;
  stop: (sessionId: string) => Promise<void>;
}

/** The user cancelled a confirmation: `run` shows no error. */
class Cancelled extends Error {}

/** A question that a start or stop asks before it continues. */
interface Ask {
  text: string;
  confirm: string;
  answer: (ok: boolean) => void;
}

/** A ⌘K action that a tab completes, such as focusing a field. `at` makes a repeated action new. */
export interface Intent {
  kind: "rename" | "mount" | "harness" | "delete" | "addRule";
  at: number;
  /** For `mount`: the canonical folder that was dropped on the map. */
  host?: string;
  /** For `mount`: the dropped path, which can be a link to `host`. */
  dropped?: string;
}

/** A dropped folder: the dropped path, its canonical folder and the workspace of the row it was dropped on. */
export interface DroppedFolder {
  dropped: string;
  folder: string;
  workspace?: string;
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

/** Shows the first-run steps once, then the projects when the runtime is ready. */
export default function App() {
  const [startup, setStartup] = useState<Startup>();
  const [prefs, setPrefs] = useState<Prefs>();
  const [firstSession, setFirstSession] = useState<string>();
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
        onEditor={(id) => savePrefs({ ...prefs, editors: { ...prefs.editors, default: id } })}
        onDone={async (sessionId) => {
          // Set first: the workspace reads it once, when it mounts.
          setFirstSession(sessionId);
          await savePrefs({ ...prefs, onboarded: true });
        }}
      />
    );
  if (startup.phase !== "ready") return <StartupScreen startup={startup} banner={banner} />;
  return (
    <Workspace
      firstSession={firstSession}
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
  /** The session that the first-run steps made. */
  firstSession?: string;
  prefs: Prefs;
  onPrefs: (prefs: Prefs) => Promise<void>;
  /** The update banner of the app. */
  banner: ReactNode;
  update?: UpdateInfo;
  onUpdate: (update?: UpdateInfo) => void;
  appearance: Appearance;
}

function Workspace({ firstSession, prefs, onPrefs, banner, update, onUpdate, appearance }: WorkspaceProps) {
  const [sessions, setSessions] = useState<SessionView[]>([]);
  const [cages, setCages] = useState<CageView[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceGroup[]>([]);
  const [selectedId, setSelectedId] = useState(firstSession);
  const [view, setView] = useState<View>({ kind: "overview" });
  /** Per session, the terminal of the pane that has the focus. */
  const [focusedPanes, setFocusedPanes] = useState<Record<string, string>>({});
  const [error, setError] = useState<string>();
  const [appNotices, setAppNotices] = useState<string[]>([]);
  const [ask, setAsk] = useState<Ask>();
  const [intent, setIntent] = useState<Intent>();
  const commandBar = useRef<HTMLDivElement>(null);
  const refreshes = useRef(0);
  const agentVersion = useRef(0);
  const agentEvents = useRef(new Map<string, { state: AgentState | null; version: number }>());
  const [net, setNet] = useState<Record<string, NetEvent[]>>({});
  /** Per cage, the last `seq` that the Network tab showed. */
  const [lastSeen, setLastSeen] = useState<Record<string, number>>({});
  const [listening, setListening] = useState(false);
  const backfilled = useRef(new Set<string>());
  const [runs, setRuns] = useState<Record<string, RunSummary[]>>({});
  /** A duo round folder on the host that the main area shows instead of a session. */
  const [roundFolder, setRoundFolder] = useState<string>();
  const [dropFolder, setDropFolder] = useState<DroppedFolder>();
  const [runtimeList, setRuntimeList] = useState<RuntimeView[]>([]);
  const [installs, setInstalls] = useState<Runtimes["installs"]>({});
  /** The agents on this Mac. `undefined` while a detection runs. */
  const [detection, setDetection] = useState<Detection>();
  /** The agent whose open with no cage waits for the warning's answer. */
  const [noCage, setNoCage] = useState<AgentKind>();
  const noCageAnswer = useRef<(ok: boolean) => void>(undefined);
  /** The tab of the app settings, which the main area shows instead of a session. */
  const [settingsTab, setSettingsTab] = useState<string>();
  const showSettings = settingsTab !== undefined;
  /** The list of cages, which the main area shows instead of a session. */
  const [showCages, setShowCages] = useState(false);
  /** The editors on this Mac. `undefined` until the first detection. */
  const [editors, setEditors] = useState<Editors>();
  const [home, setHome] = useState<string>();
  /** Per session, the host agent that its Add agent step shows. */
  const [adding, setAdding] = useState<Record<string, AgentKind | undefined>>({});
  const [railWidth, setRailWidth] = useState(() => clampRail(prefs.railWidth));

  // As the workspace renders, before its terminals mount: they take the look when they open.
  const terminalTheme = appearance.themes.find((t) => t.id === prefs.terminal.themeId) ?? appearance.theme;
  setLook(prefs.terminal, terminalTheme.terminal);

  // Replies can arrive out of order. Only the reply to the latest request is used. Resolves to the sessions it read.
  const refresh = () => {
    const request = ++refreshes.current;
    const version = agentVersion.current;
    return Promise.all([listSessions(), listWorkspaces()]).then(
      ([next, groups]) => {
        if (request === refreshes.current) {
          next.sessions = next.sessions.map((session) => ({
            ...session,
            processes: session.processes.map((process) => {
              const event = agentEvents.current.get(process.id);
              return event && event.version > version ? { ...process, agent: event.state } : process;
            }),
          }));
          setSessions(next.sessions);
          setCages(next.cages);
          setWorkspaces(groups);
          for (const [id, event] of agentEvents.current) {
            if (event.version <= version) agentEvents.current.delete(id);
          }
        }
        return next.sessions;
      },
      (e) => {
        if (request === refreshes.current) setError(errorMessage(e));
      },
    );
  };

  const run: Run = (action) => {
    setError(undefined);
    const done = action().then(refresh, (e) => {
      if (!(e instanceof Cancelled)) setError(errorMessage(e));
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

  const loadEditors = () => detectEditors().then(setEditors, (e) => setError(errorMessage(e)));

  useEffect(() => {
    reloadRuntimes();
    detect(false);
    loadEditors();
    homeDir().then(setHome, () => {});
  }, []);

  /** The editors that are on and the default come from the prefs, so they are read again. */
  const savePrefs = async (next: Prefs) => {
    await onPrefs(next);
    await loadEditors();
  };

  // An agent login was made in a cage, or it is gone.
  useEffect(() => {
    const stop = onGuestLogin(() => refresh());
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  useEffect(() => {
    const stop = onAgentState((processId, state) => {
      agentEvents.current.set(processId, { state, version: ++agentVersion.current });
      setSessions((current) =>
        current.map((session) => ({
          ...session,
          processes: session.processes.map((process) =>
            process.id === processId ? { ...process, agent: state } : process,
          ),
        })),
      );
    });
    stop.then(() => refresh(), () => refresh());
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  // The phase of a start that runs. A late event cannot undo a later state, such as running.
  useEffect(() => {
    const stop = onStartProgress((cageId, phase) =>
      setCages((current) =>
        current.map((c) =>
          c.id === cageId && ["stopped", "starting", "error"].includes(c.status.state)
            ? { ...c, status: { state: "starting", phase } }
            : c,
        ),
      ),
    );
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
  const addNet = (cageId: string, events: NetEvent[]) =>
    setNet((current) => {
      const bySeq = new Map((current[cageId] ?? []).map((e) => [e.seq, e]));
      for (const event of events) bySeq.set(event.seq, event);
      return { ...current, [cageId]: [...bySeq.values()].sort((a, b) => a.seq - b.seq).slice(-NET_RING) };
    });

  useEffect(() => {
    const stop = onNetEvent((cageId, event) => addNet(cageId, [event]));
    stop.then(() => setListening(true));
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  // The backfill starts after the listener, so no event falls between the two.
  useEffect(() => {
    if (!listening) return;
    for (const { id } of cages) {
      if (backfilled.current.has(id)) continue;
      backfilled.current.add(id);
      listNetEvents(id).then((events) => addNet(id, events), () => {});
    }
  }, [cages, listening]);

  const reloadRuns = (id: string) =>
    listTaskRuns(id).then((list) => setRuns((current) => ({ ...current, [id]: list })), () => {});

  const sessionIds = sessions.map((s) => s.id).join(" ");
  useEffect(() => {
    for (const { id } of sessions) if (!(id in runs)) reloadRuns(id);
  }, [sessionIds]);

  // The tree shows the stage of each live run, so the list polls while one runs.
  const liveRunSessions = Object.keys(runs).filter((id) => runs[id].some((r) => isLive(r.state))).join(" ");
  useEffect(() => {
    if (!liveRunSessions) return;
    const timer = setInterval(() => liveRunSessions.split(" ").forEach(reloadRuns), 2000);
    return () => clearInterval(timer);
  }, [liveRunSessions]);

  const joined: SessionCage[] = sessions.map((session) => sessionCage(session, cages));
  const panes: Pane[] = joined.flatMap(({ session }) =>
    session.terminals.map((t) => ({
      key: t.id,
      sessionId: session.id,
      kind: t.kind,
      processId: session.processes.find((p) => p.terminalId === t.id && p.exitCode === null)?.id,
    })),
  );
  const selected = joined.find((j) => j.session.id === selectedId) ?? joined[0];
  const session = selected?.session;
  const vmCage = selected?.vmCage;
  // With no effective cage there is nothing to restart into: the banner offers Stop.
  const cageChanged = vmCage?.status.state === "running" && !!selected.cage && selected.cage.id !== vmCage.id;
  const sessionPanes = panes.filter((p) => p.sessionId === session?.id);
  /** The focused pane of a session; a stale one gives way to the first pane. */
  const focusedOf = (target: SessionView) => {
    const shown = leaves(target.paneLayout);
    return shown.includes(focusedPanes[target.id]) ? focusedPanes[target.id] : shown[0];
  };
  const focused = session && focusedOf(session);
  /** The pane whose folder ⌘⇧E and the command menu open. */
  const here = view.kind === "terminal" && !showSettings ? focused : undefined;
  const events = (vmCage && net[vmCage.id]) ?? [];
  const defaultCage = cages.find((c) => c.id === workspaces.find((w) => w.sessions.includes(session?.id ?? ""))?.defaultCage);

  // A deleted task has no view.
  useEffect(() => {
    if (view.kind === "task" && session && !session.tasks.some((t) => t.id === view.taskId)) setView({ kind: "overview" });
  }, [view, session]);

  const blocked = Object.fromEntries(
    joined.map(({ session, vmCage }) => [
      session.id,
      vmCage ? (net[vmCage.id] ?? []).filter((e) => !e.allowed && e.seq > (lastSeen[vmCage.id] ?? 0)).length : 0,
    ]),
  );

  /** Saves at once on the screen, then in the backend. */
  function saveLayout(sessionId: string, layout: PaneNode | null) {
    setSessions((current) => current.map((s) => (s.id === sessionId ? { ...s, paneLayout: layout } : s)));
    setPaneLayout(sessionId, layout).then(refresh, (e) => {
      setError(errorMessage(e));
      return refresh();
    });
  }

  /** A focus event of the pane calls it too, so an unchanged focus keeps the state. */
  function focusPane(key: string | undefined, sessionId = session?.id) {
    if (sessionId && key)
      setFocusedPanes((current) => (current[sessionId] === key ? current : { ...current, [sessionId]: key }));
    setView((current) => (current.kind === "terminal" && current.key === key ? current : { kind: "terminal", key }));
  }

  /** Focuses the pane of the terminal. A terminal with no pane takes the place of the focused pane; the replaced terminal keeps running. */
  function showPane(key: string | undefined, target = session) {
    if (!target) return;
    if (key && !leaves(target.paneLayout).includes(key))
      saveLayout(target.id, replace(target.paneLayout, focusedOf(target), key));
    focusPane(key, target.id);
  }

  /** ⌘W or a pane's close button: hides the pane. The terminal and its process stay in the tree. */
  function hidePane(key = focused) {
    if (!session || !key) return;
    const layout = remove(session.paneLayout, key);
    saveLayout(session.id, layout);
    focusPane(key === focused ? leaves(layout)[0] : focused);
  }

  function moveFocus(direction: Direction) {
    const next = session?.paneLayout && focused && neighbor(session.paneLayout, focused, direction);
    if (next) focusPane(next);
  }

  /** Items belong to one session, so another session shows its Overview instead. */
  function select(id: string) {
    setRoundFolder(undefined);
    setSettingsTab(undefined);
    setShowCages(false);
    setSelectedId(id);
    if (id === session?.id || !["overview", "network", "settings"].includes(view.kind)) setView({ kind: "overview" });
  }

  function openView(sessionId: string, next: View) {
    setRoundFolder(undefined);
    setSettingsTab(undefined);
    setShowCages(false);
    setSelectedId(sessionId);
    const target = sessions.find((s) => s.id === sessionId);
    // A session click opens the terminal view with its focused pane.
    if (next.kind === "terminal") showPane(next.key ?? (target && focusedOf(target)), target);
    else setView(next);
  }

  async function addProject(folder: string, workspace?: string) {
    setSelectedId(await createSession(folder, workspace));
    setView({ kind: "overview" });
  }

  async function openRoundFolder() {
    const folder = await open({ directory: true, title: "Choose a duo round folder, such as docs/tasks/<slug>/<round>" });
    if (folder !== null) setRoundFolder(folder);
  }

  async function newProject() {
    const folder = await open({ directory: true, title: "Choose the project folder" });
    if (folder !== null) await addProject(folder);
  }

  /**
   * The warning before each open of an agent with no cage. A second ask cancels the first. True: the user continues.
   */
  function confirmNoCage(agent: AgentKind) {
    noCageAnswer.current?.(false);
    return new Promise<boolean>((answer) => {
      noCageAnswer.current = answer;
      setNoCage(agent);
    });
  }

  function answerNoCage(ok: boolean) {
    const answer = noCageAnswer.current;
    noCageAnswer.current = undefined;
    setNoCage(undefined);
    answer?.(ok);
  }

  /**
   * Opens a terminal, or the first agent: with no effective cage, the first agent on this Mac; else the first agent of
   * the cage, host agents first. It shows in the focused pane, or in a new pane right of the pane `at` (`row`) or below
   * it (`column`); no `at` splits the focused pane.
   */
  async function newSession(kind: TerminalKind | "agent", target = selected, direction?: "row" | "column", at?: string) {
    if (!target) return;
    const { session } = target;
    if (direction && tooDeep(session.paneLayout, at ?? focusedOf(session)))
      throw new Error("This pane cannot split again. Close a pane first.");
    let terminal: TerminalKind | undefined = macAgent(kind, !!session.effectiveCage, detection);
    if (terminal) {
      // The UI can show a stale cage: warn before each Mac agent. The backend checks the cage again.
      if (!(await confirmNoCage(terminal.agent))) return;
    } else {
      let cage = launchCage(target);
      // A stopped cage does not know its mounted agents. Read them after the start. A Mac shell needs no start.
      if (target.vmCage?.status.state !== "running" && (kind === "agent" || kind.type !== "host")) {
        await vm.start(session.id);
        const next = await listSessions();
        const fresh = next.sessions.find((s) => s.id === session.id);
        cage = (fresh && sessionCage(fresh, next.cages).vmCage) ?? cage;
      }
      const mounted = cage ? agentsOf(cage) : [];
      terminal = kind === "agent" ? { type: "agent", agent: mounted[0], resume: false } : kind;
      if (terminal.type === "agent" && !mounted.includes(terminal.agent))
        throw new Error(`${terminal.agent ? AGENT_NAMES[terminal.agent] : "No agent"} is not mounted in this cage.`);
    }
    const { rows, cols } = fittedSize();
    const processId = await openSession(session.id, terminal, rows, cols);
    const list = await refresh();
    const terminalId = list?.flatMap((s) => s.processes).find((p) => p.id === processId)?.terminalId;
    const fresh = list?.find((s) => s.id === session.id) ?? session;
    setSelectedId(session.id);
    if (!terminalId) return;
    if (direction) {
      saveLayout(session.id, split(fresh.paneLayout, at ?? focusedOf(fresh), terminalId, direction));
      focusPane(terminalId, session.id);
    } else showPane(terminalId, fresh);
  }

  /** Opens the session folder, or the terminal's current folder, and says where it fell back. No `editorId`: the default. */
  async function openEditor(target: SessionView, editorId?: string, terminalId?: string) {
    const askedId = editorId ?? editors?.default ?? null;
    const opened = await openInEditor(target.id, askedId, terminalId ?? null);
    const asked = editors?.editors.find((e) => e.id === askedId)?.name ?? "The editor";
    const notes = [
      opened.editorFellBack && asked !== opened.editor && `${asked} is not available, so ${opened.editor} opened.`,
      opened.folderFellBack && `The folder of the terminal is unknown, so ${opened.editor} opened the project folder ${tildePath(opened.folder, home)}.`,
    ].filter((note): note is string => !!note);
    if (notes.length > 0) setAppNotices((all) => [...all, notes.join(" ")]);
  }

  /** A choice of an editor makes it the default, so the launchers switch to it. */
  function launch(target: SessionCage, action: LaunchAction, terminalId?: string) {
    switch (action.type) {
      case "session":
        return run(() => newSession(action.kind, target));
      case "editor":
        return run(async () => {
          if (action.editor.id !== editors?.default)
            await savePrefs({ ...prefs, editors: { ...prefs.editors, default: action.editor.id } });
          await openEditor(target.session, action.editor.id, terminalId);
        });
      case "copyPath":
        return run(() => navigator.clipboard.writeText(target.session.folder));
      case "reveal":
        return run(() => revealSession(target.session.id));
    }
  }

  /** Starts a new process for a dormant or exited terminal, in its place. */
  async function reopen(pane: Pane) {
    const target = joined.find((j) => j.session.id === pane.sessionId);
    if (pane.kind.type === "hostAgent") {
      // The UI can show a stale cage: warn before each Mac agent. The backend checks the cage again.
      if (!(await confirmNoCage(pane.kind.agent))) return;
    } else if (pane.kind.type !== "host" && target && target.vmCage?.status.state !== "running") await vm.start(pane.sessionId);
    const { rows, cols } = fittedSize();
    await reopenTerminal(pane.sessionId, pane.key, rows, cols);
    showPane(pane.key, (await refresh())?.find((s) => s.id === pane.sessionId));
  }

  // A duo round folder dropped anywhere opens its room. Another folder dropped on
  // a workspace or session row, or on empty rail space, creates a project. The map has its own listener.
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
      if (!element?.closest("[data-drop=rail]")) return;
      // Only the nearest row counts: a terminal, task or group row inside a session refuses the drop.
      const row = element.closest<HTMLElement>("[role=treeitem]");
      if (row && !row.dataset.workspaceDrop) return;
      const workspace = row?.dataset.workspaceDrop;
      setError(undefined);
      resolveFolder(dropped).then((folder) => setDropFolder({ dropped, folder, workspace }), (e) => setError(errorMessage(e)));
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
    if (sessionPanes.length === 0) return;
    const index = sessionPanes.findIndex((p) => p.key === focused);
    showPane(sessionPanes[(index + step + sessionPanes.length) % sessionPanes.length].key);
  }

  /** Removes the terminal and ends its process. The backend also removes its pane. */
  function closePane(key: string) {
    const pane = panes.find((p) => p.key === key);
    if (!pane) return;
    run(() => closeTerminal(pane.sessionId, key));
    if (view.kind === "terminal" && view.key === key) setView({ kind: "terminal" });
  }

  /** Saves the order of the session's terminals. A terminal that opened or closed during the drag fails the order: the tree then shows the saved order. */
  const moveTerminals = (sessionId: string, ids: string[]) =>
    reorderTerminals(sessionId, ids).then(refresh, (e) => {
      if (errorCode(e) !== INVALID_ORDER) setError(errorMessage(e));
      return refresh();
    });

  /** Resolves when the user confirms and `go` is done. A cancel rejects with `Cancelled`. */
  const confirmFirst = (text: string, confirm: string, go: () => Promise<void>) =>
    new Promise<void>((resolve, reject) =>
      setAsk({
        text,
        confirm,
        answer: (ok) => {
          setAsk(undefined);
          if (ok) go().then(resolve, reject);
          else reject(new Cancelled());
        },
      }),
    );

  /** A restart adds a notice for each terminal that does not reopen. */
  const pullNotices = () =>
    takeAppNotices().then(
      (notes) => notes.length > 0 && setAppNotices((all) => [...all, ...notes]),
      (e) => setError(errorMessage(e)),
    );

  const vm: VmControl = {
    async start(sessionId) {
      try {
        await startCage(sessionId);
      } catch (e) {
        const add = errorDetail<RestartToAdd>(e, "RestartToAdd");
        if (!add) throw e;
        const name = joined.find((j) => j.session.id === sessionId)?.session.name ?? "this project";
        const others = add.members.map((m) => m.name).join(", ");
        const first = add.members.length === 1 ? joined.find((j) => j.session.id === add.members[0].id)?.session : undefined;
        const moves = first
          ? ` ${first.name} moves from /workspace to /workspace/${first.slug}, so its agents do not find their history of the old path.`
          : "";
        await confirmFirst(
          `Restart cage ${add.cage} to add ${name}? The terminals of ${others} reopen; their running tasks are cancelled.${moves}`,
          "Restart",
          () => restartCageToAdd(sessionId, add).finally(pullNotices),
        );
      }
    },
    stop: (sessionId) =>
      confirmShared(
        (others) => `Its VM also runs ${others}: their VM terminals and running tasks end.`,
        "Stop",
        (confirmed) => stopCage(sessionId, confirmed),
      ),
  };

  /**
   * Runs `act` on a session's VM. A VM with other members refuses with `StopShared`: the user confirms `say` of them,
   * then `act` runs again with what the user saw.
   */
  async function confirmShared(
    say: (others: string) => string,
    verb: "Stop" | "Restart",
    act: (confirmed: StopShared | null) => Promise<void>,
  ) {
    try {
      await act(null);
    } catch (e) {
      const shared = errorDetail<StopShared>(e, "StopShared");
      if (!shared) throw e;
      const others = shared.members.map((m) => m.name).join(", ");
      await confirmFirst(`${verb} cage ${shared.cage}? ${say(others)}`, verb, () => act(shared));
    }
  }

  const toggle = ({ session, vmCage }: SessionCage) =>
    run(() => (vmCage?.status.state === "running" ? vm.stop(session.id) : vm.start(session.id)));

  function startIntent(kind: Intent["kind"]) {
    setView({ kind: kind === "addRule" ? "network" : "settings" });
    setIntent({ kind, at: Date.now() });
  }

  /**
   * Restarts the VM of the selected session with all its members. After a cage change, it stops the old cage's VM and
   * starts the new cage.
   */
  const restart = (sessionId: string) =>
    run(() =>
      cageChanged
        ? vm.stop(sessionId).then(() => vm.start(sessionId))
        : confirmShared(
            (others) => `Its VM also runs ${others}: their terminals reopen; their running tasks are cancelled.`,
            "Restart",
            (confirmed) => restartCage(sessionId, confirmed).finally(pullNotices),
          ),
    );

  const actions: Record<ShortcutAction, () => void> = {
    // The command menu has no open prop. A click with no pointer opens it and focuses the first item.
    commands: () => commandBar.current?.querySelector("button")?.click(),
    newProject: () => run(newProject),
    newShell: () => run(() => newSession({ type: "host" })),
    newVmShell: () => run(() => newSession({ type: "shell" })),
    openEditor: () => session && run(() => openEditor(session, undefined, here)),
    newAgent: () => run(() => newSession("agent")),
    close: () => view.kind === "terminal" && hidePane(),
    next: () => cycle(1),
    previous: () => cycle(-1),
    overview: () => setView({ kind: "overview" }),
    agents: () => {
      const agents = sessionPanes.filter((p) => p.kind.type === "agent" || p.kind.type === "hostAgent");
      if (agents.length > 0) showPane(agents[agents.length - 1].key);
      else run(() => newSession("agent"));
    },
    terminal: () => showPane(focused ?? sessionPanes[sessionPanes.length - 1]?.key),
    network: () => setView({ kind: "network" }),
    settings: () => setView({ kind: "settings" }),
    appSettings: () => setSettingsTab("agents"),
    splitRight: () => run(() => newSession({ type: "host" }, selected, "row")),
    splitDown: () => run(() => newSession({ type: "host" }, selected, "column")),
    focusLeft: () => moveFocus("left"),
    focusRight: () => moveFocus("right"),
    focusUp: () => moveFocus("up"),
    focusDown: () => moveFocus("down"),
  };
  useShortcuts(actions);

  return (
    <div className="app" style={{ "--zw-rail-width": `${railWidth}px` } as CSSProperties}>
      <ProjectRail
        sessions={joined}
        cages={cages}
        workspaces={workspaces}
        selectedId={session?.id}
        view={view}
        blocked={blocked}
        runs={runs}
        panes={panes}
        run={run}
        vm={vm}
        onOpen={openView}
        onRunsChange={reloadRuns}
        onToggle={toggle}
        onNewSession={(id, kind) => run(() => newSession(kind, joined.find((j) => j.session.id === id)))}
        onClosePane={closePane}
        onReopen={(pane) => run(() => reopen(pane))}
        onMoveTerminals={moveTerminals}
        onNew={actions.newProject}
        onSettings={actions.appSettings}
        onCages={() => {
          setRoundFolder(undefined);
          setSettingsTab(undefined);
          setShowCages(true);
        }}
        dropFolder={dropFolder}
        editors={editors}
        detection={detection}
        home={home}
        onLaunch={(id, action) => {
          const target = joined.find((j) => j.session.id === id);
          if (target) launch(target, action);
        }}
        onDropDone={(create) => {
          setDropFolder(undefined);
          if (create && dropFolder) run(() => addProject(dropFolder.folder, dropFolder.workspace));
        }}
      >
        <CommandBar
          ref={commandBar}
          sessions={joined.map((j) => j.session)}
          selected={selected}
          panes={sessionPanes}
          focused={focused}
          here={here}
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
          editors={editors}
          detection={detection}
          onLaunch={(action) => selected && launch(selected, action, here)}
          run={run}
        />
      </ProjectRail>
      <RailHandle
        width={railWidth}
        onResize={setRailWidth}
        onDone={(width) => onPrefs({ ...prefs, railWidth: width }).catch((e) => setError(errorMessage(e)))}
      />
      <main className="main">
        <div className="notices">
          {ask && (
            <Card variant="outlined" padding="sm" className="stack" role="alertdialog" aria-label={ask.confirm}>
              <p>{ask.text}</p>
              <div className="row end">
                <Button size="sm" variant="ghost" onClick={() => ask.answer(false)}>
                  Cancel
                </Button>
                <Button size="sm" variant="danger" onClick={() => ask.answer(true)}>
                  {ask.confirm}
                </Button>
              </div>
            </Card>
          )}
          {noCage && (
            <Card variant="outlined" padding="sm" className="stack" role="alertdialog" aria-label={`Open ${AGENT_NAMES[noCage]} with no cage`}>
              <p className="row">
                <ShieldOff size={14} className="warn" aria-hidden="true" />
                {AGENT_NAMES[noCage]} runs on your Mac with your files and your logins. Nothing isolates it.
              </p>
              <div className="row end">
                <Button size="sm" variant="ghost" autoFocus onClick={() => answerNoCage(false)}>
                  Cancel
                </Button>
                <Button size="sm" onClick={() => answerNoCage(true)}>
                  Continue
                </Button>
              </div>
            </Card>
          )}
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
          {!showSettings && !showCages && !roundFolder && session && (vmCage?.pendingRestart || cageChanged) && (
            <Notice
              icon={<RotateCw size={14} className="warn" />}
              actions={
                selected.cage ? (
                  <Button size="sm" variant="outline" onClick={() => restart(session.id)}>
                    Restart
                  </Button>
                ) : (
                  <Button size="sm" variant="outline" onClick={() => run(() => vm.stop(session.id))}>
                    Stop
                  </Button>
                )
              }
            >
              {!selected.cage
                ? `The VM runs cage ${vmCage?.name}, but the project has no cage now. Stop the VM.`
                : cageChanged
                  ? `The VM runs cage ${vmCage.name}; restart to use ${selected.cage.name}.`
                  : "The running cage uses an older config. Restart it to apply the changes."}
            </Notice>
          )}
        </div>
        {settingsTab !== undefined ? (
          <AppSettings
            tab={settingsTab}
            onTab={setSettingsTab}
            editors={editors}
            onDetectEditors={loadEditors}
            detection={detection}
            sessions={joined}
            runtimes={runtimes}
            run={run}
            onRefresh={() => detect(true)}
            onUse={(sessionId, agent) => {
              select(sessionId);
              setAdding((current) => ({ ...current, [sessionId]: agent.kind }));
            }}
            prefs={prefs}
            onPrefs={savePrefs}
            update={update}
            onUpdate={onUpdate}
            appearance={appearance}
          />
        ) : showCages ? (
          <CagesPage cages={cages} sessions={sessions} workspaces={workspaces} runtimes={runtimes} run={run} />
        ) : roundFolder ? (
          <section className="page">
            <div className="view task-view">
              <Suspense>
                <TaskRoom key={roundFolder} kind="folder" path={roundFolder} />
              </Suspense>
            </div>
          </section>
        ) : selected ? (
          <ProjectPage
            session={selected.session}
            cage={selected.cage}
            vmCage={selected.vmCage}
            cages={cages}
            defaultCage={defaultCage}
            events={events}
            view={view}
            onView={setView}
            intent={intent}
            onIntentDone={() => setIntent(undefined)}
            onNetSeen={(seq) => vmCage && setLastSeen((current) => ({ ...current, [vmCage.id]: seq }))}
            onDropFolder={dropMount}
            runtimes={runtimes}
            detection={detection}
            adding={adding[session.id]}
            onAdding={(kind) => setAdding((current) => ({ ...current, [session.id]: kind }))}
            onHostAgents={() => setSettingsTab("agents")}
            editors={editors}
            home={home}
            onLaunch={(action) => launch(selected, action)}
            onEditorSettings={() => setSettingsTab("editors")}
            onSplit={(key, direction) => run(() => newSession({ type: "host" }, selected, direction, key))}
            onOpenEditor={(key) => run(() => openEditor(session, undefined, key))}
            onHide={hidePane}
            runs={runs[session.id] ?? []}
            onRunsChange={() => reloadRuns(session.id)}
            onRefresh={refresh}
            panes={panes}
            focused={focused}
            run={run}
            onToggle={() => toggle(selected)}
            onNewSession={(kind) => run(() => newSession(kind))}
            onReopen={(pane) => run(() => reopen(pane))}
            onFocus={(key) => focusPane(key)}
            onLayout={(layout) => saveLayout(session.id, layout)}
            onPaneExit={refresh}
            onPaneError={(e) => setError(errorMessage(e))}
          />
        ) : (
          <div className="empty">
            <CageMark size={40} />
            <h1>No projects yet</h1>
            <p className="muted">
              A project runs an AI agent in its cage, a microVM. The agent sees only the folders that you mount and reaches only
              the hosts that you allow.
            </p>
            <Button iconLeft={<Plus size={16} />} onClick={actions.newProject}>
              Create project
            </Button>
          </div>
        )}
        {error && <p className="status danger mono">{error}</p>}
      </main>
    </div>
  );
}
