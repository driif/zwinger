import { Button, Card, CardBody, CardHeader, CopyField, ElasticTabs, LiquidSwitch } from "@driif/motiv";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { ExternalLink, RefreshCw, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import {
  AGENT_NAMES,
  checkUpdate,
  cleanImages,
  Detection,
  HostAgent,
  Legacy,
  legacyStorage,
  LegacyView,
  Prefs,
  removeLegacyRuntime,
  storage,
  StorageView,
  ShieldView,
  UpdateInfo,
} from "./api";
import type { Run, Runtimes } from "./App";
import { errorMessage } from "./errorMessage";
import { HostAgentsSettings } from "./HostAgentsSettings";
import { InstallControl } from "./roomParts";
import { UpdateBanner } from "./UpdateBanner";

const REPO = "https://github.com/driif/zwinger";

const TABS = [
  { value: "agents", label: "Agents on this Mac" },
  { value: "runtimes", label: "Runtimes" },
  { value: "updates", label: "Updates" },
  { value: "storage", label: "Storage" },
  { value: "about", label: "About" },
];

const size = (bytes: number | null) =>
  bytes === null ? "unknown" : bytes < 1e9 ? `${Math.round(bytes / 1e6)} MB` : `${(bytes / 1e9).toFixed(1)} GB`;

function Updates({ version, prefs, onPrefs, run, update, onUpdate }: { version?: string } & Pick<Props, "prefs" | "onPrefs" | "run" | "update" | "onUpdate">) {
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<string>();

  function check() {
    setChecking(true);
    setResult(undefined);
    checkUpdate()
      .then(
        (found) => {
          onUpdate(found ?? undefined);
          if (!found) setResult("zwinger is up to date.");
        },
        (e) => setResult(errorMessage(e)),
      )
      .finally(() => setChecking(false));
  }

  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader
          title={`zwinger ${version ?? ""}`}
          titleAs="h2"
          description="Updates come from the GitHub releases of zwinger. The app checks their signature before it installs them."
          actions={
            <Button size="sm" variant="outline" iconLeft={<RefreshCw size={14} />} loading={checking} onClick={check}>
              Check now
            </Button>
          }
        />
        <CardBody className="stack">
          {result && <p className="muted">{result}</p>}
          <LiquidSwitch
            label="Check for updates when zwinger starts"
            checked={prefs.autoUpdate}
            onChange={(autoUpdate) => run(() => onPrefs({ ...prefs, autoUpdate }))}
          />
        </CardBody>
      </Card>
      {update && <UpdateBanner update={update} />}
    </div>
  );
}

const LEGACY: Record<Legacy, { label: string; confirm: string; done: string }> = {
  appData: {
    label: "Old VM runtime of zwinger",
    confirm: "Remove deletes this folder and the VMs in it. Only zwinger used it.",
    done: "Removed the old VM runtime.",
  },
  shared: {
    label: "zwinger data in ~/.microsandbox",
    confirm: "Remove deletes only the VMs and the image of zwinger. The folder and the data of other tools stay.",
    done: "Removed the VMs and the image of zwinger. The other data in ~/.microsandbox was kept.",
  },
};

/** The microsandbox homes of earlier builds. Remove asks first. */
function LegacyHomes({ homes, onRemoved }: { homes: LegacyView[]; onRemoved: (message: string) => void }) {
  const [asking, setAsking] = useState<Legacy>();
  const [busy, setBusy] = useState(false);

  function remove(legacy: Legacy) {
    setBusy(true);
    removeLegacyRuntime(legacy)
      .then(
        () => onRemoved(LEGACY[legacy].done),
        (e) => onRemoved(errorMessage(e)),
      )
      .finally(() => {
        setBusy(false);
        setAsking(undefined);
      });
  }

  return (
    <ul className="list">
      {homes.map((home) => (
        <li key={home.legacy} className="stack">
          <div className="row">
            <span className="grow">
              {LEGACY[home.legacy].label} <span className="muted mono">{home.path}</span>
            </span>
            <span className="muted">{size(home.size)}</span>
            {asking !== home.legacy && (
              <Button size="sm" variant="outline" iconLeft={<Trash2 size={14} />} disabled={busy} onClick={() => setAsking(home.legacy)}>
                Remove
              </Button>
            )}
          </div>
          {asking === home.legacy && (
            <div className="row">
              <span className="grow muted">{LEGACY[home.legacy].confirm}</span>
              <Button size="sm" variant="ghost" disabled={busy} onClick={() => setAsking(undefined)}>
                Cancel
              </Button>
              <Button size="sm" variant="danger" loading={busy} onClick={() => remove(home.legacy)}>
                Remove
              </Button>
            </div>
          )}
        </li>
      ))}
    </ul>
  );
}

