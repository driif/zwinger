/** A point in logical (CSS) pixels of the main window. */
export interface Point {
  x: number;
  y: number;
}

/** Where a folder from Finder would go. `element` is what shows the highlight. */
export interface FolderTarget {
  kind: "rail" | "map";
  element: HTMLElement;
  /** The workspace of the row under the point. Empty rail space has none. */
  workspace?: string;
}

/** The one hit test for folder drops, shared by the highlight and by the drop handlers. */
export function resolveFolderTarget({ x, y }: Point): FolderTarget | undefined {
  const under = document.elementFromPoint(x, y);
  if (!under) return;
  const rail = under.closest<HTMLElement>("[data-drop=rail]");
  if (rail) {
    // Only the nearest row counts: it names its workspace.
    const row = under.closest<HTMLElement>("[role=treeitem]");
    if (!row) return { kind: "rail", element: rail };
    const workspace = row.dataset.workspaceDrop;
    return workspace ? { kind: "rail", element: row, workspace } : undefined;
  }
  // Child nodes cover the boundary, so the point is tested against its rect.
  const boundary = under
    .closest("[data-drop=map]")
    ?.querySelector<HTMLElement>(".react-flow__node[data-id=boundary]");
  if (!boundary) return;
  const box = boundary.getBoundingClientRect();
  if (x >= box.left && x <= box.right && y >= box.top && y <= box.bottom) return { kind: "map", element: boundary };
}

/**
 * Marks the target of the Finder drag with `data-folder-drop`. A DOM attribute
 * instead of React state: no re-render per `over` event, and re-renders keep it.
 */
function tracker() {
  let point: Point | undefined;
  let shown = false;
  let marked: HTMLElement | undefined;
  let listening = false;
  const refresh = () => {
    const next = shown && point ? resolveFolderTarget(point)?.element : undefined;
    if (next === marked) return;
    marked?.removeAttribute("data-folder-drop");
    next?.setAttribute("data-folder-drop", "");
    marked = next;
  };
  // Tauri sends no `over` event while the pointer does not move, but a scroll or the end of
  // a tab transition can move a target under it. `scroll` does not bubble, so listen in capture.
  const listen = (on: boolean) => {
    if (listening === on) return;
    listening = on;
    const method = on ? "addEventListener" : "removeEventListener";
    document[method]("scroll", refresh, true);
    document[method]("transitionend", refresh, true);
  };
  return {
    /** A new drag. The mark waits for `show`. */
    enter(at: Point) {
      listen(false);
      point = at;
      shown = false;
      refresh();
    },
    over(at: Point) {
      point = at;
      refresh();
    },
    /** The entered path is a folder that can become a project or a mount. */
    show() {
      shown = true;
      listen(true);
      refresh();
    },
    /** For leave and drop. */
    clear() {
      listen(false);
      point = undefined;
      shown = false;
      refresh();
    },
    /** Tests the last point again, after the view under it changed. */
    refresh,
  };
}

export const folderDrop = tracker();
