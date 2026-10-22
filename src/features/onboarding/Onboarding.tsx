import { Button, Card, CardBody, CardHeader, RadioGroup, StepProgress } from "@driif/motiv";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowRight, Bot, CircleCheck, CircleX, Folder, FolderOpen, Globe, Shield, TriangleAlert } from "lucide-react";
import { ReactNode, useEffect, useState } from "react";
import { Startup, StartupStep } from "./api";
import { Detection, hostAgents, putHostAgent } from "../settings/api";
import { createShield, resolveFolder } from "../shields/api";
import { detectEditors, Editors } from "../launcher/api";
import { AGENT_NAMES, AgentKind } from "../../shared/api";
import { errorMessage } from "../../shared/errorMessage";
import "./onboarding.css";

const STEPS = [
  { id: "welcome", label: "Check this Mac" },
  { id: "shield", label: "Create a shield" },
  { id: "agent", label: "Add an agent" },
  { id: "editor", label: "Your editor" },
  { id: "done", label: "Done" },
];

const PHASES: Record<Startup["phase"], string> = {
  checking: "Checking this Mac…",
  installing: "Preparing the VM runtime. The first start takes a moment.",
  ready: "This Mac is ready for shields.",
  blocked: "zwinger cannot run shields on this Mac until the failed check passes.",
};

const RUNTIME_STEPS: { id: StartupStep; label: string }[] = [
  { id: "resolve", label: "Find the runtime" },
  { id: "reapLegacy", label: "Stop old VMs" },
  { id: "install", label: "Extract and verify" },
  { id: "reap", label: "Stop the last run's VMs" },
  { id: "ready", label: "Ready" },
];

/** The runtime install has no byte progress, so the current step shows its time. */
function RuntimeSteps({ step, startedAt }: { step: StartupStep; startedAt: number }) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  const current = RUNTIME_STEPS.findIndex((s) => s.id === step);
  const seconds = Math.max(0, Math.floor((now - startedAt) / 1000));
  return (
    <StepProgress
      label="VM runtime"
      steps={RUNTIME_STEPS.map((s, i) => (i === current ? { ...s, description: `${seconds} s` } : s))}
      current={current}
      completed={step === "ready"}
    />
  );
}

/** The system checks of the app start and the runtime install. */
export function SystemChecks({ startup }: { startup: Startup }) {
  return (
    <div className="stack">
      <ul className="list">
        {startup.checks.map((check) => (
          <li key={check.name} className="row">
            {check.level === "pass" ? (
              <CircleCheck size={16} className="ok" />
            ) : check.level === "warn" ? (
              <TriangleAlert size={16} className="warn" />
            ) : (
              <CircleX size={16} className="danger" />
            )}
            <strong>{check.name}</strong>
            <span className={check.level === "fail" ? "danger" : "muted"}>{check.message}</span>
          </li>
        ))}
      </ul>
      {startup.step && startup.phase !== "blocked" && <RuntimeSteps step={startup.step} startedAt={startup.startedAt} />}
      <p className={startup.phase === "blocked" ? "danger" : "muted"}>{PHASES[startup.phase]}</p>
    </div>
  );
}

/** Shown on a later start until the runtime is ready, or when a check fails. */
export function StartupScreen({ startup, banner }: { startup: Startup; banner?: ReactNode }) {
  return (
    <div className="onboarding">
      {banner}
      <h1 className="page-title">Starting zwinger</h1>
      <SystemChecks startup={startup} />
    </div>
  );
}

/**
 * The first run: the checks, a first shield, an agent from this Mac, the default editor. `onDone` opens the shield, if
 * one was made.
 */
