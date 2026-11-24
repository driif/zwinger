import { Button, Dialog, FloatingField, SpringCheckbox } from "@driif/motiv";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderPlus } from "lucide-react";
import { FormEvent, useState } from "react";
import { createHarness } from "./api";
import { AGENT_NAMES, AgentKind, CageView, SessionView } from "../../shared/api";
import { errorMessage } from "../../shared/errorMessage";
import type { Run } from "../../App";

const AGENTS: AgentKind[] = ["claude", "codex", "pi", "gemini"];

interface Props {
  session: SessionView;
  cage: CageView;
  run: Run;
}

/** "Create harness…": a new harness folder with a template for the chosen agents. */
export function CreateHarness({ session, cage, run }: Props) {
  const name = `${session.name.replace(/\//g, "-")}-harness`;
  const parent = session.folder.replace(/\/[^/]+\/?$/, "") || "/";
  const [folder, setFolder] = useState<string>();
  const [agents, setAgents] = useState<AgentKind[]>([]);
  const [error, setError] = useState<string>();
  const [saving, setSaving] = useState(false);

  function start() {
    setFolder(`${parent === "/" ? "" : parent}/${name}`);
    setAgents(AGENTS.filter((a) => a === "pi" || cage.hostAgents.some((h) => h.kind === a)));
    setError(undefined);
  }

  async function pickParent() {
    const chosen = await open({ directory: true, title: "Choose the parent folder of the harness" });
    if (chosen !== null) setFolder(`${chosen.replace(/\/$/, "")}/${name}`);
  }

  function create(event: FormEvent) {
    event.preventDefault();
    if (!folder || saving) return;
    setSaving(true);
    run(() =>
      createHarness(session.id, folder, agents)
        .then(
          () => setFolder(undefined),
          (e) => setError(errorMessage(e)),
        )
        .finally(() => setSaving(false)),
    );
  }

  return (
    <>
      <Button size="sm" variant="outline" iconLeft={<FolderPlus size={14} />} onClick={start}>
        Create harness…
      </Button>
      <Dialog open={folder !== undefined} title="Create harness" onClose={() => setFolder(undefined)}>
        <form className="stack" onSubmit={create}>
          <p className="muted">
            zwinger writes AGENTS.md, task notes and skill folders for the agents. It does not change a file that exists.
            A folder outside the project folder and the mounts becomes a read-write mount.
          </p>
          <div className="row">
            <FloatingField
              label="Harness folder"
              className="grow mono"
              value={folder ?? ""}
              hint={error}
              aria-invalid={error !== undefined}
              showValidity={false}
              disabled={saving}
              onChange={(e) => {
                setFolder(e.target.value);
                setError(undefined);
              }}
            />
            <Button variant="outline" disabled={saving} onClick={pickParent}>
              Choose parent…
            </Button>
          </div>
          <fieldset className="stack">
            <legend>Agents</legend>
            {AGENTS.map((a) => (
              <SpringCheckbox
                key={a}
                label={AGENT_NAMES[a]}
                checked={agents.includes(a)}
                disabled={saving}
                onChange={(e) => setAgents(e.target.checked ? [...agents, a] : agents.filter((x) => x !== a))}
              />
            ))}
          </fieldset>
          <div className="row end">
            <Button variant="ghost" disabled={saving} onClick={() => setFolder(undefined)}>
              Cancel
            </Button>
            <Button type="submit" loading={saving} disabled={saving || !folder?.trim()}>
              Create
            </Button>
          </div>
        </form>
      </Dialog>
    </>
  );
}
