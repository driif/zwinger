import { Button, Card } from "@driif/motiv";
import { Download } from "lucide-react";
import { useState } from "react";
import { installUpdate, UpdateInfo } from "./api";
import { errorCode, errorMessage } from "./errorMessage";

interface Props {
  update: UpdateInfo;
  /** No `onLater`: the banner cannot be hidden, as in Settings. */
  onLater?: () => void;
}

/** Installs the update and restarts. A running shield is stopped only after the user agrees. */
export function UpdateBanner({ update, onLater }: Props) {
  const [ask, setAsk] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  function install(stopShields: boolean) {
    setBusy(true);
    setError(undefined);
    // On success the app restarts, so only a failure comes back.
    installUpdate(stopShields).catch((e) => {
      if (!stopShields && errorCode(e) === "ShieldsRunning") setAsk(true);
      else setError(errorMessage(e));
      setBusy(false);
    });
  }

  return (
    <Card variant="outlined" padding="sm" className="banner row">
      <Download size={16} className="ok" />
      <span className="grow">
        {ask
          ? "Shields are running. The restart ends their VMs and the work of their agents."
          : `Update ${update.version} is available.`}
        {error && <span className="danger"> {error}</span>}
      </span>
      {ask ? (
        <>
          <Button size="sm" variant="ghost" disabled={busy} onClick={() => setAsk(false)}>
            Cancel
          </Button>
          <Button size="sm" variant="danger" loading={busy} onClick={() => install(true)}>
            Stop shields and restart
          </Button>
        </>
      ) : (
        <>
          {onLater && (
            <Button size="sm" variant="ghost" disabled={busy} onClick={onLater}>
              Later
            </Button>
          )}
          <Button size="sm" variant="outline" loading={busy} onClick={() => install(false)}>
            Restart to update
          </Button>
        </>
      )}
    </Card>
  );
}
