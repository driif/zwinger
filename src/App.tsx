import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { Button, Card } from "@driif/motiv";
import { Plus, RotateCw, Shield } from "lucide-react";
import {
  Chat,
  closeSession,
  createShield,
  deleteChat,
  isBlocked,
  listChats,
  listNetEvents,
  listShields,
  NetEvent,
  onNetEvent,
  resolveFolder,
  SessionKind,
  ShieldView,
  startShield,
  stopShield,
} from "./api";
import { CommandBar } from "./CommandBar";
import { ShieldPage } from "./ShieldPage";
import { ShieldRail } from "./ShieldRail";
import { ShortcutAction, useShortcuts } from "./useShortcuts";
import "./App.css";

/** The same size as the ring in the backend. */
const NET_RING = 500;

export type TabView = "overview" | "network" | "settings";

/** What the main area shows for the selected shield. */
export type View =
  | { kind: TabView }
  /** No `chat` keeps pi's current chat. */
  | { kind: "chat"; chat?: string }
  /** No `key` shows the empty terminal view. */
  | { kind: "terminal"; key?: string }
  | { kind: "app"; appId: string };

export interface Pane {
  key: string;
  shieldId: string;
  kind: SessionKind;
  sessionId?: string;
}

export type Run = (action: () => Promise<unknown>) => Promise<unknown>;

