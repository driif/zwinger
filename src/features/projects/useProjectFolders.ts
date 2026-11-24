import { useEffect, useRef, useState } from "react";
import { SessionView } from "../../shared/api";
import { FolderList, listFolders, rescanFolders, unwatchFolders, watchFolders } from "../settings/api";
import { folderKey, followFolders } from "./followFolders";

const API = { watch: watchFolders, unwatch: unwatchFolders, rescan: rescanFolders, list: listFolders };

/**
 * The top-level folders of the project, live. `folders` is `undefined` until
 * they are listed and while the project folder cannot be read; `unreadable`
 * is the reason then. `reload` reads them again.
 */
export function useProjectFolders(session: SessionView) {
  const [listed, setListed] = useState<{ key: string; folders: FolderList }>();
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
  const current = listed?.key === key ? listed.folders : undefined;
  return {
    folders: Array.isArray(current) ? current : undefined,
    unreadable: Array.isArray(current) ? undefined : current,
    reload: () => reload.current(),
  };
}
