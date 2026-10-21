import { Button } from "@driif/motiv";
import { Play, RotateCw, Square } from "lucide-react";
import { App, restartApp, ShieldView, startApp, stopApp } from "./api";
import type { Run } from "./App";
import { TerminalPane } from "./TerminalPane";

interface Props {
  shield: ShieldView;
  app: App;
  run: Run;
  onExit: () => void;
  onError: (error: unknown) => void;
}

/** The output of an app run, with Start, Stop and Restart. */
export function AppView({ shield, app, run, onExit, onError }: Props) {
  const appRun = shield.appRuns.find((r) => r.appId === app.id);
  const running = appRun?.exitCode === null;
  const shieldRunning = shield.status.state === "running";
  const state = !appRun ? "stopped" : running ? "running" : `exited ${appRun.exitCode}`;

  return (
    <div className="terminal-tab">
      <header className="row">
        <div className="grow">
          <h2 className="row">
            {app.name} <span className="muted">· {state}</span>
          </h2>
          <p className="muted mono grow" title={app.command}>
            {app.cwd} $ {app.command}
          </p>
        </div>
        {running ? (
          <Button size="sm" variant="secondary" iconLeft={<Square size={14} />} onClick={() => run(() => stopApp(shield.id, app.id))}>
            Stop
          </Button>
        ) : (
          <Button
            size="sm"
            iconLeft={<Play size={14} />}
            disabled={!shieldRunning}
            onClick={() => run(() => startApp(shield.id, app.id))}
          >
            Start
          </Button>
        )}
        <Button
          size="sm"
          variant="outline"
          iconLeft={<RotateCw size={14} />}
          disabled={!shieldRunning}
          onClick={() => run(() => restartApp(shield.id, app.id))}
        >
          Restart
        </Button>
      </header>
      {appRun ? (
        <TerminalPane key={appRun.sessionId} session={{ attach: appRun.sessionId }} active onExit={onExit} onError={onError} />
      ) : (
        <div className="empty">
          <p className="muted">
            {shieldRunning ? "The app is stopped." : "Start the shield to run the app."} It runs{" "}
            <span className="mono">{app.command}</span> in <span className="mono">{app.cwd}</span>.
          </p>
        </div>
      )}
    </div>
  );
}
