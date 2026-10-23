import { CSSProperties, lazy, ReactNode, Suspense, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { homeDir } from "@tauri-apps/api/path";
import { Button } from "@driif/motiv";
import { Plus, RotateCw, TriangleAlert } from "lucide-react";
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
  createProject,
  listProjects,
  onGuestLogin,
  onStartProgress,
  resolveFolder,
  startCage,
  stopCage,
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
  INVALID_ORDER,
  PaneNode,
  SessionKind,
  ProjectView,
} from "./shared/api";
import { AppSettings } from "./features/settings/AppSettings";
import { CommandBar } from "./shared/CommandBar";
import { Onboarding, StartupScreen } from "./features/onboarding/Onboarding";
import { ProjectPage } from "./features/projects/ProjectPage";
import { ProjectRail } from "./features/projects/ProjectRail";
import { agentsOf, LaunchAction } from "./features/launcher/actions";
import { detectEditors, Editors, openInEditor, revealProject } from "./features/launcher/api";
import { tildePath } from "./shared/paths";
import { UpdateBanner } from "./features/updates/UpdateBanner";
import { Appearance, useAppearance } from "./features/appearance/themes";
import { clampRail, RailHandle } from "./features/projects/RailHandle";
import { Notice } from "./shared/Notice";
import { ShortcutAction, useShortcuts } from "./shared/useShortcuts";
import { errorCode, errorMessage, isBackendError } from "./shared/errorMessage";
import { LOST_TITLE, ReloadScreen, useBackendLost } from "./shared/Recover";
import { CageMark } from "./shared/CageMark";

const TaskRoom = lazy(() => import("./features/tasks/TaskRoom").then((m) => ({ default: m.TaskRoom })));

/** The same size as the ring in the backend. */
const NET_RING = 2000;

export type TabView = "overview" | "network" | "settings";

/** What the main area shows for the selected project. */
export type View =
  | { kind: TabView }
  /** `key` is the focused pane. No `key`: no pane is focused. */
  | { kind: "terminal"; key?: string }
  | { kind: "app"; appId: string }
  /** No `run` shows the latest run. */
  | { kind: "task"; taskId: string; run?: string }
  /** No `taskId` adds a task. */
  | { kind: "taskEdit"; taskId?: string };

