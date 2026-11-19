import { useEffect, useRef, useState } from "react";
import { SessionView } from "../../shared/api";
import { listFolders, rescanFolders, unwatchFolders, watchFolders } from "../settings/api";
import { folderKey, followFolders } from "./followFolders";

const API = { watch: watchFolders, unwatch: unwatchFolders, rescan: rescanFolders, list: listFolders };

/**
 * The top-level folders of the project, live. `undefined` until they are
 * listed and while the project folder cannot be read. `reload` reads them again.
 */
export function useProjectFolders(session: SessionView) {
  const [listed, setListed] = useState<{ key: string; folders: string[] | undefined }>();
  const reload = useRef(() => {});
  const key = folderKey(session);
  useEffect(() => {
    const follow = followFolders(session.id, (folders) => setListed({ key, folders }), API);
    reload.current = follow.reload;
    window.addEventListener("focus", follow.reload);
    return () => {
      window.removeEventListener("focus", follow.reload);
      follow.close();
    };
  }, [key, session.id]);
  return {
    folders: listed?.key === key ? listed.folders : undefined,
    reload: () => reload.current(),
  };
}
