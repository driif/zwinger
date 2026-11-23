import { Badge, Button, Card, CardBody, CardHeader, FloatingField } from "@driif/motiv";
import { ArrowLeft, Copy, Pencil, Plus, Settings, Trash2 } from "lucide-react";
import { FormEvent, useState } from "react";
import { copyName, createCage, deleteCage, duplicateCage, renameCage } from "./api";
import { CageSettings, StartDiagnostics } from "./CageSettings";
import { CageView, SessionView, Workspace } from "../../shared/api";
import type { Run, Runtimes } from "../../App";

interface Props {
  cages: CageView[];
  sessions: SessionView[];
  workspaces: Workspace[];
  runtimes: Runtimes;
  run: Run;
  /** The id of the cage that the Edit view shows. `undefined`: the list shows. */
  editing?: string;
  onEdit: (cageId: string | undefined) => void;
}

/** The reason why the cage cannot be deleted, as the backend checks it. `undefined`: it can be deleted. */
function inUse(cage: CageView, sessions: SessionView[], workspaces: Workspace[]) {
  const session = sessions.find((s) => s.effectiveCage?.id === cage.id);
  if (session) return `Session ${session.name} uses it.`;
  const workspace = workspaces.find((w) => w.defaultCage === cage.id);
  if (workspace) return `It is the default cage of workspace ${workspace.name}.`;
  if (cage.vms.some((v) => v.status.state !== "stopped" && v.status.state !== "error")) return "Its VM runs.";
}

/** The network policy of the cage, read-only. The Network tab of a project that uses the cage edits it. */
function NetworkSummary({ cage }: { cage: CageView }) {
  const { network } = cage;
  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Network policy"
        titleAs="h2"
        description="Edit it in the Network tab of a project that uses this cage."
      />
      <CardBody className="stack">
        <p>
          {network.offline ? "Offline: all requests are blocked. " : ""}
          Hosts with no rule: {network.default === "allow" ? "allowed" : network.default === "ask" ? "ask" : "blocked"}.
          {network.hostPort ? ` Port ${network.hostPort} of this Mac is open to the cage.` : ""}
        </p>
        {network.rules.length === 0 && <p className="muted">No rules.</p>}
        {network.rules.map((rule) => (
          <p key={rule.id} className="row">
            <Badge tone={rule.action === "allow" ? "success" : "danger"}>{rule.action === "allow" ? "Allow" : "Block"}</Badge>
            <span className="mono grow">{rule.target.kind === "host" ? rule.target.host : rule.target.id}</span>
            {(rule.credentials?.length ?? 0) > 0 && <span className="muted">{rule.credentials?.map((c) => c.name).join(", ")}</span>}
          </p>
        ))}
      </CardBody>
    </Card>
  );
}

/** The list of cages: create, rename, edit and delete. */
export function CagesPage({ cages, sessions, workspaces, runtimes, run, editing, onEdit }: Props) {
  const [name, setName] = useState("");
  const [renaming, setRenaming] = useState<{ id: string; name: string }>();
  const [confirming, setConfirming] = useState<string>();
  const edited = cages.find((c) => c.id === editing);

  function create(event: FormEvent) {
    event.preventDefault();
    run(() => createCage(name).then(() => setName("")));
  }

  function rename(event: FormEvent) {
    event.preventDefault();
    if (!renaming) return;
    run(() => renameCage(renaming.id, renaming.name).then(() => setRenaming(undefined)));
  }

  if (edited) {
    const editedVm = { ...edited, ...edited.vms[0] };
    return (
      <section className="page">
        <header className="page-header row">
          <Button size="sm" variant="ghost" iconLeft={<ArrowLeft size={14} />} onClick={() => onEdit(undefined)}>
            All cages
          </Button>
          <h1 className="page-title grow">{edited.name}</h1>
        </header>
        <div className="tab-body">
          <CageSettings key={edited.id} cage={edited} vm={editedVm} runtimes={runtimes} run={run} />
          <NetworkSummary cage={edited} />
          <StartDiagnostics vm={editedVm} startHistory={editedVm.startHistory} />
        </div>
      </section>
    );
  }

  return (
    <section className="page">
      <header className="page-header">
        <h1 className="page-title">Cages</h1>
        <p className="muted">
          A cage is the sandbox config of a VM: its mounts, network rules and agents. A project uses its own cage or the
          default cage of its workspace. A cage runs one project at a time.
        </p>
      </header>
      <div className="tab-body">
        <form className="row" onSubmit={create}>
          <FloatingField label="New cage name" className="grow" value={name} maxLength={64} onChange={(e) => setName(e.target.value)} />
          <Button type="submit" variant="outline" iconLeft={<Plus size={14} />} disabled={!name.trim()}>
            Create cage
          </Button>
        </form>
        {cages.length === 0 && <p className="muted">No cages yet.</p>}
        {cages.map((cage) => {
          const users = sessions.filter((s) => s.effectiveCage?.id === cage.id);
          const reason = inUse(cage, sessions, workspaces);
          return (
            <Card key={cage.id} variant="outlined" padding="md">
              <CardHeader
                titleAs="h2"
                title={
                  renaming?.id === cage.id ? (
                    <form className="row" onSubmit={rename}>
                      <FloatingField
                        label="Name"
                        className="grow"
                        value={renaming.name}
                        maxLength={64}
                        autoFocus
                        onChange={(e) => setRenaming({ id: cage.id, name: e.target.value })}
                      />
                      <Button size="sm" variant="ghost" onClick={() => setRenaming(undefined)}>
                        Cancel
                      </Button>
                      <Button size="sm" type="submit">
                        Save
                      </Button>
                    </form>
                  ) : (
                    cage.name
                  )
                }
                description={
                  users.length === 0
                    ? "No project uses it."
                    : `Used by ${users.map((s) => `${s.name}${s.effectiveCage?.inherited ? " (inherited)" : ""}`).join(", ")}.`
                }
                actions={
                  confirming === cage.id ? (
                    <>
                      <span className="danger">Delete {cage.name} and its secret values?</span>
                      <Button size="sm" variant="ghost" onClick={() => setConfirming(undefined)}>
                        Cancel
                      </Button>
                      <Button
                        size="sm"
                        variant="danger"
                        iconLeft={<Trash2 size={14} />}
                        onClick={() => run(() => deleteCage(cage.id).finally(() => setConfirming(undefined)))}
                      >
                        Delete
                      </Button>
                    </>
                  ) : (
                    <>
                      <Button
                        size="sm"
                        variant="ghost"
                        iconLeft={<Pencil size={14} />}
                        onClick={() => setRenaming({ id: cage.id, name: cage.name })}
                      >
                        Rename
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        iconLeft={<Copy size={14} />}
                        onClick={() => run(() => duplicateCage(cage.id, copyName(cage.name)))}
                      >
                        Duplicate
                      </Button>
                      <Button size="sm" variant="outline" iconLeft={<Settings size={14} />} onClick={() => onEdit(cage.id)}>
                        Edit
                      </Button>
                      {/* A disabled button gets no pointer events, so the span shows the reason. */}
                      <span title={reason}>
                        <Button
                          size="sm"
                          variant="danger"
                          iconLeft={<Trash2 size={14} />}
                          disabled={reason !== undefined}
                          onClick={() => setConfirming(cage.id)}
                        >
                          Delete…
                        </Button>
                      </span>
                    </>
                  )
                }
              />
            </Card>
          );
        })}
      </div>
    </section>
  );
}
