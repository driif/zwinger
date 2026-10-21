import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { closeSession, createShield, listShields, SessionKind, ShieldView, startShield } from "./api";
import { Inspector } from "./Inspector";
import { Pane, Sidebar } from "./Sidebar";
import { TerminalPane } from "./TerminalPane";
import { useShortcuts } from "./useShortcuts";
import "./App.css";

export default function App() {
  const [shields, setShields] = useState<ShieldView[]>([]);
  const [panes, setPanes] = useState<Pane[]>([]);
  const [selectedShield, setSelectedShield] = useState<string>();
  const [selectedPane, setSelectedPane] = useState<string>();
  const [error, setError] = useState<string>();

  const refresh = () => listShields().then(setShields, (e) => setError(String(e)));

  function run(action: () => Promise<unknown>) {
    setError(undefined);
    const done = action().then(refresh, (e) => {
      setError(String(e));
      return refresh();
    });
    refresh();
    return done;
  }

  useEffect(() => {
    listShields().then((loaded) => {
      setShields(loaded);
      setSelectedShield((current) => current ?? loaded[0]?.id);
    }, (e) => setError(String(e)));
  }, []);

  useEffect(() => {
    setPanes((current) => current.filter((p) => shields.some((s) => s.id === p.shieldId)));
  }, [shields]);

  const shield = shields.find((s) => s.id === selectedShield);

  function selectPane(key: string | undefined) {
    setSelectedPane(key);
    const pane = panes.find((p) => p.key === key);
    if (pane) setSelectedShield(pane.shieldId);
  }

  async function newShield() {
    const folder = await open({ directory: true, title: "Choose the shield folder" });
    if (folder === null) return;
    setSelectedShield(await createShield(folder));
  }

  async function newSession(kind: SessionKind) {
    if (!shield) return;
    if (shield.status.state !== "running") await startShield(shield.id);
    const key = crypto.randomUUID();
    setPanes((current) => [...current, { key, shieldId: shield.id, kind }]);
    setSelectedPane(key);
  }

  function cycle(step: number) {
    if (panes.length === 0) return;
    const index = panes.findIndex((p) => p.key === selectedPane);
    selectPane(panes[(index + step + panes.length) % panes.length].key);
  }

  function closePane() {
    const pane = panes.find((p) => p.key === selectedPane);
    if (!pane) return;
    const { sessionId } = pane;
    if (sessionId) run(() => closeSession(sessionId));
    const rest = panes.filter((p) => p !== pane);
    setPanes(rest);
    setSelectedPane(rest[rest.length - 1]?.key);
  }

  useShortcuts({
    newShield: () => run(newShield),
    newShell: () => run(() => newSession("shell")),
    newPi: () => run(() => newSession("pi")),
    close: closePane,
    next: () => cycle(1),
    previous: () => cycle(-1),
  });

  return (
    <div className="app">
      <Sidebar
        shields={shields}
        panes={panes}
        selectedShield={selectedShield}
        selectedPane={selectedPane}
        onSelectShield={setSelectedShield}
        onSelectPane={selectPane}
      />
      <main className="main">
        {panes.map((pane) => (
          <TerminalPane
            key={pane.key}
            shieldId={pane.shieldId}
            kind={pane.kind}
            active={pane.key === selectedPane}
            onOpen={(sessionId) => {
              setPanes((current) => current.map((p) => (p.key === pane.key ? { ...p, sessionId } : p)));
              refresh();
            }}
            onExit={refresh}
            onError={(e) => {
              setError(String(e));
              setPanes((current) => current.filter((p) => p.key !== pane.key));
            }}
          />
        ))}
        {panes.length === 0 && (
          <p className="empty muted">
            {shield ? "⌘T opens a shell, ⌘P opens pi in this shield." : "⌘N creates a shield from a folder."}
          </p>
        )}
        {error && <p className="status danger mono">{error}</p>}
      </main>
      {shield && <Inspector shield={shield} run={run} />}
    </div>
  );
}
