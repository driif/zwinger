import { Button, Card, CardBody, CardHeader, CopyField, GlideNav, LiquidSwitch, SpringCheckbox } from "@driif/motiv";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { ExternalLink, RefreshCw, Trash2 } from "lucide-react";
import { ReactNode, useEffect, useState } from "react";
import {
  cleanImages,
  Detection,
  HostAgent,
  Legacy,
  legacyStorage,
  LegacyView,
  Leftover,
  leftoverVmFolders,
  Prefs,
  PrefsPatch,
  proxyCaFingerprint,
  removeLegacyRuntime,
  removeLeftoverVmFolders,
  storage,
  StorageView,
} from "./api";
import { checkUpdate, UpdateInfo } from "../updates/api";
import { AGENT_NAMES, SessionCage } from "../../shared/api";
import type { Run, Runtimes } from "../../App";
import { errorMessage } from "../../shared/errorMessage";
import { HostAgentsSettings } from "./HostAgentsSettings";
import { InstallControl } from "../tasks/roomParts";
import { UpdateBanner } from "../updates/UpdateBanner";
import { AppearancePage } from "../appearance/AppearancePage";
import { Appearance } from "../appearance/themes";
import { TerminalSettings } from "../terminal";
import { Editors } from "../launcher/api";
import { EditorsSettings } from "./EditorsSettings";
import { LibraryCredentials } from "./LibraryCredentials";
import { LegacyKeychain } from "./LegacyKeychain";

const REPO = "https://github.com/driif/zwinger";

