import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { Button, Card } from "@driif/motiv";
import { Plus, RotateCw, Shield } from "lucide-react";
import { closeSession, createShield, listShields, SessionKind, ShieldView, startShield, stopShield } from "./api";
import { CommandBar } from "./CommandBar";
import { ShieldPage } from "./ShieldPage";
import { ShieldRail } from "./ShieldRail";
import { ShortcutAction, useShortcuts } from "./useShortcuts";
import "./App.css";

export type Tab = "overview" | "terminal" | "network" | "settings";

export interface Pane {
  key: string;
  shieldId: string;
  kind: SessionKind;
  sessionId?: string;
}

export type Run = (action: () => Promise<unknown>) => Promise<unknown>;

/** A ⌘K action that a tab completes, such as focusing a field. `at` makes a repeated action new. */
export interface Intent {
  kind: "rename" | "mount" | "harness" | "delete" | "allowList";
  at: number;
}

export default function App() {
  const [shields, setShields] = useState<ShieldView[]>([]);
  const [selectedId, setSelectedId] = useState<string>();
  const [tab, setTab] = useState<Tab>("overview");
  const [panes, setPanes] = useState<Pane[]>([]);
  const [activePanes, setActivePanes] = useState<Record<string, string>>({});
  const [error, setError] = useState<string>();
  const [intent, setIntent] = useState<Intent>();
  const commandBar = useRef<HTMLDivElement>(null);
  const refreshes = useRef(0);

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

  useEffect(() => {
    setPanes((current) => current.filter((p) => shields.some((s) => s.id === p.shieldId)));
  }, [shields]);

  const shield = shields.find((s) => s.id === selectedId) ?? shields[0];
  const shieldPanes = panes.filter((p) => p.shieldId === shield?.id);
  const activePane = shield && activePanes[shield.id];

  function showPane(key: string | undefined) {
    if (!shield) return;
    setActivePanes((current) => ({ ...current, [shield.id]: key ?? "" }));
    setTab("terminal");
  }

  async function newShield() {
    const folder = await open({ directory: true, title: "Choose the shield folder" });
    if (folder === null) return;
    setSelectedId(await createShield(folder));
    setTab("overview");
  }

  async function newSession(kind: SessionKind) {
    if (!shield) return;
    if (shield.status.state !== "running") await startShield(shield.id);
    const key = crypto.randomUUID();
    setPanes((current) => [...current, { key, shieldId: shield.id, kind }]);
    showPane(key);
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
    if (key === activePane) showPane(rest[rest.length - 1]?.key);
  }

  const toggle = (shield: ShieldView) =>
    run(() => (shield.status.state === "running" ? stopShield(shield.id) : startShield(shield.id)));

  function startIntent(kind: Intent["kind"]) {
    setTab(kind === "allowList" ? "network" : "settings");
    setIntent({ kind, at: Date.now() });
  }

  const restart = (id: string) => run(() => stopShield(id).then(() => startShield(id)));

  const actions: Record<ShortcutAction, () => void> = {
    // The command menu has no open prop. A click with no pointer opens it and focuses the first item.
    commands: () => commandBar.current?.querySelector("button")?.click(),
    newShield: () => run(newShield),
    newShell: () => run(() => newSession("shell")),
    newPi: () => run(() => newSession("pi")),
    close: () => activePane && closePane(activePane),
    next: () => cycle(1),
    previous: () => cycle(-1),
    overview: () => setTab("overview"),
    terminal: () => setTab("terminal"),
    network: () => setTab("network"),
    settings: () => setTab("settings"),
  };
  useShortcuts(actions);

  return (
    <div className="app">
      <ShieldRail
        shields={shields}
        selectedId={shield?.id}
        onSelect={setSelectedId}
        onNew={actions.newShield}
      >
        <CommandBar
          ref={commandBar}
          shields={shields}
          shield={shield}
          panes={shieldPanes}
          activePane={activePane}
          actions={actions}
          onSelect={setSelectedId}
          onShowPane={showPane}
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
              tab={tab}
              onTab={setTab}
              intent={intent}
              onIntentDone={() => setIntent(undefined)}
              panes={panes}
              activePanes={activePanes}
              run={run}
              onToggle={() => toggle(shield)}
              onShowPane={showPane}
              onNewSession={(kind) => run(() => newSession(kind))}
              onClosePane={closePane}
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