export function Onboarding({
  startup,
  banner,
  onEditor,
  onDone,
}: {
  startup: Startup;
  /** The update banner: a blocked start can still update. */
  banner?: ReactNode;
  /** Saves the default editor. */
  onEditor: (id: string) => Promise<void>;
  onDone: (shieldId?: string) => Promise<void>;
}) {
  const [step, setStep] = useState(0);
  const [shieldId, setShieldId] = useState<string>();
  const [detection, setDetection] = useState<Detection>();
  const [added, setAdded] = useState<AgentKind[]>([]);
  const [editors, setEditors] = useState<Editors>();
  const [editor, setEditor] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  async function act(action: () => Promise<void>) {
    setBusy(true);
    setError(undefined);
    try {
      await action();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  }

  async function create(folder: string) {
    setShieldId(await createShield(await resolveFolder(folder)));
    setStep(2);
  }

  // Step 2 also takes a folder that is dropped on the window.
  useEffect(() => {
    if (step !== 1) return;
    const stop = getCurrentWebview().onDragDropEvent(({ payload }) => {
      if (payload.type === "drop" && payload.paths.length > 0) act(() => create(payload.paths[0]));
    });
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, [step]);

  useEffect(() => {
    if (step === 2 && !detection) hostAgents().then(setDetection, (e) => setError(errorMessage(e)));
    if (step === 3 && !editors)
      detectEditors().then((found) => {
        setEditors(found);
        setEditor(found.default ?? undefined);
      }, (e) => setError(errorMessage(e)));
  }, [step]);

  const add = (kind: AgentKind, profile: boolean, login: boolean) =>
    act(async () => {
      // The login needs its provider host, so the shield allows it.
      await putHostAgent(shieldId!, { kind, profile, login }, login);
      setAdded((current) => [...current, kind]);
    });

  const finish = () => act(() => onDone(shieldId));

  return (
    <div className="onboarding">
      {banner}
      <h1 className="page-title">Welcome to zwinger</h1>
      <StepProgress label="First steps" steps={STEPS} current={step} completed={step === 4} />
      {step === 0 && (
        <Card variant="outlined" padding="md">
          <CardHeader title="What a shield is" titleAs="h2" />
          <CardBody className="stack">
            <p>A shield runs an AI agent in a small Linux VM on this Mac.</p>
            <p>The agent sees only the folders that you mount, and reaches only the hosts that you allow.</p>
            <p>Your logins stay on this Mac: the shield adds them to requests, and the agent sees a placeholder.</p>
            <div className="row onboarding-picture" aria-hidden>
              <Folder size={28} />
              <ArrowRight size={16} className="muted" />
              <Shield size={36} className="ok" />
              <ArrowRight size={16} className="muted" />
              <Globe size={28} />
            </div>
            <SystemChecks startup={startup} />
          </CardBody>
        </Card>
      )}
      {step === 1 && (
        <Card variant="outlined" padding="md">
          <CardHeader
            title="Create your first shield"
            titleAs="h2"
            description="Pick the project folder. The agent works in it as /workspace. You can also drop the folder on this window."
          />
          <CardBody className="row">
            <Button
              iconLeft={<FolderOpen size={16} />}
              loading={busy}
              onClick={() =>
                act(async () => {
                  const folder = await open({ directory: true, title: "Choose the shield folder" });
                  if (folder !== null) await create(folder);
                })
              }
            >
              Choose folder
            </Button>
          </CardBody>
        </Card>
      )}
      {step === 2 && (
        <Card variant="outlined" padding="md">
          <CardHeader
            title="Add an agent"
            titleAs="h2"
            description="The shield can use the agents of this Mac, with your profile (read-only) and your subscription login. You can change this later in Settings."
          />
          <CardBody className="stack">
            {!detection && <p className="muted">Looking for agents on this Mac…</p>}
            {detection?.agents.length === 0 && (
              <p className="muted">No agent is installed on this Mac. The shield has pi built in; add Claude Code or Codex later in Settings.</p>
            )}
            <ul className="list">
              {detection?.agents.map((agent) => {
                const login = agent.kind !== "gemini" && agent.login.state === "found";
                return (
                  <li key={agent.kind} className="row">
                    <Bot size={16} />
                    <strong>
                      {AGENT_NAMES[agent.kind]} {agent.version}
                    </strong>
                    <span className="grow muted">{login ? "with your login" : "no login on this Mac"}</span>
                    {added.includes(agent.kind) ? (
                      <span className="ok row">
                        <CircleCheck size={14} /> Added
                      </span>
                    ) : (
                      <Button size="sm" variant="outline" disabled={busy} onClick={() => add(agent.kind, agent.profile, login)}>
                        Add
                      </Button>
                    )}
                  </li>
                );
              })}
            </ul>
            {added.includes("claude") && (
              <p className="muted">When the shield starts, macOS asks once to allow access to the Claude login: select Allow.</p>
            )}
          </CardBody>
        </Card>
      )}
      {step === 3 && (
        <Card variant="outlined" padding="md">
          <CardHeader
            title="Your editor"
            titleAs="h2"
            description="Open in editor opens the shield folder, or the current folder of a terminal, in this editor. You can change it later in Settings."
          />
          <CardBody className="stack">
            {!editors && <p className="muted">Looking for editors on this Mac…</p>}
            {editors?.editors.length === 0 && <p className="muted">No known editor is installed. You can detect editors again later in Settings.</p>}
            {editor && editors && (
              <RadioGroup
                name="default-editor"
                legend="Default editor"
                options={editors.editors.map((e) => ({ value: e.id, label: e.name }))}
                value={editor}
                onChange={setEditor}
              />
            )}
          </CardBody>
        </Card>
      )}
      {step === 4 && (
        <Card variant="outlined" padding="md">
          <CardHeader title="Your shield is ready" titleAs="h2" />
          <CardBody className="stack">
            <p>Start the shield, then open an agent with ⌘P or a shell with ⌘T.</p>
            <p className="muted">⌘K shows all actions. ⌘, opens Settings.</p>
          </CardBody>
        </Card>
      )}
      {error && <p className="danger">{error}</p>}
      <div className="row end">
        {step >= 1 && step <= 3 && (
          <Button variant="ghost" disabled={busy} onClick={finish}>
            Skip
          </Button>
        )}
        {step === 0 && (
          <Button
            loading={startup.phase === "checking" || startup.phase === "installing"}
            disabled={startup.phase !== "ready"}
            onClick={() => setStep(1)}
          >
            Next
          </Button>
        )}
        {step === 2 && (
          <Button disabled={busy} onClick={() => setStep(3)}>
            Next
          </Button>
        )}
        {step === 3 && (
          <Button
            loading={busy}
            onClick={() =>
              act(async () => {
                if (editor && editor !== editors?.default) await onEditor(editor);
                setStep(4);
              })
            }
          >
            Next
          </Button>
        )}
        {step === 4 && (
          <Button loading={busy} onClick={finish}>
            Open the shield
          </Button>
        )}
      </div>
    </div>
  );
}
