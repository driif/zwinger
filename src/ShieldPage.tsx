import { Button, ElasticTabs } from "@driif/motiv";
import { Play, Square } from "lucide-react";
import { useEffect, useState } from "react";
import { listFolders, SessionKind, ShieldView } from "./api";
import type { Intent, Pane, Run, Tab } from "./App";
import { NetworkTab } from "./NetworkTab";
import { SettingsTab } from "./SettingsTab";
import { ShieldMap } from "./ShieldMap";
import { TerminalTab } from "./TerminalTab";

const TABS = [
  { value: "overview", label: "Overview" },
  { value: "terminal", label: "Terminal" },
  { value: "network", label: "Network" },
  { value: "settings", label: "Settings" },
];

interface Props {
  shield: ShieldView;
  tab: Tab;
  onTab: (tab: Tab) => void;
  intent?: Intent;
  onIntentDone: () => void;
  panes: Pane[];
  activePanes: Record<string, string>;
  run: Run;
  onToggle: () => void;
  onShowPane: (key: string) => void;
  onNewSession: (kind: SessionKind) => void;
  onClosePane: (key: string) => void;
  onPaneOpen: (key: string, sessionId: string) => void;
  onPaneExit: () => void;
  onPaneError: (key: string, error: unknown) => void;
}

// Not keyed by shield: the terminal tab keeps the panes of every shield mounted.
export function ShieldPage({
  shield,
  tab,
  onTab,
  intent,
  onIntentDone,
  panes,
  activePanes,
  run,
  onToggle,
  ...terminal
}: Props) {
  const { status } = shield;
  const [listed, setListed] = useState<{ id: string; folders: string[] }>();
  const [reloads, setReloads] = useState(0);
  const folders = listed?.id === shield.id ? listed.folders : [];

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

  return (
    <section className="page">
      <header className="page-header row">
        <div className="grow">
          <h1 className="page-title">{shield.name}</h1>
          <p className="muted row">
            <span className={`dot ${status.state}`} />
            {status.state === "error" ? <span className="danger">{status.message}</span> : status.state}
            <span className="mono grow" title={shield.folder}>
              {shield.folder}
            </span>
          </p>
        </div>
        <Button
          variant={running ? "secondary" : "primary"}
          iconLeft={running ? <Square size={14} /> : <Play size={14} />}
          loading={status.state === "starting" || status.state === "stopping"}
          onClick={onToggle}
        >
          {running ? "Stop" : "Start"}
        </Button>
      </header>
      <ElasticTabs
        className="tabs"
        label="Shield"
        tabs={TABS}
        value={tab}
        onChange={(value) => onTab(value as Tab)}
        panels={{
          overview: (
            <div className="overview">
              <ShieldMap shield={shield} folders={folders} onJump={onTab} />
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
              </dl>
            </div>
          ),
          terminal: (
            <TerminalTab
              shield={shield}
              panes={panes}
              activePane={activePanes[shield.id]}
              visible={tab === "terminal"}
              {...terminal}
            />
          ),
          network: (
            <NetworkTab
              key={shield.id}
              shield={shield}
              run={edit}
              intent={tab === "network" ? intent : undefined}
              onIntentDone={onIntentDone}
            />
          ),
          settings: (
            <SettingsTab
              key={shield.id}
              shield={shield}
              folders={folders}
              run={edit}
              intent={tab === "settings" ? intent : undefined}
              onIntentDone={onIntentDone}
            />
          ),
        }}
      />
    </section>
  );
}
