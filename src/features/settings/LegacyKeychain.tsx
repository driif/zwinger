import { Button, Card, CardBody, CardHeader, LiquidSwitch } from "@driif/motiv";
import { Download, Search, Trash2, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";
import {
  cleanLegacyKeychain,
  importLegacyKeychain,
  legacyKeychainStatus,
  LegacyOutcome,
  LegacyReason,
  LegacyReport,
  LegacyRow,
  LegacyStatus,
  scanLegacyKeychain,
} from "./api";
import { errorMessage } from "../../shared/errorMessage";

const OUTCOME: Record<LegacyOutcome, string> = {
  importable: "Can be imported",
  skipped: "Not imported",
  retiredCa: "Old proxy CA key. It is never imported",
  imported: "Imported",
  same: "Already in the vault",
  conflict: "Not imported: the vault has a different value. Both values stay",
  denied: "macOS did not give access. The item stays",
  missing: "Not in the Keychain",
  failed: "Failed. The item stays",
  cleaned: "Removed from the Keychain",
  kept: "Kept in the Keychain",
};

const REASON: Record<LegacyReason, string> = {
  format: "the name is not <project>/<NAME>",
  noCage: "no cage has this project id",
  noCredential: "the cage has no own credential with this name",
  library: "the cage takes this name from the credential library",
  hostLogin: "this name is a host login, and the vault does not keep it",
  empty: "the value is empty",
  notText: "the value is not text",
  vault: "the vault could not be read, written or saved to disk",
  keychain: "the Keychain failed",
  changed: "the item, the vault value or the config changed after the import. Run Import again",
  notImported: "the last import does not allow its removal. Run Import again",
  notConfirmed: "select it under Other old items to remove it",
  ambiguous: "more than one Keychain item has this name. zwinger does not read or remove them",
};

function describe(row: LegacyRow) {
  const reason = row.reason ? `: ${REASON[row.reason]}` : "";
  const code = row.code === undefined ? "" : ` (OSStatus ${row.code})`;
  return `${OUTCOME[row.outcome]}${reason}${code}.`;
}

/** An import result that a removal can delete with no vault copy, after the user confirms it. */
const RISKY: LegacyOutcome[] = ["skipped", "conflict", "retiredCa"];
/** A cleanup result after which the item is not offered again in this visit. */
const ENDED_OUTCOMES: LegacyOutcome[] = ["cleaned", "missing"];
const ENDED_REASONS: LegacyReason[] = ["changed", "notImported", "ambiguous"];

const ended = (row?: LegacyRow) =>
  !!row && (ENDED_OUTCOMES.includes(row.outcome) || (!!row.reason && ENDED_REASONS.includes(row.reason)));

/**
 * The opt-in import of the credentials that older builds kept in the macOS Keychain, and the cleanup of those items.
 * Nothing touches the Keychain until a click. The UI gets account names and states, never values. Only a saved import
 * receipt allows a cleanup: a scan never does.
 */
export function LegacyKeychain() {
  const [status, setStatus] = useState<LegacyStatus>();
  /** The last scan. It allows no cleanup. */
  const [scanned, setScanned] = useState<LegacyRow[]>();
  /** The last import of this visit. Only its reviewable rows can be confirmed. */
  const [imported, setImported] = useState<LegacyReport>();
  /** The cleanup results of this visit, by account, merged over each cleanup. */
  const [results, setResults] = useState<Record<string, LegacyRow>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const [asking, setAsking] = useState<"safe" | "risky">();
  const [picked, setPicked] = useState<string[]>([]);

  const loadStatus = () =>
    legacyKeychainStatus().then(setStatus, (e) => {
      setStatus(undefined);
      setError(errorMessage(e));
    });

  useEffect(() => {
    loadStatus();
  }, []);

  // A cleanup needs the saved receipt. An import of this visit that was not saved allows nothing.
  const allowed = !!status?.ran && (!imported || imported.saved);
  const safe = allowed ? (status?.pending ?? []).filter((a) => !ended(results[a])) : [];
  const risky =
    allowed && imported
      ? imported.rows.filter((r) => RISKY.includes(r.outcome) && !ended(results[r.account]))
      : [];
  /** Only a selection that is still offered. */
  const chosen = picked.filter((a) => risky.some((r) => r.account === a));

  function step<T>(action: () => Promise<T>, done: (result: T) => void) {
    setBusy(true);
    setError(undefined);
    setNotice(undefined);
    action()
      .then(done, (e) => setError(errorMessage(e)))
      .then(loadStatus)
      .finally(() => {
        setBusy(false);
        setAsking(undefined);
      });
  }

  function scan() {
    step(scanLegacyKeychain, (rows) => {
      setScanned(rows);
      setImported(undefined);
      setResults({});
      setPicked([]);
    });
  }

  function runImport() {
    step(importLegacyKeychain, (report) => {
      setImported(report);
      setScanned(undefined);
      setResults({});
      setPicked([]);
      if (!report.saved) setNotice("The import receipt was not saved. All Keychain items stayed in place. Run Import from Keychain again.");
    });
  }

  function clean(accounts: string[], confirmed: string[]) {
    step(
      () => cleanLegacyKeychain(accounts, confirmed, confirmed.length > 0 ? imported?.generation : undefined),
      (report) => {
        setResults((prior) => ({ ...prior, ...Object.fromEntries(report.rows.map((r) => [r.account, r])) }));
        setPicked([]);
        if (!report.saved) {
          const removed = report.rows.filter((row) => row.outcome === "cleaned").length;
          setNotice(`${removed} ${removed === 1 ? "item was" : "items were"} removed in this cleanup. Its receipt was not saved. Run Import from Keychain again to review the remaining items.`);
        }
      },
    );
  }

  const base = imported?.rows ?? scanned;
  const shown = [
    ...(base ?? []).map((row) => results[row.account] ?? row),
    ...Object.values(results).filter((row) => !base?.some((b) => b.account === row.account)),
  ];

  return (
    <Card variant="outlined" padding="md">
      <CardHeader
        title="Credentials in the macOS Keychain"
        titleAs="h2"
        description="Versions before the vault file kept credentials in the macOS Keychain. Import copies each value to the cage that still has a credential with that name. It never replaces a value in the vault and never removes a Keychain item. zwinger does not touch the Keychain until you click."
        actions={
          <div className="row">
            <Button size="sm" variant="outline" iconLeft={<Search size={14} />} disabled={busy} onClick={scan}>
              Check Keychain
            </Button>
            <Button size="sm" variant="outline" iconLeft={<Download size={14} />} loading={busy} onClick={runImport}>
              Import from Keychain
            </Button>
          </div>
        }
      />
      <CardBody className="stack">
        <p className="muted">
          Check Keychain lists the old items. It reads no value, changes nothing and does not allow a removal. Import
          reads the values and saves a receipt: macOS can ask for your password or for Allow one time for each item. If
          you cancel a prompt, that item stays and the others go on. Only items from a saved import can be removed.
        </p>
        {status?.ran && (
          <p className="muted">
            Last import: {status.pending.length} imported {status.pending.length === 1 ? "item is" : "items are"} still
            in the Keychain, {status.cleaned.length} removed.
          </p>
        )}
        {error && (
          <p className="danger row">
            <TriangleAlert size={14} />
            <span className="grow">{error}</span>
          </p>
        )}
        {notice && <p className="muted">{notice}</p>}
        {base?.length === 0 && shown.length === 0 && (
          <p className="muted">The Keychain has no items of older zwinger versions.</p>
        )}
        {shown.length > 0 && (
          <ul className="list">
            {shown.map((row) => (
              <li key={row.account} className="row">
                <span className="mono">{row.account}</span>
                <span className="grow muted">{describe(row)}</span>
              </li>
            ))}
          </ul>
        )}
        {safe.length > 0 && (
          <div className="stack">
            <h3>Remove imported items</h3>
            <div className="row">
              <span className="grow muted">
                {safe.length} imported {safe.length === 1 ? "item" : "items"} can be removed from the Keychain.
              </span>
              {asking !== "safe" && (
                <Button size="sm" variant="outline" iconLeft={<Trash2 size={14} />} disabled={busy} onClick={() => setAsking("safe")}>
                  Remove from Keychain
                </Button>
              )}
            </div>
            {asking === "safe" && (
              <div className="row">
                <span className="grow muted">
                  zwinger reads each item again. It removes the item only if the item did not change after the import and
                  the vault holds the same value. Other items stay. macOS can ask again for each item.
                </span>
                <Button size="sm" variant="ghost" disabled={busy} onClick={() => setAsking(undefined)}>
                  Cancel
                </Button>
                <Button size="sm" variant="danger" loading={busy} onClick={() => clean(safe, [])}>
                  Remove
                </Button>
              </div>
            )}
          </div>
        )}
        {risky.length > 0 && (
          <div className="stack">
            <h3>Other old items</h3>
            <p className="muted">
              The vault has no copy of these values, or a different one. A removal deletes the only copy in the Keychain,
              and it cannot be undone. Select each item that you do not need. An item that changed after the import
              stays.
            </p>
            <ul className="list">
              {risky.map((row) => (
                <li key={row.account} className="row">
                  <LiquidSwitch
                    label={`Remove ${row.account}`}
                    checked={chosen.includes(row.account)}
                    onChange={(on) =>
                      setPicked(on ? [...chosen, row.account] : chosen.filter((a) => a !== row.account))
                    }
                  />
                  <span className="grow muted">{describe(row)}</span>
                </li>
              ))}
            </ul>
            <div className="row">
              <span className="grow" />
              {asking !== "risky" && (
                <Button
                  size="sm"
                  variant="outline"
                  iconLeft={<Trash2 size={14} />}
                  disabled={busy || chosen.length === 0}
                  onClick={() => setAsking("risky")}
                >
                  Remove selected
                </Button>
              )}
            </div>
            {asking === "risky" && chosen.length > 0 && (
              <div className="row">
                <span className="grow danger">
                  Delete {chosen.length} {chosen.length === 1 ? "item" : "items"} from the Keychain: {chosen.join(", ")}.
                  zwinger cannot get these values back.
                </span>
                <Button size="sm" variant="ghost" disabled={busy} onClick={() => setAsking(undefined)}>
                  Cancel
                </Button>
                <Button size="sm" variant="danger" loading={busy} onClick={() => clean([], chosen)}>
                  Delete
                </Button>
              </div>
            )}
          </div>
        )}
        <p className="muted">
          A kept item stays in the Keychain. To use its value, open Keychain Access, search for zwinger, open the item
          and select Show password. Then enter the value in the cage or in the credential library.
        </p>
      </CardBody>
    </Card>
  );
}
