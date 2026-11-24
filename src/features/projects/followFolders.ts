import type { FolderList, Unreadable } from "../settings/api.ts";

/** The folder commands, passed in so that a test can replace them. */
export interface FolderApi {
  watch: (sessionId: string, onFolders: (folders: FolderList) => void) => Promise<number>;
  unwatch: (token: number) => unknown;
  rescan: (token: number) => Promise<unknown>;
  list: (sessionId: string) => Promise<FolderList>;
}

/** Why the project folder cannot be read. The folder path follows it. */
export const UNREADABLE: Record<Unreadable, string> = {
  missing: "The project folder does not exist",
  notAFolder: "The project path is not a folder",
  permissionDenied: "No permission to read the project folder",
  other: "The project folder cannot be read",
};

/** The key of a displayed list: a list of one folder must not show for an other folder. */
export const folderKey = (session: { id: string; folder: string }) => `${session.id}\n${session.folder}`;

/**
 * Follows the folders of a project from the watch. `reload` tells the watch
 * to read again. If the watch fails or ends, each `reload` reads the folders
 * directly. A failed direct read is `"other"`. Nothing arrives after `close`.
 */
export function followFolders(
  sessionId: string,
  onFolders: (folders: FolderList) => void,
  api: FolderApi,
) {
  let closed = false;
  let token: number | undefined;
  let fallback = false;
  let reads = 0;
  const read = () => {
    const current = ++reads;
    // Only the newest read sets the list, so an older and slower read cannot replace it.
    const newest = () => !closed && current === reads;
    api.list(sessionId).then(
      (folders) => newest() && onFolders(folders),
      () => newest() && onFolders("other"),
    );
  };
  const fallBack = () => {
    fallback = true;
    token = undefined;
    read();
  };
  const reload = () => {
    if (closed) return;
    if (fallback) read();
    else if (token !== undefined) api.rescan(token).catch(() => !closed && !fallback && fallBack());
  };
  api.watch(sessionId, (folders) => !closed && !fallback && onFolders(folders)).then(
    (t) => {
      if (closed) api.unwatch(t);
      else token = t;
    },
    () => !closed && fallBack(),
  );
  return {
    reload,
    close: () => {
      closed = true;
      if (token !== undefined) api.unwatch(token);
    },
  };
}
