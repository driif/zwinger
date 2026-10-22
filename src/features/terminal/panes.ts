import type { PaneNode } from "../../shared/api";

/** The place of a pane or split, as fractions of the layout. */
export interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** The deepest split that the backend saves. */
const DEPTH = 8;

/** The way from the root to a node: `a` or `b` at each split. */
export type Path = ("a" | "b")[];

export type Direction = "left" | "right" | "up" | "down";

const leaf = (terminal: string): PaneNode => ({ type: "leaf", terminal });

/** The terminal ids of the panes, in layout order. */
export function leaves(node: PaneNode | null): string[] {
  if (!node) return [];
  return node.type === "leaf" ? [node.terminal] : [...leaves(node.a), ...leaves(node.b)];
}

/** Each pane with its rect, and each split with its rect and path. */
export function place(node: PaneNode, rect: Rect = { left: 0, top: 0, width: 1, height: 1 }, path: Path = []) {
  const panes: { terminal: string; rect: Rect }[] = [];
  const splits: { path: Path; direction: "row" | "column"; ratio: number; rect: Rect }[] = [];
  if (node.type === "leaf") {
    panes.push({ terminal: node.terminal, rect });
    return { panes, splits };
  }
  const row = node.direction === "row";
  const a = row ? { ...rect, width: rect.width * node.ratio } : { ...rect, height: rect.height * node.ratio };
  const b = row
    ? { ...rect, left: rect.left + a.width, width: rect.width - a.width }
    : { ...rect, top: rect.top + a.height, height: rect.height - a.height };
  splits.push({ path, direction: node.direction, ratio: node.ratio, rect });
  for (const [side, part] of [["a", a], ["b", b]] as const) {
    const inner = place(node[side], part, [...path, side]);
    panes.push(...inner.panes);
    splits.push(...inner.splits);
  }
  return { panes, splits };
}

function map(node: PaneNode, change: (leaf: PaneNode & { type: "leaf" }) => PaneNode): PaneNode {
  return node.type === "leaf" ? change(node) : { ...node, a: map(node.a, change), b: map(node.b, change) };
}

/** Splits the pane of `target`; the new pane of `terminal` goes right (`row`) or down (`column`). No such pane: the whole layout splits. */
export function split(node: PaneNode | null, target: string | undefined, terminal: string, direction: "row" | "column") {
  const added = (part: PaneNode): PaneNode => ({ type: "split", direction, ratio: 0.5, a: part, b: leaf(terminal) });
  if (!node) return leaf(terminal);
  if (target === undefined || !leaves(node).includes(target)) return added(node);
  return map(node, (l) => (l.terminal === target ? added(l) : l));
}

/** The number of splits above the pane of `terminal`. */
export function depth(node: PaneNode | null, terminal: string): number {
  if (!node || node.type === "leaf") return 0;
  if (leaves(node.a).includes(terminal)) return 1 + depth(node.a, terminal);
  return 1 + depth(node.b, terminal);
}

/** A split of this pane is one level too deep to save. */
export const tooDeep = (node: PaneNode | null, terminal: string | undefined) =>
  terminal !== undefined && leaves(node).includes(terminal) && depth(node, terminal) >= DEPTH;

/** Removes the pane of `terminal`; its sibling takes the place of the split. `null` if no pane stays. */
export function remove(node: PaneNode | null, terminal: string): PaneNode | null {
  if (!node) return null;
  if (node.type === "leaf") return node.terminal === terminal ? null : node;
  const a = remove(node.a, terminal);
  const b = remove(node.b, terminal);
  return a && b ? { ...node, a, b } : (a ?? b);
}

/** Shows `terminal` in the pane of `target`. No layout or no such pane: `terminal` is the only pane. */
export function replace(node: PaneNode | null, target: string | undefined, terminal: string): PaneNode {
  if (!node || target === undefined || !leaves(node).includes(target)) return leaf(terminal);
  return map(node, (l) => (l.terminal === target ? leaf(terminal) : l));
}

export function setRatio(node: PaneNode, path: Path, ratio: number): PaneNode {
  if (node.type === "leaf") return node;
  if (path.length === 0) return { ...node, ratio };
  const [side, ...rest] = path;
  return { ...node, [side]: setRatio(node[side], rest, ratio) };
}

/** The nearest pane in `direction` from the pane of `from` that shares an edge span with it. */
export function neighbor(node: PaneNode, from: string, direction: Direction): string | undefined {
  const { panes } = place(node);
  const start = panes.find((p) => p.terminal === from)?.rect;
  if (!start) return;
  const eps = 1e-9;
  const horizontal = direction === "left" || direction === "right";
  const overlaps = (r: Rect) =>
    horizontal
      ? r.top < start.top + start.height - eps && r.top + r.height > start.top + eps
      : r.left < start.left + start.width - eps && r.left + r.width > start.left + eps;
  const gap = (r: Rect) => {
    switch (direction) {
      case "right":
        return r.left - (start.left + start.width);
      case "left":
        return start.left - (r.left + r.width);
      case "down":
        return r.top - (start.top + start.height);
      case "up":
        return start.top - (r.top + r.height);
    }
  };
  return panes
    .filter((p) => p.terminal !== from && overlaps(p.rect) && gap(p.rect) > -eps)
    .sort((x, y) => gap(x.rect) - gap(y.rect) || (horizontal ? x.rect.top - y.rect.top : x.rect.left - y.rect.left))[0]?.terminal;
}