const TABS = [
  { id: "agents", label: "Agents on this Mac" },
  { id: "runtimes", label: "Runtimes" },
  { id: "credentials", label: "Credentials" },
  { id: "appearance", label: "Appearance" },
  { id: "terminal", label: "Terminal" },
  { id: "editors", label: "Editors" },
  { id: "updates", label: "Updates" },
  { id: "storage", label: "Storage" },
  { id: "about", label: "About" },
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
            onChange={(autoUpdate) => run(() => onPrefs({ autoUpdate }))}
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

/** The folders of VM keys that nothing uses. The user picks the keys, then Remove asks first. */
function Leftovers({ leftovers, onRemoved }: { leftovers: Leftover[]; onRemoved: (message: string) => void }) {
  const [chosen, setChosen] = useState<string[]>([]);
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const picked = chosen.filter((key) => leftovers.some((l) => l.key === key));

  function remove() {
    setBusy(true);
    removeLeftoverVmFolders(picked)
      .then(
        (removed) => onRemoved(`Removed the folders of ${removed.length} VM ${removed.length === 1 ? "key" : "keys"}.`),
        (e) => onRemoved(errorMessage(e)),
      )
      .finally(() => {
        setBusy(false);
        setAsking(false);
        setChosen([]);
      });
  }

  return (
    <>
      <ul className="list">
        {leftovers.map((l) => (
          <li key={l.key} className="row">
            <span className="grow">
              <SpringCheckbox
                label={<span className="mono">{l.key}</span>}
                title={l.folders.join("\n")}
                checked={picked.includes(l.key)}
                disabled={busy}
                onChange={(e) => setChosen(e.target.checked ? [...picked, l.key] : picked.filter((k) => k !== l.key))}
              />
            </span>
            <span className="muted">{size(l.size)}</span>
          </li>
        ))}
      </ul>
      {asking ? (
        <div className="row">
          <span className="grow muted">
            Remove deletes the agent state, the wrappers and the profiles of {picked.length} VM{" "}
            {picked.length === 1 ? "key" : "keys"}. The agent history in them is lost.
          </span>
          <Button size="sm" variant="ghost" disabled={busy} onClick={() => setAsking(false)}>
            Cancel
          </Button>
          <Button size="sm" variant="danger" loading={busy} onClick={remove}>
            Remove
          </Button>
        </div>
      ) : (
        <div className="row end">
          <Button size="sm" variant="outline" iconLeft={<Trash2 size={14} />} disabled={picked.length === 0} onClick={() => setAsking(true)}>
            Remove selected
          </Button>
        </div>
      )}
    </>
  );
}

function Storage() {
  const [view, setView] = useState<StorageView>();
  const [legacy, setLegacy] = useState<LegacyView[]>([]);
  const [leftovers, setLeftovers] = useState<Leftover[]>([]);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string>();
  const load = () =>
    Promise.all([
      storage().then(setView, (e) => setMessage(errorMessage(e))),
      legacyStorage().then(setLegacy, (e) => setMessage(errorMessage(e))),
      leftoverVmFolders().then(setLeftovers, (e) => setMessage(errorMessage(e))),
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
        <CardHeader title="Storage" titleAs="h2" description="The projects, the VM runtime, the images and the agents of zwinger." />
        <CardBody className="stack">
          {view && <CopyField label="App data" value={view.appData} />}
          <ul className="list">
            <li className="row">
              <span className="grow">Cage images</span>
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
          <p className="muted">Clean up removes the images that no cage uses. A cage downloads its image again when it needs it.</p>
          {leftovers.length > 0 && (
            <>
              <h3>Unused VM folders</h3>
              <p className="muted">
                No project, cage or running VM uses these VM keys. An interrupted removal of a project or cage can leave
                them.
              </p>
              <Leftovers
                leftovers={leftovers}
                onRemoved={(text) => {
                  setMessage(text);
                  load();
                }}
              />
            </>
          )}
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
  const [fingerprint, setFingerprint] = useState<string>();
  const [error, setError] = useState<string>();

  useEffect(() => {
    proxyCaFingerprint().then(setFingerprint, (e) => setError(errorMessage(e)));
  }, []);

  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader
          title={`zwinger ${version ?? ""}`}
          titleAs="h2"
          description="Runs AI agents in microVM cages. The agent sees only the folders that you mount and reaches only the hosts that you allow."
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
      <Card variant="outlined" padding="md">
        <CardHeader
          title="Proxy CA SHA-256"
          titleAs="h2"
          description="Each app start makes a new CA for the proxy. To compare it with the CA in a cage, see Network in the guide."
        />
        <CardBody>
          {fingerprint && (
            <CopyField
              value={fingerprint}
              label="Copy proxy CA SHA-256 fingerprint"
              display={<span style={{ whiteSpace: "normal", overflowWrap: "anywhere" }}>{fingerprint}</span>}
            />
          )}
          {error && <p className="muted">{error}</p>}
        </CardBody>
      </Card>
    </div>
  );
}

interface Props {
  tab: string;
  onTab: (tab: string) => void;
  editors?: Editors;
  /** Looks for the editors again. */
  onDetectEditors: () => void;
  detection?: Detection;
  sessions: SessionCage[];
  runtimes: Runtimes;
  run: Run;
  /** Detects the agents on this Mac again. */
  onRefresh: () => void;
  /** Opens the project's Add agent step for this agent. */
  onUse: (sessionId: string, agent: HostAgent) => void;
  prefs: Prefs;
  onPrefs: (patch: PrefsPatch) => Promise<void>;
  update?: UpdateInfo;
  onUpdate: (update?: UpdateInfo) => void;
  appearance: Appearance;
}

/** The settings of the app, for all projects. */
export function AppSettings({
  tab,
  onTab,
  editors,
  onDetectEditors,
  detection,
  sessions,
  runtimes,
  run,
  onRefresh,
  onUse,
  prefs,
  onPrefs,
  update,
  onUpdate,
  appearance,
}: Props) {
  const [version, setVersion] = useState<string>();

  useEffect(() => {
    getVersion().then(setVersion);
  }, []);

  useEffect(() => {
    if (tab === "runtimes") runtimes.reload();
  }, [tab]);

  const panels: Record<string, ReactNode> = {
    agents: (
      <HostAgentsSettings
        detection={detection}
        sessions={sessions}
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
            description="The Linux builds of the agents, for all projects. A cage uses them when this Mac has no install of the agent."
          />
          <CardBody>
            <ul className="list">
              {runtimes.list.map((runtime) => (
                <li key={runtime.name} className="list">
                  <div className="row">
                    <strong>{AGENT_NAMES[runtime.name]}</strong>
                    <span className="grow muted">
                      {runtime.installed
                        ? `${runtime.version} installed`
                        : runtime.update
                          ? `${runtime.version} is available`
                          : `${runtime.version}, not installed`}
                    </span>
                    {!runtime.installed && <InstallControl runtime={runtime} runtimes={runtimes} />}
                  </div>
                  {runtime.others.map((other) => (
                    <div key={other.version} className="row">
                      <span className="grow muted">{other.version}</span>
                      {other.inUse ? (
                        <span className="muted" title="A running project or the agent on this Mac uses this version.">
                          in use
                        </span>
                      ) : (
                        <Button
                          size="sm"
                          variant="ghost"
                          iconLeft={<Trash2 size={14} />}
                          onClick={() => runtimes.onDelete(runtime.name, other.version)}
                        >
                          Delete
                        </Button>
                      )}
                    </div>
                  ))}
                </li>
              ))}
            </ul>
          </CardBody>
        </Card>
      </div>
    ),
    credentials: (
      <div className="tab-body">
        <LibraryCredentials />
        <LegacyKeychain />
      </div>
    ),
    appearance: <AppearancePage appearance={appearance} prefs={prefs} onPrefs={onPrefs} run={run} />,
    terminal: <TerminalSettings appearance={appearance} prefs={prefs} onPrefs={onPrefs} run={run} />,
    editors: <EditorsSettings editors={editors} onPrefs={onPrefs} run={run} onDetect={onDetectEditors} />,
    updates: <Updates version={version} prefs={prefs} onPrefs={onPrefs} run={run} update={update} onUpdate={onUpdate} />,
    storage: <Storage />,
    about: <About version={version} />,
  };

  return (
    <section className="page">
      <header className="page-header">
        <h1 className="page-title">Settings</h1>
      </header>
      <div className="settings">
        <GlideNav className="settings-nav" label="Settings" items={TABS} value={tab} onChange={onTab} />
        {/* All panels stay mounted, so a draft in one panel stays when the user opens a different one. */}
        {TABS.map(({ id }) => (
          <div key={id} className="settings-panel" hidden={id !== tab}>
            {panels[id]}
          </div>
        ))}
      </div>
    </section>
  );
}
