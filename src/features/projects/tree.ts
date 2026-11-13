import type { Workspace } from "../../shared/api";

/** The workspaces with `session` moved to workspace `to`: before `before`, else at the end. */
export function moveSession(workspaces: Workspace[], session: string, to: string, before?: string): Workspace[] {
  return workspaces.map((w) => {
    const sessions = w.sessions.filter((s) => s !== session);
    if (w.id === to) {
      const index = before ? sessions.indexOf(before) : -1;
      sessions.splice(index < 0 ? sessions.length : index, 0, session);
    }
    return { ...w, sessions };
  });
}

/**
 * The workspaces with `session` one row up (-1) or down (1). Over the border of its workspace it goes to the end of
 * the workspace above or to the start of the workspace below. At the first or last row nothing changes.
 */
export function stepSession(workspaces: Workspace[], session: string, dir: 1 | -1): Workspace[] {
  const from = workspaces.findIndex((w) => w.sessions.includes(session));
  if (from < 0) return workspaces;
  const sessions = workspaces[from].sessions;
  const index = sessions.indexOf(session);
  const to = index + dir;
  if (to >= 0 && to < sessions.length) {
    const next = [...sessions];
    [next[index], next[to]] = [next[to], next[index]];
    return workspaces.map((w, i) => (i === from ? { ...w, sessions: next } : w));
  }
  const target = workspaces[from + dir];
  if (!target) return workspaces;
  return moveSession(workspaces, session, target.id, dir === 1 ? target.sessions[0] : undefined);
}

/** The workspaces with `workspace` one place up (-1) or down (1). At the first or last place nothing changes. */
export function stepWorkspace(workspaces: Workspace[], workspace: string, dir: 1 | -1): Workspace[] {
  const index = workspaces.findIndex((w) => w.id === workspace);
  const to = index + dir;
  if (index < 0 || to < 0 || to >= workspaces.length) return workspaces;
  const next = [...workspaces];
  [next[index], next[to]] = [next[to], next[index]];
  return next;
}
