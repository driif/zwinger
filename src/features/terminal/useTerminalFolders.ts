import { useEffect, useRef, useState } from "react";
import type { FolderReport, SessionView, TerminalView } from "../../shared/api";
import { onTerminalFolder } from "../projects/api";
import { terminalFolders } from "./api";
import { liveFolder, mergeFolder } from "./folders";

/** How often the folders of the shown processes are read again, for a `cd` that writes no output. */
const FOLDER_POLL_MS = 2000;

/**
 * The live folder of each process. It keeps the newest report per process from the listing, the `terminal-folder`
 * event and the batch call, which reads the processes of `shown` again and again. `hide` removes the folder of a
 * process at its exit, before a listing shows the exit.
 */
export function useTerminalFolders(sessions: SessionView[], shown: string[]) {
  const [reports, setReports] = useState(new Map<string, FolderReport>());
  /** The processes whose exit came before the listing that removes them. */
  const [exited, setExited] = useState(new Set<string>());
  /** The last batch call. A new one waits for it, so at most one is in flight. */
  const inFlight = useRef<Promise<void>>(Promise.resolve());

  const keep = (found: Record<string, FolderReport>) =>
    setReports((current) => {
      let next = current;
      for (const [id, report] of Object.entries(found)) {
        const kept = current.get(id);
        if (mergeFolder(kept, report) !== kept) next = new Map(next).set(id, report);
      }
      return next;
    });

  useEffect(() => {
    const stop = onTerminalFolder((id, report) => keep({ [id]: report }));
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  const ids = shown.join(" ");
  useEffect(() => {
    if (!ids) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout>;
    const poll = () => {
      inFlight.current = inFlight.current
        .then(() => live && terminalFolders(ids.split(" ")).then((found) => live && keep(found), () => {}))
        .then(() => {
          if (live) timer = setTimeout(poll, FOLDER_POLL_MS);
        });
    };
    poll();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [ids]);

  // A process that the listing no longer has never comes back: its id is new for each process.
  const listed = sessions.flatMap((s) => s.processes.map((p) => p.id)).join(" ");
  useEffect(() => {
    const known = new Set(listed.split(" "));
    setReports((current) => {
      const gone = [...current.keys()].filter((id) => !known.has(id));
      if (gone.length === 0) return current;
      const next = new Map(current);
      for (const id of gone) next.delete(id);
      return next;
    });
    setExited((current) => {
      const gone = [...current].filter((id) => !known.has(id));
      if (gone.length === 0) return current;
      const next = new Set(current);
      for (const id of gone) next.delete(id);
      return next;
    });
  }, [listed]);

  return {
    folderOf: (process: TerminalView) => liveFolder(process, reports.get(process.id), exited),
    hide: (processId: string) => setExited((current) => (current.has(processId) ? current : new Set(current).add(processId))),
  };
}
