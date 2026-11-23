import {
  Accordion,
  Button,
  Card,
  CardBody,
  CardHeader,
  Confirm,
  FloatingField,
  Select,
} from "@driif/motiv";
import { Save, X } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import type { Run } from "../../App";
import type { NetworkPolicy } from "../../shared/api";
import { errorMessage } from "../../shared/errorMessage";
import "../tasks/tasks.css";
import {
  applyNetworkProfile,
  deleteNetworkProfile,
  networkProfiles,
  policyJson,
  Profile,
  saveNetworkProfile,
} from "./api";

interface Props {
  cageId: string;
  /** The saved policy. A change loads the JSON view again. */
  network: NetworkPolicy;
  run: Run;
}

/** The network profiles and the JSON view of the effective policy. */
export function PolicyProfiles({ cageId, network, run }: Props) {
  const [profiles, setProfiles] = useState<Profile[]>([]);
  const [name, setName] = useState("");
  const [problem, setProblem] = useState<string>();
  const [chosen, setChosen] = useState("");
  const [applying, setApplying] = useState(false);
  const [open, setOpen] = useState<string[]>([]);
  const [json, setJson] = useState("");

  const load = () => networkProfiles().then(setProfiles);

  useEffect(() => {
    load();
  }, []);

  useEffect(() => {
    if (open.length > 0)
      policyJson(cageId).then(
        (policy) => setJson(JSON.stringify(policy, null, 2)),
        (e) => setJson(errorMessage(e)),
      );
  }, [open, cageId, network]);

  function save(event: FormEvent) {
    event.preventDefault();
    run(() =>
      saveNetworkProfile(cageId, name).then(
        () => {
          setChosen(name.trim());
          setName("");
          return load();
        },
        (e) => setProblem(errorMessage(e)),
      ),
    );
  }

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Profiles"
        titleAs="h2"
        description="A profile is a named copy of the default and the rules, with no credentials. Apply copies it into this cage. A later change of the profile does not change the cage."
      />
      <CardBody className="stack">
        <form className="row" onSubmit={save}>
          <FloatingField
            label="Profile name"
            className="grow"
            value={name}
            hint={problem}
            aria-invalid={problem !== undefined}
            showValidity={false}
            onChange={(e) => {
              setName(e.target.value);
              setProblem(undefined);
            }}
          />
          <Button type="submit" variant="outline" iconLeft={<Save size={14} />} disabled={name.trim() === ""}>
            Save current rules
          </Button>
        </form>
        {profiles.length > 0 && (
          <div className="row">
            <Select
              className="grow"
              label="Profile"
              options={[
                { value: "", label: "Choose a profile" },
                ...profiles.map((p) => ({ value: p.name, label: `${p.name} (${p.rules.length} rules)` })),
              ]}
              value={chosen}
              onChange={setChosen}
            />
            <Button variant="outline" disabled={chosen === ""} onClick={() => setApplying(true)}>
              Apply…
            </Button>
            <Button
              variant="ghost"
              aria-label={`Delete profile ${chosen}`}
              iconLeft={<X size={14} />}
              disabled={chosen === ""}
              onClick={() => run(() => deleteNetworkProfile(chosen).then(() => setChosen("")).then(load))}
            />
          </div>
        )}
        <Confirm
          open={applying}
          title={`Apply profile ${chosen}?`}
          confirmLabel="Apply"
          onConfirm={() => {
            setApplying(false);
            run(() => applyNetworkProfile(cageId, chosen));
          }}
          onCancel={() => setApplying(false)}
        >
          <p>
            The default and the rules of the profile replace the ones of this cage. Rules with a credential stay.
            Offline and the service on this Mac stay.
          </p>
        </Confirm>
        <Accordion
          headingLevel={3}
          open={open}
          onOpenChange={setOpen}
          items={[
            {
              id: "json",
              title: "Effective policy as JSON (read-only)",
              content: <pre className="tool-output mono">{json}</pre>,
            },
          ]}
        />
      </CardBody>
    </Card>
  );
}
