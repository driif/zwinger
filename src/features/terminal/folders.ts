import type { FolderReport, TerminalFolder, TerminalView } from "../../shared/api";
import { tildePath, truncateMiddle } from "../../shared/paths.ts";

/** The longest folder text in a pane header. */
const FOLDER_MAX = 40;

/** The folder as a pane header shows it: a Mac folder with `~`, middle-truncated. */
export function folderLabel(folder: TerminalFolder, home: string | undefined) {
  return truncateMiddle(folder.vm ? folder.path : tildePath(folder.path, home), FOLDER_MAX);
}

/**
 * The command that opens the folder of a terminal in `editor`. It names the project folder when that opens instead.
 * `folder` null: the folder of the terminal is unknown.
 */
export function editorLabel(editor: string, folder: TerminalFolder | null, projectFolder: string, home: string | undefined) {
  if (folder?.opens) return `Open ${tildePath(folder.opens, home)} in ${editor}`;
  const fallback = `Open project folder ${tildePath(projectFolder, home)} in ${editor}`;
  if (!folder) return `${fallback} (the folder of the terminal is unknown)`;
  return folder.vm
    ? `${fallback} (VM folder ${folder.path} is not on the Mac)`
    : `${fallback} (${tildePath(folder.path, home)} is gone)`;
}

/** The newer of two reports of one process. On a tie, `kept` stays: the backend gives one folder per seq. */
export const mergeFolder = (kept: FolderReport | undefined, next: FolderReport) =>
  kept && kept.seq >= next.seq ? kept : next;

/**
 * The folder that a process shows: the newer of its listing and of `kept`, the newest event or batch reply.
 * An exited process, also one that only `exited` knows of, shows none.
 */
export const liveFolder = (process: TerminalView, kept: FolderReport | undefined, exited: ReadonlySet<string>) =>
  process.exitCode !== null || exited.has(process.id)
    ? null
    : mergeFolder(kept, { seq: process.folderSeq, folder: process.folder }).folder;