/** A terminal of a project. `key` is the terminal id. No `sessionId`: the terminal is dormant or has exited. */
export interface Pane {
  key: string;
  projectId: string;
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

/** Shows the first-run steps once, then the projects when the runtime is ready. */
export default function App() {
  const [startup, setStartup] = useState<Startup>();
  const [prefs, setPrefs] = useState<Prefs>();
  const [firstProject, setFirstProject] = useState<string>();
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
        onDone={async (projectId) => {
          // Set first: the workspace reads it once, when it mounts.
          setFirstProject(projectId);
          await savePrefs({ ...prefs, onboarded: true });
        }}
      />
    );
  if (startup.phase !== "ready") return <StartupScreen startup={startup} banner={banner} />;
  return (
    <Workspace
      firstProject={firstProject}
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
  /** The project that the first-run steps made. */
  firstProject?: string;
  prefs: Prefs;
  onPrefs: (prefs: Prefs) => Promise<void>;
  /** The update banner of the app. */
  banner: ReactNode;
  update?: UpdateInfo;
  onUpdate: (update?: UpdateInfo) => void;
  appearance: Appearance;
}

function Workspace({ firstProject, prefs, onPrefs, banner, update, onUpdate, appearance }: WorkspaceProps) {
  const [projects, setProjects] = useState<ProjectView[]>([]);
  const [selectedId, setSelectedId] = useState(firstProject);
  const [view, setView] = useState<View>({ kind: "overview" });
  /** Per project, the terminal of the pane that has the focus. */
  const [focusedPanes, setFocusedPanes] = useState<Record<string, string>>({});
  const [error, setError] = useState<string>();
  const [appNotices, setAppNotices] = useState<string[]>([]);
  const [intent, setIntent] = useState<Intent>();
  const commandBar = useRef<HTMLDivElement>(null);
  const refreshes = useRef(0);
  const [net, setNet] = useState<Record<string, NetEvent[]>>({});
  /** Per project, the last `seq` that the Network tab showed. */
  const [lastSeen, setLastSeen] = useState<Record<string, number>>({});
  const [listening, setListening] = useState(false);
  const backfilled = useRef(new Set<string>());
  const [runs, setRuns] = useState<Record<string, RunSummary[]>>({});
  /** A duo round folder on the host that the main area shows instead of a project. */
  const [roundFolder, setRoundFolder] = useState<string>();
  const [dropFolder, setDropFolder] = useState<DroppedFolder>();
  const [runtimeList, setRuntimeList] = useState<RuntimeView[]>([]);
  const [installs, setInstalls] = useState<Runtimes["installs"]>({});
  /** The agents on this Mac. `undefined` while a detection runs. */
  const [detection, setDetection] = useState<Detection>();
  /** The tab of the app settings, which the main area shows instead of a project. */
  const [settingsTab, setSettingsTab] = useState<string>();
  const showSettings = settingsTab !== undefined;
  /** The editors on this Mac. `undefined` until the first detection. */
  const [editors, setEditors] = useState<Editors>();
  const [home, setHome] = useState<string>();
  /** Per project, the host agent that its Add agent step shows. */
  const [adding, setAdding] = useState<Record<string, AgentKind | undefined>>({});
  const [railWidth, setRailWidth] = useState(() => clampRail(prefs.railWidth));

  // As the workspace renders, before its terminals mount: they take the look when they open.
  const terminalTheme = appearance.themes.find((t) => t.id === prefs.terminal.themeId) ?? appearance.theme;
  setLook(prefs.terminal, terminalTheme.terminal);

  // Replies can arrive out of order. Only the reply to the latest request is used. Resolves to the list it read.
  const refresh = () => {
    const request = ++refreshes.current;
    return listProjects().then(
      (next) => {
        if (request === refreshes.current) setProjects(next);
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

  const loadEditors = () => detectEditors().then(setEditors, (e) => setError(errorMessage(e)));

  useEffect(() => {
    refresh();
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

  // The phase of a start that runs. A late event cannot undo a later state, such as running.
  useEffect(() => {
    const stop = onStartProgress((projectId, phase) =>
      setProjects((current) =>
        current.map((s) =>
          s.id === projectId && ["stopped", "starting", "error"].includes(s.status.state)
            ? { ...s, status: { state: "starting", phase } }
            : s,
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
  const addNet = (projectId: string, events: NetEvent[]) =>
    setNet((current) => {
      const bySeq = new Map((current[projectId] ?? []).map((e) => [e.seq, e]));
      for (const event of events) bySeq.set(event.seq, event);
      return { ...current, [projectId]: [...bySeq.values()].sort((a, b) => a.seq - b.seq).slice(-NET_RING) };
    });

  useEffect(() => {
    const stop = onNetEvent((projectId, event) => addNet(projectId, [event]));
    stop.then(() => setListening(true));
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  // The backfill starts after the listener, so no event falls between the two.
  useEffect(() => {
    if (!listening) return;
    for (const { id } of projects) {
      if (backfilled.current.has(id)) continue;
      backfilled.current.add(id);
      listNetEvents(id).then((events) => addNet(id, events), () => {});
    }
  }, [projects, listening]);

  const reloadRuns = (id: string) =>
    listTaskRuns(id).then((list) => setRuns((current) => ({ ...current, [id]: list })), () => {});

  const projectIds = projects.map((s) => s.id).join(" ");
  useEffect(() => {
    for (const { id } of projects) if (!(id in runs)) reloadRuns(id);
  }, [projectIds]);

  // The tree shows the stage of each live run, so the list polls while one runs.
  const liveRunProjects = Object.keys(runs).filter((id) => runs[id].some((r) => isLive(r.state))).join(" ");
  useEffect(() => {
    if (!liveRunProjects) return;
    const timer = setInterval(() => liveRunProjects.split(" ").forEach(reloadRuns), 2000);
    return () => clearInterval(timer);
  }, [liveRunProjects]);

  // An app can exit while no view is attached, so the list polls while one runs.
  const appsRunning = projects.some((s) => s.appRuns.some((r) => r.exitCode === null));
  useEffect(() => {
    if (!appsRunning) return;
    const timer = setInterval(refresh, 2000);
    return () => clearInterval(timer);
  }, [appsRunning]);

  const panes: Pane[] = projects.flatMap((s) =>
    s.terminals.map((t) => ({
      key: t.id,
      projectId: s.id,
      kind: t.kind,
      sessionId: s.sessions.find((session) => session.terminalId === t.id && session.exitCode === null)?.id,
    })),
  );
  const project = projects.find((s) => s.id === selectedId) ?? projects[0];
  const projectPanes = panes.filter((p) => p.projectId === project?.id);
  /** The focused pane of a project; a stale one gives way to the first pane. */
  const focusedOf = (target: ProjectView) => {
    const shown = leaves(target.paneLayout);
    return shown.includes(focusedPanes[target.id]) ? focusedPanes[target.id] : shown[0];
  };
  const focused = project && focusedOf(project);
  /** The pane whose folder ⌘⇧E and the command menu open. */
  const here = view.kind === "terminal" && !showSettings ? focused : undefined;
  const events = (project && net[project.id]) ?? [];

  // A deleted app or task has no view.
  useEffect(() => {
    if (view.kind === "app" && project && !project.apps.some((a) => a.id === view.appId)) setView({ kind: "overview" });
    if (view.kind === "task" && project && !project.tasks.some((t) => t.id === view.taskId)) setView({ kind: "overview" });
  }, [view, project]);

  const blocked = Object.fromEntries(
    projects.map(({ id }) => [id, (net[id] ?? []).filter((e) => !e.allowed && e.seq > (lastSeen[id] ?? 0)).length]),
  );

  /** Saves at once on the screen, then in the backend. */
  function saveLayout(projectId: string, layout: PaneNode | null) {
    setProjects((current) => current.map((s) => (s.id === projectId ? { ...s, paneLayout: layout } : s)));
    setPaneLayout(projectId, layout).then(refresh, (e) => {
      setError(errorMessage(e));
      return refresh();
    });
  }

  /** A focus event of the pane calls it too, so an unchanged focus keeps the state. */
  function focusPane(key: string | undefined, projectId = project?.id) {
    if (projectId && key)
      setFocusedPanes((current) => (current[projectId] === key ? current : { ...current, [projectId]: key }));
    setView((current) => (current.kind === "terminal" && current.key === key ? current : { kind: "terminal", key }));
  }

  /** Focuses the pane of the terminal. A terminal with no pane takes the place of the focused pane; the replaced terminal keeps running. */
  function showPane(key: string | undefined, target = project) {
    if (!target) return;
    if (key && !leaves(target.paneLayout).includes(key))
      saveLayout(target.id, replace(target.paneLayout, focusedOf(target), key));
    focusPane(key, target.id);
  }

  /** ⌘W or a pane's close button: hides the pane. The terminal and its process stay in the tree. */
  function hidePane(key = focused) {
    if (!project || !key) return;
    const layout = remove(project.paneLayout, key);
    saveLayout(project.id, layout);
    focusPane(key === focused ? leaves(layout)[0] : focused);
  }

  function moveFocus(direction: Direction) {
    const next = project?.paneLayout && focused && neighbor(project.paneLayout, focused, direction);
    if (next) focusPane(next);
  }

  /** Items belong to one project, so another project shows its Overview instead. */
  function select(id: string) {
    setRoundFolder(undefined);
    setSettingsTab(undefined);
    setSelectedId(id);
    if (id === project?.id || !["overview", "network", "settings"].includes(view.kind)) setView({ kind: "overview" });
  }

  function openView(projectId: string, next: View) {
    setRoundFolder(undefined);
    setSettingsTab(undefined);
    setSelectedId(projectId);
    if (next.kind === "terminal") showPane(next.key, projects.find((s) => s.id === projectId));
    else setView(next);
  }

  async function addProject(folder: string) {
    setSelectedId(await createProject(folder));
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
   * Opens a terminal, or the first agent of the project: host agents come first. It shows in the focused pane, or in a new
   * pane right of the pane `at` (`row`) or below it (`column`); no `at` splits the focused pane.
   */
  async function newSession(kind: SessionKind | "agent", target = project, direction?: "row" | "column", at?: string) {
    if (!target) return;
    if (direction && tooDeep(target.paneLayout, at ?? focusedOf(target)))
      throw new Error("This pane cannot split again. Close a pane first.");
    // A stopped cage does not know its mounted agents. Read them after the start. A Mac shell needs no start.
    if (target.status.state !== "running" && (kind === "agent" || kind.type !== "host")) {
      await startCage(target.id);
      target = (await listProjects()).find((s) => s.id === target!.id) ?? target;
    }
    const mounted = agentsOf(target);
    const session: SessionKind = kind === "agent" ? { type: "agent", agent: mounted[0], resume: false } : kind;
    if (session.type === "agent" && !mounted.includes(session.agent))
      throw new Error(`${session.agent ? AGENT_NAMES[session.agent] : "No agent"} is not mounted in this cage.`);
    const { rows, cols } = fittedSize();
    const sessionId = await openSession(target.id, session, rows, cols);
    const list = await refresh();
    const terminalId = list?.flatMap((s) => s.sessions).find((s) => s.id === sessionId)?.terminalId;
    const fresh = list?.find((s) => s.id === target!.id) ?? target;
    setSelectedId(target.id);
    if (!terminalId) return;
    if (direction) {
      saveLayout(target.id, split(fresh.paneLayout, at ?? focusedOf(fresh), terminalId, direction));
      focusPane(terminalId, target.id);
    } else showPane(terminalId, fresh);
  }

  /** Opens the project folder, or the terminal's current folder, and says where it fell back. No `editorId`: the default. */
  async function openEditor(target: ProjectView, editorId?: string, terminalId?: string) {
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
  function launch(target: ProjectView, action: LaunchAction, terminalId?: string) {
    switch (action.type) {
      case "session":
        return run(() => newSession(action.kind, target));
      case "editor":
        return run(async () => {
          if (action.editor.id !== editors?.default)
            await savePrefs({ ...prefs, editors: { ...prefs.editors, default: action.editor.id } });
          await openEditor(target, action.editor.id, terminalId);
        });
      case "copyPath":
        return run(() => navigator.clipboard.writeText(target.folder));
      case "reveal":
        return run(() => revealProject(target.id));
    }
  }

  /** Starts a new process for a dormant or exited terminal, in its place. */
  async function reopen(pane: Pane) {
    if (pane.kind.type !== "host" && projects.find((s) => s.id === pane.projectId)?.status.state !== "running")
      await startCage(pane.projectId);
    const { rows, cols } = fittedSize();
    await reopenTerminal(pane.projectId, pane.key, rows, cols);
    showPane(pane.key, (await refresh())?.find((s) => s.id === pane.projectId));
  }

  // A duo round folder dropped anywhere opens its room. Another folder dropped on
  // empty rail space creates a project. The map has its own listener.
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
    if (projectPanes.length === 0) return;
    const index = projectPanes.findIndex((p) => p.key === focused);
    showPane(projectPanes[(index + step + projectPanes.length) % projectPanes.length].key);
  }

  /** Removes the terminal and ends its process. The backend also removes its pane. */
  function closePane(key: string) {
    const pane = panes.find((p) => p.key === key);
    if (!pane) return;
    run(() => closeTerminal(pane.projectId, key));
    if (view.kind === "terminal" && view.key === key) setView({ kind: "terminal" });
  }

  /** Saves the order of the project's terminals. A terminal that opened or closed during the drag fails the order: the tree then shows the saved order. */
  const moveTerminals = (projectId: string, ids: string[]) =>
    reorderTerminals(projectId, ids).then(refresh, (e) => {
      if (errorCode(e) !== INVALID_ORDER) setError(errorMessage(e));
      return refresh();
    });

  const toggle = (project: ProjectView) =>
    run(() => (project.status.state === "running" ? stopCage(project.id) : startCage(project.id)));

  function startIntent(kind: Intent["kind"], appId?: string) {
    setView({ kind: kind === "addRule" ? "network" : "settings" });
    setIntent({ kind, appId, at: Date.now() });
  }

  const restart = (id: string) => run(() => stopCage(id).then(() => startCage(id)));

  const actions: Record<ShortcutAction, () => void> = {
    // The command menu has no open prop. A click with no pointer opens it and focuses the first item.
    commands: () => commandBar.current?.querySelector("button")?.click(),
    newProject: () => run(newProject),
    newShell: () => run(() => newSession({ type: "host" })),
    newVmShell: () => run(() => newSession({ type: "shell" })),
    openEditor: () => project && run(() => openEditor(project, undefined, here)),
    newAgent: () => run(() => newSession("agent")),
    close: () => view.kind === "terminal" && hidePane(),
    next: () => cycle(1),
    previous: () => cycle(-1),
    overview: () => setView({ kind: "overview" }),
    agents: () => {
      const agents = projectPanes.filter((p) => p.kind.type === "agent");
      if (agents.length > 0) showPane(agents[agents.length - 1].key);
      else run(() => newSession("agent"));
    },
    terminal: () => showPane(focused ?? projectPanes[projectPanes.length - 1]?.key),
    network: () => setView({ kind: "network" }),
    settings: () => setView({ kind: "settings" }),
    appSettings: () => setSettingsTab("agents"),
    splitRight: () => run(() => newSession({ type: "host" }, project, "row")),
    splitDown: () => run(() => newSession({ type: "host" }, project, "column")),
    focusLeft: () => moveFocus("left"),
    focusRight: () => moveFocus("right"),
    focusUp: () => moveFocus("up"),
    focusDown: () => moveFocus("down"),
  };
  useShortcuts(actions);

  return (
    <div className="app" style={{ "--zw-rail-width": `${railWidth}px` } as CSSProperties}>
      <ProjectRail
        projects={projects}
        selectedId={project?.id}
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
        onNewSession={(id, kind) => run(() => newSession(kind, projects.find((s) => s.id === id)))}
        onClosePane={closePane}
        onReopen={(pane) => run(() => reopen(pane))}
        onMoveTerminals={moveTerminals}
        onNew={actions.newProject}
        onSettings={actions.appSettings}
        dropFolder={dropFolder}
        editors={editors}
        home={home}
        onLaunch={(id, action) => {
          const target = projects.find((s) => s.id === id);
          if (target) launch(target, action);
        }}
        onDropDone={(create) => {
          setDropFolder(undefined);
          if (create && dropFolder) run(() => addProject(dropFolder.folder));
        }}
      >
        <CommandBar
          ref={commandBar}
          projects={projects}
          project={project}
          panes={projectPanes}
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
          onLaunch={(action) => project && launch(project, action, here)}
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
          {!showSettings && !roundFolder && project?.pendingRestart && (
            <Notice
              icon={<RotateCw size={14} className="warn" />}
              actions={
                <Button size="sm" variant="outline" onClick={() => restart(project.id)}>
                  Restart
                </Button>
              }
            >
              The running cage uses an older config. Restart it to apply the changes.
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
            projects={projects}
            runtimes={runtimes}
            run={run}
            onRefresh={() => detect(true)}
            onUse={(projectId, agent) => {
              select(projectId);
              setAdding((current) => ({ ...current, [projectId]: agent.kind }));
            }}
            prefs={prefs}
            onPrefs={savePrefs}
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
        ) : project ? (
          <ProjectPage
            project={project}
            events={events}
            view={view}
            onView={setView}
            intent={intent}
            onIntentDone={() => setIntent(undefined)}
            onNetSeen={(seq) => setLastSeen((current) => ({ ...current, [project.id]: seq }))}
            onDropFolder={dropMount}
            runtimes={runtimes}
            detection={detection}
            adding={adding[project.id]}
            onAdding={(kind) => setAdding((current) => ({ ...current, [project.id]: kind }))}
            onHostAgents={() => setSettingsTab("agents")}
            editors={editors}
            home={home}
            onLaunch={(action) => launch(project, action)}
            onEditorSettings={() => setSettingsTab("editors")}
            onSplit={(key, direction) => run(() => newSession({ type: "host" }, project, direction, key))}
            onOpenEditor={(key) => run(() => openEditor(project, undefined, key))}
            onHide={hidePane}
            runs={runs[project.id] ?? []}
            onRunsChange={() => reloadRuns(project.id)}
            onRefresh={refresh}
            panes={panes}
            focused={focused}
            run={run}
            onToggle={() => toggle(project)}
            onNewSession={(kind) => run(() => newSession(kind))}
            onReopen={(pane) => run(() => reopen(pane))}
            onFocus={(key) => focusPane(key)}
            onLayout={(layout) => saveLayout(project.id, layout)}
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