/** A ⌘K action that a tab completes, such as focusing a field. `at` makes a repeated action new. */
export interface Intent {
  kind: "rename" | "mount" | "harness" | "delete" | "allowList" | "app";
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

/** A rail action on a chat that pi does in the shield's Agents view. */
export type ChatAction = { chat: string; at: number } & ({ kind: "rename"; name: string } | { kind: "delete" });

export default function App() {
  const [shields, setShields] = useState<ShieldView[]>([]);
  const [selectedId, setSelectedId] = useState<string>();
  const [view, setView] = useState<View>({ kind: "overview" });
  const [panes, setPanes] = useState<Pane[]>([]);
  const [activePanes, setActivePanes] = useState<Record<string, string>>({});
  const [error, setError] = useState<string>();
  const [intent, setIntent] = useState<Intent>();
  const commandBar = useRef<HTMLDivElement>(null);
  const refreshes = useRef(0);
  const [net, setNet] = useState<Record<string, NetEvent[]>>({});
  /** Per shield, the last `seq` that the Network tab showed. */
  const [lastSeen, setLastSeen] = useState<Record<string, number>>({});
  const [listening, setListening] = useState(false);
  const backfilled = useRef(new Set<string>());
  const [chats, setChats] = useState<Record<string, Chat[]>>({});
  const [dropFolder, setDropFolder] = useState<DroppedFolder>();
  const [chatAction, setChatAction] = useState<{ shieldId: string; action: ChatAction }>();

  // Replies can arrive out of order. Only the reply to the latest request is used.
  const refresh = () => {
    const request = ++refreshes.current;
    return listShields().then(
      (next) => request === refreshes.current && setShields(next),
      (e) => request === refreshes.current && setError(String(e)),
    );
  };

  const run: Run = (action) => {
    setError(undefined);
    const done = action().then(refresh, (e) => {
      setError(String(e));
      return refresh();
    });
    refresh();
    return done;
  };

  useEffect(() => {
    refresh();
  }, []);

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

  useEffect(() => {
    setPanes((current) => current.filter((p) => shields.some((s) => s.id === p.shieldId)));
  }, [shields]);

  const reloadChats = (id: string) =>
    listChats(id).then((list) => setChats((current) => ({ ...current, [id]: list })), () => {});

  const shieldIds = shields.map((s) => s.id).join(" ");
  useEffect(() => {
    for (const { id } of shields) if (!(id in chats)) reloadChats(id);
  }, [shieldIds]);

  // An app can exit while no view is attached, so the list polls while one runs.
  const appsRunning = shields.some((s) => s.appRuns.some((r) => r.exitCode === null));
  useEffect(() => {
    if (!appsRunning) return;
    const timer = setInterval(refresh, 2000);
    return () => clearInterval(timer);
  }, [appsRunning]);

  const shield = shields.find((s) => s.id === selectedId) ?? shields[0];
  const shieldPanes = panes.filter((p) => p.shieldId === shield?.id);
  const activePane = shield && activePanes[shield.id];
  const events = (shield && net[shield.id]) ?? [];

  // A deleted app has no view.
  useEffect(() => {
    if (view.kind === "app" && shield && !shield.apps.some((a) => a.id === view.appId)) setView({ kind: "overview" });
  }, [view, shield]);

  const blocked = Object.fromEntries(
    shields.map(({ id }) => [id, (net[id] ?? []).filter((e) => isBlocked(e) && e.seq > (lastSeen[id] ?? 0)).length]),
  );

  function showPane(key: string | undefined, shieldId = shield?.id) {
    if (!shieldId) return;
    setActivePanes((current) => ({ ...current, [shieldId]: key ?? "" }));
    setView({ kind: "terminal", key });
  }

  /** Items belong to one shield, so another shield shows its Overview instead. */
  function select(id: string) {
    setSelectedId(id);
    if (id === shield?.id || !["overview", "network", "settings"].includes(view.kind)) setView({ kind: "overview" });
  }

  function openView(shieldId: string, next: View) {
    setSelectedId(shieldId);
    if (next.kind === "terminal") showPane(next.key, shieldId);
    else setView(next);
  }

  async function addShield(folder: string) {
    setSelectedId(await createShield(folder));
    setView({ kind: "overview" });
  }

  async function newShield() {
    const folder = await open({ directory: true, title: "Choose the shield folder" });
    if (folder !== null) await addShield(folder);
  }

  async function newSession(kind: SessionKind, target = shield) {
    if (!target) return;
    if (target.status.state !== "running") await startShield(target.id);
    const key = crypto.randomUUID();
    setPanes((current) => [...current, { key, shieldId: target.id, kind }]);
    setSelectedId(target.id);
    showPane(key, target.id);
  }

  // A folder dropped on empty rail space creates a shield. The map has its own listener.
  useEffect(() => {
    const stop = getCurrentWebview().onDragDropEvent(({ payload }) => {
      if (payload.type !== "drop" || payload.paths.length === 0) return;
      const { x, y } = payload.position.toLogical(window.devicePixelRatio);
      const element = document.elementFromPoint(x, y);
      if (!element?.closest("[data-drop=rail]") || element.closest("[role=treeitem]")) return;
      const dropped = payload.paths[0];
      setError(undefined);
      resolveFolder(dropped).then((folder) => setDropFolder({ dropped, folder }), (e) => setError(String(e)));
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

  /** pi renames a chat, so it must run. A running pi checks its current chat before a delete. */
  async function startChatAction(target: ShieldView, action: ChatAction) {
    const running = target.status.state === "running";
    if (action.kind === "delete" && !running) {
      await deleteChat(target.id, action.chat);
      if (target.id === shield?.id)
        setView((current) => (current.kind === "chat" && current.chat === action.chat ? { kind: "chat" } : current));
      return reloadChats(target.id);
    }
    if (!running) await startShield(target.id);
    if (target.id !== shield?.id) select(target.id);
    setChatAction({ shieldId: target.id, action });
  }

  function cycle(step: number) {
    if (shieldPanes.length === 0) return;
    const index = shieldPanes.findIndex((p) => p.key === activePane);
    showPane(shieldPanes[(index + step + shieldPanes.length) % shieldPanes.length].key);
  }

  function closePane(key: string) {
    const pane = panes.find((p) => p.key === key);
    if (!pane) return;
    const { sessionId } = pane;
    if (sessionId) run(() => closeSession(sessionId));
    setPanes((current) => current.filter((p) => p !== pane));
    const rest = shieldPanes.filter((p) => p !== pane);
    if (key === activePane && view.kind === "terminal") showPane(rest[rest.length - 1]?.key);
    else if (key === activePane) setActivePanes((current) => ({ ...current, [pane.shieldId]: "" }));
  }

  /** Puts the panes of the shield in the order of `keys`, for this app session. */
  const movePanes = (shieldId: string, keys: string[]) =>
    setPanes((current) => [
      ...current.filter((p) => p.shieldId !== shieldId),
      ...keys.map((k) => current.find((p) => p.key === k)!),
    ]);

  const toggle = (shield: ShieldView) =>
    run(() => (shield.status.state === "running" ? stopShield(shield.id) : startShield(shield.id)));

  function startIntent(kind: Intent["kind"], appId?: string) {
    setView({ kind: kind === "allowList" ? "network" : "settings" });
    setIntent({ kind, appId, at: Date.now() });
  }

  const restart = (id: string) => run(() => stopShield(id).then(() => startShield(id)));

  const actions: Record<ShortcutAction, () => void> = {
    // The command menu has no open prop. A click with no pointer opens it and focuses the first item.
    commands: () => commandBar.current?.querySelector("button")?.click(),
    newShield: () => run(newShield),
    newShell: () => run(() => newSession("shell")),
    newPi: () =>
      run(async () => {
        setView({ kind: "chat" });
        if (shield && shield.status.state !== "running") await startShield(shield.id);
      }),
    close: () => view.kind === "terminal" && activePane && closePane(activePane),
    next: () => cycle(1),
    previous: () => cycle(-1),
    overview: () => setView({ kind: "overview" }),
    agents: () => setView({ kind: "chat" }),
    terminal: () => showPane(activePane || shieldPanes[shieldPanes.length - 1]?.key),
    network: () => setView({ kind: "network" }),
    settings: () => setView({ kind: "settings" }),
  };
  useShortcuts(actions);

  return (
    <div className="app">
      <ShieldRail
        shields={shields}
        selectedId={shield?.id}
        view={view}
        blocked={blocked}
        chats={chats}
        panes={panes}
        run={run}
        onSelect={select}
        onOpen={openView}
        onIntent={(id, kind, appId) => {
          setSelectedId(id);
          startIntent(kind, appId);
        }}
        onChatAction={(id, action) => {
          const target = shields.find((s) => s.id === id);
          if (target) run(() => startChatAction(target, action));
        }}
        onToggle={toggle}
        onNewShell={(id) => run(() => newSession("shell", shields.find((s) => s.id === id)))}
        onClosePane={closePane}
        onMovePanes={movePanes}
        onNew={actions.newShield}
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
          onView={setView}
          onToggle={toggle}
          onRestart={restart}
          onIntent={startIntent}
          run={run}
        />
      </ShieldRail>
      <main className="main">
        {shield ? (
          <>
            {shield.pendingRestart && (
              <Card variant="outlined" padding="sm" className="banner row">
                <RotateCw size={16} className="warn" />
                <span className="grow">The running shield uses an older config. Restart it to apply the changes.</span>
                <Button size="sm" variant="outline" onClick={() => restart(shield.id)}>
                  Restart
                </Button>
              </Card>
            )}
            <ShieldPage
              shield={shield}
              events={events}
              view={view}
              onView={setView}
              intent={intent}
              onIntentDone={() => setIntent(undefined)}
              onNetSeen={(seq) => setLastSeen((current) => ({ ...current, [shield.id]: seq }))}
              onChat={(chat) => setView((current) => (current.kind === "chat" ? { kind: "chat", chat } : current))}
              onChatsChange={() => reloadChats(shield.id)}
              chatAction={chatAction?.shieldId === shield.id ? chatAction.action : undefined}
              onChatActionDone={(work) => {
                setChatAction(undefined);
                const id = shield.id;
                run(() => work.finally(() => reloadChats(id)));
              }}
              onDropFolder={dropMount}
              panes={panes}
              activePanes={activePanes}
              run={run}
              onToggle={() => toggle(shield)}
              onNewSession={(kind) => run(() => newSession(kind))}
              onPaneOpen={(key, sessionId) => {
                setPanes((current) => current.map((p) => (p.key === key ? { ...p, sessionId } : p)));
                refresh();
              }}
              onPaneExit={refresh}
              onPaneError={(key, e) => {
                setError(String(e));
                setPanes((current) => current.filter((p) => p.key !== key));
              }}
            />
          </>
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
