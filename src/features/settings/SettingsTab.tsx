import { Button, Card, CardBody, CardHeader, FloatingField, RadioGroup, Select, TextField } from "@driif/motiv";
import { Trash2 } from "lucide-react";
import { FormEvent, useEffect, useRef, useState } from "react";
import { setHarness, setProjectAccent } from "./api";
import { deleteSession, renameSession, setSessionCage } from "../projects/api";
import { deleteCage } from "../cages/api";
import { CageView, SessionView } from "../../shared/api";
import type { Editors } from "../launcher/api";
import { useThemeColor } from "../appearance/themes";
import type { Intent, Run, Runtimes } from "../../App";
import { errorMessage } from "../../shared/errorMessage";
import { CageSettings, StartDiagnostics, WORKSPACE } from "../cages/CageSettings";
import { sessionCage } from "../cages/sessionCage";

interface Props {
  session: SessionView;
  onProjectEditor: (sessionId: string, editor: string | null) => Promise<void>;
  /** All cages. */
  cages: CageView[];
  /** The default cage of the session's workspace. */
  defaultCage?: CageView;
  /** The cage that only this session uses: Delete offers to delete it too. */
  soleCage?: CageView;
  folders: string[];
  /** `undefined` until the first detection. */
  editors?: Editors;
  runtimes: Runtimes;
  run: Run;
  intent?: Intent;
  onIntentDone: () => void;
}

const HEX = /^#[0-9a-f]{6}$/i;

