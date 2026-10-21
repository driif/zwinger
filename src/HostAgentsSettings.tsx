import { Button, Card, CardBody, CardHeader, CommandMenu } from "@driif/motiv";
import { Download, RefreshCw, TriangleAlert } from "lucide-react";
import { useState } from "react";
import { AGENT_NAMES, Detection, GEMINI_NO_LOGIN, HostAgent, RuntimeName, ShieldView } from "./api";
import type { Run, Runtimes } from "./App";
import { InstallControl } from "./AgentsTab";

const CONFIG_DIRS: Record<HostAgent["kind"], string> = {
  claude: "~/.claude",
  codex: "~/.codex",
  pi: "~/.pi/agent",
  gemini: "~/.gemini",
};

export const time = (ms: number) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

function source(agent: HostAgent) {
  if (agent.source === "host") return "Runs from the install on this Mac, mounted read-only.";
  if (agent.source === "cached") return `The Linux build of ${agent.version} is downloaded and shared by all shields.`;
  return `Shields use the pinned build until the Linux build of ${agent.version} is downloaded.`;
}

function login(agent: HostAgent) {
  if (agent.kind === "gemini") return GEMINI_NO_LOGIN;
  if (agent.login.state === "expired") return `The login has expired. Open ${AGENT_NAMES[agent.kind]} on this Mac once to renew it.`;
  if (agent.login.state === "none") return "No subscription login on this Mac.";
  if (agent.kind === "claude")
    return "Login found in the Keychain. When a shield starts, macOS asks once to allow access: select Allow.";
  return agent.login.expires ? `Login found, valid until ${time(agent.login.expires)}.` : "Login found.";
}

interface Props {
  detection?: Detection;
  shields: ShieldView[];
  runtimes: Runtimes;
  run: Run;
  onRefresh: () => void;
  /** Opens the shield's Add agent step for this agent. */
  onUse: (shieldId: string, agent: HostAgent) => void;
}

/** The agents on this Mac, for all shields. */
export function HostAgentsSettings({ detection, shields, runtimes, run, onRefresh, onUse }: Props) {
  /** The agent whose unpinned download waits for a confirm. */
  const [confirming, setConfirming] = useState<RuntimeName>();

  return (
    <section className="page">
      <header className="page-header row">
        <div className="grow">
          <h1 className="page-title">Agents on this Mac</h1>
          <p className="muted">
            Shields can use these agents instead of their own copies, with your profile (read-only) and your subscription
            login. The agent never sees the login: the shield's proxy adds it to requests to the provider only.
          </p>
        </div>
        <Button variant="outline" iconLeft={<RefreshCw size={14} />} loading={!detection} onClick={onRefresh}>
          Refresh detection
        </Button>
      </header>
      <div className="tab-body">
        {detection?.agents.length === 0 && <p className="muted">No supported agent is installed on this Mac.</p>}
        {detection?.agents.map((agent) => {
          const native = agent.kind === "claude" || agent.kind === "codex";
          const runtime = native ? runtimes.list.find((r) => r.name === agent.kind) : undefined;
          return (
            <Card key={agent.kind} variant="outlined" padding="md">
              <CardHeader title={`${AGENT_NAMES[agent.kind]} ${agent.version}`} titleAs="h2" description={agent.path} />
              <CardBody className="stack">
                <span>{source(agent)}</span>
                <span className={agent.login.state === "expired" ? "warn" : undefined}>{login(agent)}</span>
                <span className="muted">
                  {agent.profile
                    ? `Profile: the skills, instructions and settings in ${CONFIG_DIRS[agent.kind]}, copied read-only. Logins, history and sessions stay on this Mac.`
                    : `No profile: ${CONFIG_DIRS[agent.kind]} does not exist.`}
                </span>
                {confirming === agent.kind && (
                  <span className="warn row">
                    <TriangleAlert size={14} />
                    zwinger has no pinned hash for {AGENT_NAMES[agent.kind]} {agent.version}. The download is checked only
                    against the integrity that the npm registry gives.
                  </span>
                )}
                <div className="row end">
                  {native && agent.source === "pinned" && runtime && runtimes.installs[runtime.name] && (
                    <InstallControl runtime={runtime} runtimes={runtimes} />
                  )}
                  {native && agent.source === "pinned" && runtime && !runtimes.installs[runtime.name] && (
                    <>
                      {confirming === agent.kind && (
                        <Button variant="ghost" onClick={() => setConfirming(undefined)}>
                          Cancel
                        </Button>
                      )}
                      <Button
                        variant="outline"
                        iconLeft={<Download size={14} />}
                        onClick={() => {
                          if (!agent.pinned && confirming !== agent.kind) return setConfirming(runtime.name);
                          setConfirming(undefined);
                          runtimes.onInstall(runtime.name, agent.version, !agent.pinned);
                        }}
                      >
                        {confirming === agent.kind ? "Download anyway" : `Download Linux build ${agent.version}`}
                      </Button>
                    </>
                  )}
                  <CommandMenu
                    className="model-picker"
                    menuLabel={`Use ${AGENT_NAMES[agent.kind]} in a shield`}
                    placeholder="Use in shields"
                    commands={shields.map((shield) => ({
                      id: shield.id,
                      label: shield.hostAgents.some((a) => a.kind === agent.kind) ? `${shield.name} (in use)` : shield.name,
                      onSelect: () => run(async () => onUse(shield.id, agent)),
                    }))}
                  />
                </div>
              </CardBody>
            </Card>
          );
        })}
        {detection?.antigravity && (
          <Card variant="outlined" padding="md">
            <CardHeader title="Antigravity" titleAs="h2" description="Not supported yet." />
          </Card>
        )}
      </div>
    </section>
  );
}
