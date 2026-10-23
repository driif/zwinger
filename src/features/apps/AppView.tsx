import { Button } from "@driif/motiv";
import { Play, RotateCw, Square } from "lucide-react";
import { restartApp, startApp, stopApp } from "./api";
import { App, ProjectView } from "../../shared/api";
import type { Run } from "../../App";
import { TerminalView } from "../terminal";

interface Props {
  project: ProjectView;
  app: App;
  run: Run;
  onExit: () => void;
  onError: (error: unknown) => void;
}

/** The output of an app run, with Start, Stop and Restart. */
export function AppView({ project, app, run, onExit, onError }: Props) {
  const appRun = project.appRuns.find((r) => r.appId === app.id);
  const running = appRun?.exitCode === null;
  const cageRunning = project.status.state === "running";
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
          <Button size="sm" variant="secondary" iconLeft={<Square size={14} />} onClick={() => run(() => stopApp(project.id, app.id))}>
            Stop
          </Button>
        ) : (
          <Button
            size="sm"
            iconLeft={<Play size={14} />}
            disabled={!cageRunning}
            onClick={() => run(() => startApp(project.id, app.id))}
          >
            Start
          </Button>
        )}
        <Button
          size="sm"
          variant="outline"
          iconLeft={<RotateCw size={14} />}
          disabled={!cageRunning}
          onClick={() => run(() => restartApp(project.id, app.id))}
        >
          Restart
        </Button>
      </header>
      {appRun ? (
        <div className="pane-layout">
          <TerminalView key={appRun.sessionId} sessionId={appRun.sessionId} visible focused onExit={onExit} onError={onError} />
        </div>
      ) : (
        <div className="empty">
          <p className="muted">
            {cageRunning ? "The app is stopped." : "Start the cage to run the app."} It runs{" "}
            <span className="mono">{app.command}</span> in <span className="mono">{app.cwd}</span>.
          </p>
        </div>
      )}
    </div>
  );
}