function Storage() {
  const [view, setView] = useState<StorageView>();
  const [legacy, setLegacy] = useState<LegacyView[]>([]);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string>();
  const load = () =>
    Promise.all([
      storage().then(setView, (e) => setMessage(errorMessage(e))),
      legacyStorage().then(setLegacy, (e) => setMessage(errorMessage(e))),
    ]);

  useEffect(() => {
    load();
  }, []);

  function clean() {
    setBusy(true);
    setMessage(undefined);
    cleanImages()
      .then(
        (freed) => setMessage(freed ? `Freed ${size(freed)}.` : "Nothing to clean up."),
        (e) => setMessage(errorMessage(e)),
      )
      .then(load)
      .finally(() => setBusy(false));
  }

  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader title="Storage" titleAs="h2" description="The shields, the VM runtime, the images and the agents of zwinger." />
        <CardBody className="stack">
          {view && <CopyField label="App data" value={view.appData} />}
          <ul className="list">
            <li className="row">
              <span className="grow">Shield images</span>
              <span className="muted">{view ? size(view.images) : "…"}</span>
              <Button size="sm" variant="outline" iconLeft={<Trash2 size={14} />} loading={busy} onClick={clean}>
                Clean up
              </Button>
            </li>
            <li className="row">
              <span className="grow">Agent runtimes</span>
              <span className="muted">{view ? size(view.runtimes) : "…"}</span>
            </li>
          </ul>
          <p className="muted">Clean up removes the images that no shield uses. A shield downloads its image again when it needs it.</p>
          {legacy.length > 0 && (
            <>
              <h3>Data of earlier versions</h3>
              <LegacyHomes
                homes={legacy}
                onRemoved={(text) => {
                  setMessage(text);
                  load();
                }}
              />
            </>
          )}
          {message && <p className="muted">{message}</p>}
        </CardBody>
      </Card>
    </div>
  );
}

function About({ version }: { version?: string }) {
  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader
          title={`zwinger ${version ?? ""}`}
          titleAs="h2"
          description="Runs AI agents in microVM shields. The agent sees only the folders that you mount and reaches only the hosts that you allow."
        />
        <CardBody className="row">
          <Button size="sm" variant="outline" iconLeft={<ExternalLink size={14} />} onClick={() => openUrl(REPO)}>
            Source code
          </Button>
          <Button size="sm" variant="outline" iconLeft={<ExternalLink size={14} />} onClick={() => openUrl(`${REPO}/blob/main/LICENSE`)}>
            License (Apache 2.0)
          </Button>
        </CardBody>
      </Card>
    </div>
  );
}

interface Props {
  detection?: Detection;
  shields: ShieldView[];
  runtimes: Runtimes;
  run: Run;
  /** Detects the agents on this Mac again. */
  onRefresh: () => void;
  /** Opens the shield's Add agent step for this agent. */
  onUse: (shieldId: string, agent: HostAgent) => void;
  prefs: Prefs;
  onPrefs: (prefs: Prefs) => Promise<void>;
  update?: UpdateInfo;
  onUpdate: (update?: UpdateInfo) => void;
}

/** The settings of the app, for all shields. */
export function AppSettings({ detection, shields, runtimes, run, onRefresh, onUse, prefs, onPrefs, update, onUpdate }: Props) {
  const [tab, setTab] = useState("agents");
  const [version, setVersion] = useState<string>();

  useEffect(() => {
    getVersion().then(setVersion);
  }, []);

  return (
    <section className="page">
      <header className="page-header">
        <h1 className="page-title">Settings</h1>
      </header>
      <ElasticTabs
        className="tabs"
        label="Settings"
        tabs={TABS}
        value={tab}
        onChange={setTab}
        panels={{
          agents: (
            <HostAgentsSettings
              detection={detection}
              shields={shields}
              runtimes={runtimes}
              run={run}
              onRefresh={onRefresh}
              onUse={onUse}
            />
          ),
          runtimes: (
            <div className="tab-body">
              <Card variant="outlined" padding="md">
                <CardHeader
                  title="Agent runtimes"
                  titleAs="h2"
                  description="The Linux builds of the agents, for all shields. A shield uses them when this Mac has no install of the agent."
                />
                <CardBody>
                  <ul className="list">
                    {runtimes.list.map((runtime) => (
                      <li key={runtime.name} className="row">
                        <strong>{AGENT_NAMES[runtime.name]}</strong>
                        <span className="grow muted">
                          {runtime.installed ? `${runtime.version} installed` : `${runtime.version}, not installed`}
                        </span>
                        {!runtime.installed && <InstallControl runtime={runtime} runtimes={runtimes} />}
                      </li>
                    ))}
                  </ul>
                </CardBody>
              </Card>
            </div>
          ),
          updates: <Updates version={version} prefs={prefs} onPrefs={onPrefs} run={run} update={update} onUpdate={onUpdate} />,
          storage: <Storage />,
          about: <About version={version} />,
        }}
      />
    </section>
  );
}
