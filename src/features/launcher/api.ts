import { call } from "../../shared/api";

/** An IDE on this Mac. `path` is its app bundle. */
export interface Editor {
  id: string;
  name: string;
  path: string;
  /** The launchers offer it. */
  enabled: boolean;
}

/** The detected editors in a fixed order, and the default, which is on. `null`: no editor is on. */
export interface Editors {
  editors: Editor[];
  default: string | null;
}

/** What Open in editor did. A fall back opened the default editor or the session folder instead. */
export interface Opened {
  editor: string;
  folder: string;
  editorFellBack: boolean;
  folderFellBack: boolean;
}

/** Looks in /Applications, ~/Applications and /Applications/Setapp again. */
export const detectEditors = () => call<Editors>("detect_editors");

/** Opens the session folder, or the current folder of the terminal. `null` editor: the default. */
export const openInEditor = (sessionId: string, editorId: string | null, terminalId: string | null) =>
  call<Opened>("open_in_editor", { sessionId, editorId, terminalId });

/** Shows the session folder in Finder. */
export const revealSession = (sessionId: string) => call<void>("reveal_session", { sessionId });