export function SettingsTab({ session, onProjectEditor, cages, defaultCage, soleCage, folders, editors, runtimes, run, intent, onIntentDone }: Props) {
  const { cage, vmCage } = sessionCage(session, cages);
  // By key, not by `vmCage`: that is null for a planned VM, and for a shared VM that runs for other projects.
  const startHistory = cages.flatMap((c) => c.vms).find((v) => v.key === session.vmKey)?.startHistory ?? [];
  const [name, setName] = useState(session.name);
  const [nameError, setNameError] = useState<string>();
  const [confirmDelete, setConfirmDelete] = useState(false);
  const nameField = useRef<HTMLInputElement>(null);
  const harnessGroup = useRef<HTMLFieldSetElement>(null);
  /** The picker value before it is stored. `undefined`: the field shows the stored accent. `null`: the theme color. */
  const [accentDraft, setAccentDraft] = useState<string | null>();
  /** The text in the Hex field. `undefined`: not edited, the field shows the accent. */
  const [hexDraft, setHexDraft] = useState<string>();
  /** The number of the last accent save. Only that save can reset the draft. */
  const accentSaves = useRef(0);
  const accentField = useRef<HTMLInputElement>(null);

  const harnessOptions = [...folders.map((f) => `${WORKSPACE}/${f}`), ...(cage?.mounts ?? []).map((m) => m.guest)];
  if (session.harness && !harnessOptions.includes(session.harness)) harnessOptions.push(session.harness);

  const enabledEditors = editors?.editors.filter((e) => e.enabled) ?? [];
  const appEditor = editors?.editors.find((e) => e.id === editors.default)?.name ?? "none";
  // A saved editor that is off or not installed stays visible, so the user can clear it.
  const lostEditor =
    session.editor && !enabledEditors.some((e) => e.id === session.editor)
      ? (editors?.names[session.editor] ?? session.editor)
      : undefined;

  const accent = accentDraft === undefined ? session.accent : accentDraft;
  const themeColor = useThemeColor();
  const shownAccent = accent ?? themeColor;
  const hexInvalid = hexDraft !== undefined && !HEX.test(hexDraft);

  // The app shows the error with the project name, also after this tab closes. Its refresh shows the stored accent.
  const saveAccent = (next: string | null) => {
    const save = ++accentSaves.current;
    setAccentDraft(next);
    setHexDraft(undefined);
    run(() =>
      setProjectAccent(session.id, next).catch((e) => {
        // A later save keeps its draft. Else the field goes back to the stored accent.
        if (save === accentSaves.current) setAccentDraft(undefined);
        throw new Error(`${session.name}: the accent color was not saved. ${errorMessage(e)}`);
      }),
    );
  };

  // A Hex field that the user did not edit does not save, so the theme color does not become the project's accent.
  const saveHex = () => {
    if (hexDraft !== undefined && !hexInvalid) saveAccent(hexDraft.toLowerCase());
  };

  // The draft stays until the session list has the stored value, so the field does not jump back.
  useEffect(() => {
    if (session.accent === accentDraft) setAccentDraft(undefined);
  }, [session.accent]);

  // The input event changes the draft only. The native change event comes when the picker commits, and saves.
  useEffect(() => {
    const field = accentField.current;
    if (!field) return;
    const commit = () => saveAccent(field.value);
    field.addEventListener("change", commit);
    return () => field.removeEventListener("change", commit);
  });

  // A blank name reverts to the folder name, as in the backend.
  const nextName = name.trim() || (session.folder.split("/").filter(Boolean).pop() ?? session.folder);

  function rename(event: FormEvent) {
    event.preventDefault();
    setName(nextName);
    run(() => renameSession(session.id, name).catch((e) => setNameError(errorMessage(e))));
  }

  // The cage settings below handle the `mount` intent.
  useEffect(() => {
    if (!intent) return;
    onIntentDone();
    switch (intent.kind) {
      case "rename":
        nameField.current?.focus();
        nameField.current?.select();
        break;
      case "harness":
        harnessGroup.current?.querySelector<HTMLInputElement>("input:checked")?.focus();
        break;
      case "delete":
        setConfirmDelete(true);
    }
  }, [intent]);

  const busy = vmCage?.status.state === "starting" || vmCage?.status.state === "stopping";

  return (
    <div className="tab-body">
      <Card variant="outlined" padding="md">
        <CardHeader title="Project" titleAs="h2" />
        <CardBody className="stack">
          <form className="row" onSubmit={rename}>
            <FloatingField
              ref={nameField}
              label="Name"
              className="grow"
              value={name}
              maxLength={64}
              hint={nameError}
              aria-invalid={nameError !== undefined}
              showValidity={false}
              onChange={(e) => {
                setName(e.target.value);
                setNameError(undefined);
              }}
            />
            <Button type="submit" variant="outline" disabled={nextName === session.name}>
              Rename
            </Button>
          </form>
          <p className="muted">
            Folder <span className="mono">{session.folder}</span> is{" "}
            <span className="mono">{session.guestFolder ?? WORKSPACE}</span> in the cage. It is read-write.
          </p>
          <Select
            label="Cage"
            options={[
              { value: "", label: `Inherit from workspace (${defaultCage?.name ?? "no default cage"})` },
              ...cages.map((c) => ({ value: c.id, label: c.name })),
            ]}
            value={session.cage ?? ""}
            onChange={(id) => run(() => setSessionCage(session.id, id || null))}
          />
          {!cage && <p className="muted">With no cage, the project opens Mac shells only.</p>}
          <div className="row">
            <TextField
              ref={accentField}
              type="color"
              label="Accent color"
              value={shownAccent}
              hint={accent === null ? "Uses the theme color." : undefined}
              onChange={(e) => {
                setAccentDraft(e.target.value);
                setHexDraft(undefined);
              }}
            />
            <TextField
              label="Hex"
              value={hexDraft ?? shownAccent}
              maxLength={7}
              spellCheck={false}
              hint={hexInvalid ? "Enter a color as #rrggbb." : undefined}
              aria-invalid={hexInvalid}
              onChange={(e) => setHexDraft(e.target.value)}
              onBlur={saveHex}
              onKeyDown={(e) => {
                if (e.key === "Enter") saveHex();
              }}
            />
            <Button variant="outline" disabled={accent === null} onClick={() => saveAccent(null)}>
              Use theme color
            </Button>
          </div>
          {editors && (
            <Select
              label="Editor"
              options={[
                { value: "", label: `App default (${appEditor})` },
                ...enabledEditors.map((e) => ({ value: e.id, label: e.name })),
                ...(lostEditor && session.editor ? [{ value: session.editor, label: `${lostEditor} (not available)` }] : []),
              ]}
              value={session.editor ?? ""}
              onChange={(id) => run(() => onProjectEditor(session.id, id || null))}
            />
          )}
          {editors && (
            <p className="muted">
              An editor that is not available falls back to the app default. If the project has its own editor, an editor
              that you choose from a menu changes only this project.
            </p>
          )}
        </CardBody>
      </Card>

      {cage && (
        <Card variant="outlined" padding="md">
          <CardHeader
            title="Harness"
            titleAs="h2"
            description="The folder where the pi agent starts. Put its skills and instructions there."
          />
          <CardBody>
            <RadioGroup
              ref={harnessGroup}
              name={`harness-${session.id}`}
              legend="Agent starts in"
              options={[
                { value: "", label: <span className="mono">{WORKSPACE}</span>, hint: "No harness" },
                ...harnessOptions.map((path) => ({ value: path, label: <span className="mono">{path}</span> })),
              ]}
              value={session.harness ?? ""}
              onChange={(value) => run(() => setHarness(session.id, value || null))}
            />
          </CardBody>
        </Card>
      )}

      {cage && <CageSettings key={cage.id} cage={cage} vm={vmCage} runtimes={runtimes} run={run} intent={intent} />}
      {/* Outside the `cage` guard: a running VM keeps its diagnostics when the project has no next cage. */}
      <StartDiagnostics vm={vmCage} startHistory={startHistory} />

      <Card variant="outlined" padding="md">
        <CardHeader
          title="Delete project"
          titleAs="h2"
          description={
            soleCage
              ? `Stops the VM of the project. Your folders stay. No other project uses cage ${soleCage.name}: you can delete it and its credential values too.`
              : "Stops the VM of the project. The cage and your folders stay."
          }
        />
        <CardBody className="row">
          {confirmDelete ? (
            <>
              <span className="grow danger">Delete {session.name}?</span>
              <Button variant="ghost" onClick={() => setConfirmDelete(false)}>
                Cancel
              </Button>
              <Button variant="danger" iconLeft={<Trash2 size={14} />} onClick={() => run(() => deleteSession(session.id))}>
                Delete
              </Button>
              {soleCage && (
                <Button
                  variant="danger"
                  iconLeft={<Trash2 size={14} />}
                  onClick={() => run(() => deleteSession(session.id).then(() => deleteCage(soleCage.id)))}
                >
                  Delete with cage {soleCage.name}
                </Button>
              )}
            </>
          ) : (
            <Button variant="danger" iconLeft={<Trash2 size={14} />} disabled={busy} onClick={() => setConfirmDelete(true)}>
              Delete project…
            </Button>
          )}
        </CardBody>
      </Card>
    </div>
  );
}
