import { Button, Card, FloatingField, IconButton } from "@driif/motiv";
import {
  Active,
  Announcements,
  closestCenter,
  CollisionDetection,
  DndContext,
  DragEndEvent,
  DragMoveEvent,
  KeyboardSensor,
  Over,
  PointerSensor,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { arrayMove, SortableContext, sortableKeyboardCoordinates, useSortable, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import {
  AppWindow,
  Box,
  ChevronDown,
  ChevronRight,
  Ellipsis,
  History,
  Laptop,
  ListChecks,
  Pencil,
  Play,
  Plus,
  RotateCw,
  Settings,
  Square,
  Trash2,
  X,
} from "lucide-react";
import { FormEvent, KeyboardEvent, ReactNode, useEffect, useRef, useState } from "react";
import {
  removeApp,
  reorderApps,
  restartApp,
  startApp,
  stopApp,
} from "../apps/api";
import { reorderShields } from "./api";
import { isLive, removeTask, RunSummary, startTaskRun } from "../tasks/api";
import {
  AGENT_NAMES,
  AgentKind,
  SessionKind,
  ShieldView,
} from "../../shared/api";
import type { DroppedFolder, Intent, Pane, Run, View } from "../../App";
import { AGENT_ICONS } from "../tasks/roomParts";
import "./rail.css";

const COLLAPSED_KEY = "zwinger.tree.collapsed";
const FORBIDDEN = "Not allowed: a shield is a security boundary. Items stay in their shield.";

type Group = "terminals" | "apps" | "tasks";
const GROUPS: { group: Group; label: string }[] = [
  { group: "terminals", label: "Terminals" },
  { group: "apps", label: "Apps" },
  { group: "tasks", label: "Tasks" },
];

const AGENT_ORDER: AgentKind[] = ["claude", "codex", "pi", "gemini"];

/**
 * The agents of the shield, host agents first. A running shield has pi and its mounted runtimes. A stopped shield
 * lists the agents that a start can mount; Gemini CLI runs only from this Mac.
 */
export function agentsOf(shield: ShieldView): AgentKind[] {
  const host = shield.hostAgents.map((a) => a.kind);
  const available: AgentKind[] =
    shield.status.state === "running" ? ["pi", ...shield.runtimes] : ["claude", "codex", "pi", ...host.filter((k) => k === "gemini")];
  return [...host, ...AGENT_ORDER.filter((k) => !host.includes(k))].filter((k) => available.includes(k));
}

const kindName = (kind: SessionKind) =>
  kind.type === "host" ? "Mac shell" : kind.type === "shell" ? "VM shell" : AGENT_NAMES[kind.agent];

/** Where a terminal runs: on this Mac, or in the shield's VM. An agent always runs in the VM. */
function KindIcon({ kind }: { kind: SessionKind }) {
  if (kind.type === "agent") {
    const Icon = AGENT_ICONS[kind.agent];
    return <Icon size={14} aria-label="In the VM" />;
  }
  return kind.type === "host" ? <Laptop size={14} aria-label="On this Mac" /> : <Box size={14} aria-label="In the VM" />;
}

/** The name of a terminal: its kind, and its number among the shield's terminals of that kind. */
export function paneName(pane: Pane, panes: Pane[]) {
  const same = panes.filter((p) => p.shieldId === pane.shieldId && kindName(p.kind) === kindName(pane.kind));
  return `${kindName(pane.kind)} ${same.indexOf(pane) + 1}`;
}

/** The badge of a task: its latest run. */
function runBadge(run: RunSummary | undefined) {
  if (!run) return undefined;
  if (run.state === "running") return run.stage ?? "running";
  if (run.state === "done") return run.ship ? "ship" : "no ship";
  return run.state;
}

/** What dnd-kit carries per sortable row. Only rows of the same container can swap. */
interface DragData {
  container: string;
  shieldId: string;
  entity: string;
  label: string;
}

const dataOf = (item: Active | Over) => item.data.current as DragData;

/** A row action. `confirm` asks first; `rename` asks for a new name first. */
type MenuAction = { label: string; icon: ReactNode; disabled?: boolean } & (
  | { run: () => void; confirm?: string }
  | { rename: string; onRename: (name: string) => void }
);

interface Leaf {
  entity: string;
  label: ReactNode;
  name: string;
  title?: string;
  icon: ReactNode;
  view: View;
  menu: MenuAction[];
}

/** The actions of a row. Arrow keys move between them; Escape and a click outside close it. */
function RowMenu({ label, actions, onClose }: { label: string; actions: MenuAction[]; onClose: (refocus: boolean) => void }) {
  const card = useRef<HTMLElement>(null);
  const [step, setStep] = useState<MenuAction>();
  const [name, setName] = useState("");
  const close = useRef(onClose);
  close.current = onClose;

  useEffect(() => {
    card.current?.querySelector<HTMLElement>("input, button:not(:disabled)")?.focus();
  }, [step]);

  useEffect(() => {
    const outside = (event: PointerEvent) => !card.current?.contains(event.target as Node) && close.current(false);
    document.addEventListener("pointerdown", outside);
    return () => document.removeEventListener("pointerdown", outside);
  }, []);

  function onKeyDown(event: KeyboardEvent) {
    // The tree and dnd-kit do not see the keys of the menu.
    event.stopPropagation();
    if (event.key === "Escape") {
      event.preventDefault();
      return onClose(true);
    }
    if (step || (event.key !== "ArrowDown" && event.key !== "ArrowUp")) return;
    event.preventDefault();
    const items = [...(card.current?.querySelectorAll<HTMLElement>("[role=menuitem]:not(:disabled)") ?? [])];
    const index = items.indexOf(document.activeElement as HTMLElement);
    items[(index + (event.key === "ArrowDown" ? 1 : -1) + items.length) % items.length]?.focus();
  }

  function choose(action: MenuAction) {
    if ("rename" in action) {
      setName(action.rename);
      return setStep(action);
    }
    if (action.confirm) return setStep(action);
    onClose(true);
    action.run();
  }

  function rename(event: FormEvent) {
    event.preventDefault();
    if (!step || !("rename" in step) || !name.trim()) return;
    onClose(true);
    step.onRename(name.trim());
  }

  return (
    <Card ref={card} padding="sm" className="tree-menu" onKeyDown={onKeyDown}>
      {!step ? (
        <div role="menu" aria-label={label} className="stack">
          {actions.map((action) => (
            <Button
              key={action.label}
              role="menuitem"
              size="sm"
              variant="ghost"
              iconLeft={action.icon}
              disabled={action.disabled}
              onClick={() => choose(action)}
            >
              {action.label}
            </Button>
          ))}
        </div>
      ) : "rename" in step ? (
        <form className="stack" onSubmit={rename}>
          <FloatingField label="Name" value={name} maxLength={200} showValidity={false} onChange={(e) => setName(e.target.value)} />
          <div className="row end">
            <Button size="sm" variant="ghost" onClick={() => onClose(true)}>
              Cancel
            </Button>
            <Button size="sm" type="submit" disabled={!name.trim()}>
              Rename
            </Button>
          </div>
        </form>
      ) : (
        <div className="stack" role="alertdialog" aria-label={step.label}>
          <p>{step.confirm}</p>
          <div className="row end">
            <Button size="sm" variant="ghost" onClick={() => onClose(true)}>
              Cancel
            </Button>
            <Button
              size="sm"
              variant="danger"
              onClick={() => {
                onClose(true);
                step.run();
              }}
            >
              {step.label}
            </Button>
          </div>
        </div>
      )}
    </Card>
  );
}

interface RowProps {
  id: string;
  level: number;
  drag?: DragData;
  expanded?: boolean;
  selected: boolean;
  tabStop: boolean;
  className?: string;
  shieldDrop?: string;
  row: ReactNode;
  onOpen: () => void;
  /** Opens the row menu on a right-click. */
  onMenu?: () => void;
  children?: ReactNode;
}

/** A treeitem. A sortable one is dragged by its own row only, so a drag in a child list does not move the parent. */
function TreeRow({ id, level, drag, expanded, selected, tabStop, className, shieldDrop, row, onOpen, onMenu, children }: RowProps) {
  const sortable = useSortable({ id, data: drag, disabled: !drag });
  return (
    <li
      ref={sortable.setNodeRef}
      role="treeitem"
      aria-level={level}
      aria-expanded={expanded}
      aria-selected={selected}
      aria-describedby={drag && sortable.attributes["aria-describedby"]}
      tabIndex={tabStop ? 0 : -1}
      data-tree-id={id}
      data-shield-drop={shieldDrop}
      className={`tree-item ${className ?? ""} ${sortable.isDragging ? "dragging" : ""}`}
      style={{ transform: CSS.Translate.toString(sortable.transform), transition: sortable.transition }}
      onKeyDown={(event) => event.target === event.currentTarget && sortable.listeners?.onKeyDown?.(event)}
    >
      <div
        ref={sortable.setActivatorNodeRef}
        className={`tree-row level-${level}`}
        onPointerDown={(event) => sortable.listeners?.onPointerDown?.(event)}
        onClick={onOpen}
        onContextMenu={(event) => {
          if (!onMenu) return;
          event.preventDefault();
          onMenu();
        }}
      >
        {row}
      </div>
      {children}
    </li>
  );
}

interface Props {
  shields: ShieldView[];
  selectedId?: string;
  view: View;
  /** Per shield, the blocked events that the Network tab has not shown yet. */
  blocked: Record<string, number>;
  /** Per shield, the runs of its tasks. */
  runs: Record<string, RunSummary[]>;
  panes: Pane[];
  run: Run;
  onSelect: (id: string) => void;
  onOpen: (shieldId: string, view: View) => void;
  onIntent: (shieldId: string, kind: Intent["kind"], appId?: string) => void;
  onRunsChange: (shieldId: string) => void;
  onToggle: (shield: ShieldView) => void;
  onNewSession: (shieldId: string, kind: SessionKind) => void;
  onClosePane: (key: string) => void;
  /** Opens a dormant or exited terminal again. */
  onReopen: (pane: Pane) => void;
  /** Saves the order of all terminals of the shield. */
  onMoveTerminals: (shieldId: string, ids: string[]) => void;
  onNew: () => void;
  /** Opens the app settings. */
  onSettings: () => void;
  /** A folder dropped from Finder on empty rail space. */
  dropFolder?: DroppedFolder;
  onDropDone: (create: boolean) => void;
  children: ReactNode;
}

/** The shields as a tree: shield → Terminals / Apps / Tasks → items. */
export function ShieldRail({
  shields,
  selectedId,
  view,
  blocked,
  runs,
  panes,
  run,
  onSelect,
  onOpen,
  onIntent,
  onRunsChange,
  onToggle,
  onNewSession,
  onClosePane,
  onReopen,
  onMoveTerminals,
  onNew,
  onSettings,
  dropFolder,
  onDropDone,
  children,
}: Props) {
  const tree = useRef<HTMLUListElement>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(
    () => new Set(JSON.parse(localStorage.getItem(COLLAPSED_KEY) ?? "[]") as string[]),
  );
  const [focused, setFocused] = useState<string>();
  const [dragging, setDragging] = useState(false);
  /** The other shield that a dragged item is over. */
  const [forbidden, setForbidden] = useState<string>();
  const [announcement, setAnnouncement] = useState("");
  /** The row whose menu is open. */
  const [menu, setMenu] = useState<string>();
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    // Enter opens a row, so only Space picks up and drops.
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
      keyboardCodes: { start: ["Space"], cancel: ["Escape"], end: ["Space"] },
    }),
  );

  useEffect(() => {
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...collapsed]));
  }, [collapsed]);

  const toggle = (id: string) =>
    setCollapsed((current) => {
      const next = new Set(current);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  function leaves(shield: ShieldView, group: Group): Leaf[] {
    const sid = shield.id;
    if (group === "terminals")
      return panes
        .filter((p) => p.shieldId === sid)
        .map((pane) => {
          const { kind } = pane;
          const session = shield.sessions.find((s) => s.terminalId === pane.key);
          const state = !session ? "dormant" : session.exitCode !== null ? `exited ${session.exitCode}` : undefined;
          const name = paneName(pane, panes);
          const close: MenuAction = { label: "Close", icon: <X size={14} />, run: () => onClosePane(pane.key) };
          const open: MenuAction[] = state ? [{ label: "Open", icon: <Play size={14} />, run: () => onReopen(pane) }] : [];
          return {
            entity: pane.key,
            label: (
              <span className="grow">
                {name}
                {state && <span className="muted"> · {state}</span>}
              </span>
            ),
            name,
            icon: <KindIcon kind={kind} />,
            view: { kind: "terminal", key: pane.key },
            menu:
              kind.type === "agent"
                ? [
                    ...open,
                    {
                      label: "Continue last session",
                      icon: <History size={14} />,
                      run: () => onNewSession(sid, { ...kind, resume: true }),
                    },
                    close,
                  ]
                : [...open, close],
          };
        });
    const shieldRunning = shield.status.state === "running";
    if (group === "tasks")
      return shield.tasks.map((task) => {
        const taskRuns = (runs[sid] ?? []).filter((r) => r.taskId === task.id);
        const latest = taskRuns[taskRuns.length - 1];
        const live = taskRuns.some((r) => isLive(r.state));
        const badge = runBadge(latest);
        return {
          entity: task.id,
          label: (
            <>
              <span className="grow">{task.title}</span>
              {badge && <span className={latest?.state === "done" && !latest.ship ? "danger tree-state" : "muted tree-state"}>{badge}</span>}
            </>
          ),
          name: task.title,
          title: task.request,
          icon: <ListChecks size={14} className={live ? "ok" : undefined} />,
          view: { kind: "task", taskId: task.id },
          menu: [
            {
              label: "Start run",
              icon: <Play size={14} />,
              disabled: !shieldRunning || live,
              run: () =>
                run(async () => {
                  const next = await startTaskRun(sid, task.id);
                  onOpen(sid, { kind: "task", taskId: task.id, run: next });
                  onRunsChange(sid);
                }),
            },
            { label: "Edit", icon: <Pencil size={14} />, run: () => onOpen(sid, { kind: "taskEdit", taskId: task.id }) },
            {
              label: "Delete",
              icon: <Trash2 size={14} />,
              disabled: live,
              confirm: `Delete the task ${task.title} and its runs?`,
              run: () => run(() => removeTask(sid, task.id).then(() => onRunsChange(sid))),
            },
          ],
        };
      });
    return shield.apps.map((app) => {
      const appRun = shield.appRuns.find((r) => r.appId === app.id);
      const state = !appRun ? "stopped" : appRun.exitCode === null ? "running" : `exited ${appRun.exitCode}`;
      const running = state === "running";
      return {
        entity: app.id,
        label: (
          <>
            <span className="grow">{app.name}</span>
            <span className="muted tree-state">{state}</span>
          </>
        ),
        name: app.name,
        title: app.command,
        icon: <AppWindow size={14} className={running ? "ok" : undefined} />,
        view: { kind: "app", appId: app.id },
        menu: [
          {
            label: running ? "Stop" : "Start",
            icon: running ? <Square size={14} /> : <Play size={14} />,
            disabled: !shieldRunning,
            run: () => run(() => (running ? stopApp(sid, app.id) : startApp(sid, app.id))),
          },
          {
            label: "Restart",
            icon: <RotateCw size={14} />,
            disabled: !shieldRunning,
            run: () => run(() => restartApp(sid, app.id)),
          },
          { label: "Edit", icon: <Pencil size={14} />, run: () => onIntent(sid, "app", app.id) },
          {
            label: "Delete",
            icon: <Trash2 size={14} />,
            confirm: `Delete the app ${app.name}? It stops.`,
            run: () => run(() => removeApp(sid, app.id)),
          },
        ],
      };
    });
  }

  // The rows in tree order, for the arrow keys, and each row's parent.
  const visible: string[] = [];
  const parentOf: Record<string, string> = {};
  const lists: Record<string, string[]> = { shields: shields.map((s) => s.id) };
  const content = shields.map((shield) => {
    const sid = shield.id;
    const shieldRow = `s|${sid}`;
    visible.push(shieldRow);
    const groups = GROUPS.map(({ group, label }) => {
      const id = `g|${sid}|${group}`;
      const container = `${sid}|${group}`;
      const items = leaves(shield, group);
      lists[container] = items.map((i) => i.entity);
      const open = items.length > 0 && !collapsed.has(id);
      const showOpen = !collapsed.has(shieldRow);
      if (showOpen) {
        visible.push(id);
        parentOf[id] = shieldRow;
        if (open)
          for (const item of items) {
            visible.push(`i|${container}|${item.entity}`);
            parentOf[`i|${container}|${item.entity}`] = id;
          }
      }
      return { group, label, id, container, items, open };
    });
    return { shield, shieldRow, groups };
  });
  const tabStop = focused && visible.includes(focused) ? focused : (visible.find((id) => id === `s|${selectedId}`) ?? visible[0]);

  const selectedRow =
    view.kind === "terminal"
      ? `i|${selectedId}|terminals|${view.key}`
      : view.kind === "app"
          ? `i|${selectedId}|apps|${view.appId}`
          : view.kind === "task" || (view.kind === "taskEdit" && view.taskId)
            ? `i|${selectedId}|tasks|${view.taskId}`
            : `s|${selectedId}`;

  function focus(id: string | undefined, event: KeyboardEvent) {
    if (!id) return;
    event.preventDefault();
    setFocused(id);
    tree.current?.querySelector<HTMLElement>(`[data-tree-id="${window.CSS.escape(id)}"]`)?.focus();
  }

  function onKeyDown(event: KeyboardEvent<HTMLUListElement>) {
    const item = event.target as HTMLElement;
    if (dragging || event.defaultPrevented || item.getAttribute("role") !== "treeitem") return;
    const id = item.dataset.treeId!;
    const index = visible.indexOf(id);
    const expanded = item.getAttribute("aria-expanded");
    switch (event.key) {
      case "ArrowDown":
        return focus(visible[index + 1], event);
      case "ArrowUp":
        return focus(visible[index - 1], event);
      case "Home":
        return focus(visible[0], event);
      case "End":
        return focus(visible[visible.length - 1], event);
      case "ArrowRight":
        if (expanded === "true") return focus(visible[index + 1], event);
        if (expanded === "false") {
          event.preventDefault();
          toggle(id);
        }
        return;
      case "ArrowLeft":
        if (expanded === "true") {
          event.preventDefault();
          return toggle(id);
        }
        return focus(parentOf[id], event);
      case "Enter":
        event.preventDefault();
        item.querySelector<HTMLElement>(":scope > .tree-row")?.click();
        return;
      case "F10":
      case "ContextMenu":
        if (event.key === "F10" && !event.shiftKey) return;
        if (!id.startsWith("i|")) return;
        event.preventDefault();
        setMenu(id);
    }
  }

  function closeMenu(refocus: boolean) {
    if (refocus && menu) tree.current?.querySelector<HTMLElement>(`[data-tree-id="${window.CSS.escape(menu)}"]`)?.focus();
    setMenu(undefined);
  }

  const collision: CollisionDetection = (args) =>
    closestCenter({
      ...args,
      droppableContainers: args.droppableContainers.filter(
        (c) => (c.data.current as DragData | undefined)?.container === dataOf(args.active).container,
      ),
    });

  // An item over another shield shows a forbidden drop. The check uses the
  // dragged row's rect, so it works for the pointer and the keyboard.
  function onDragMove({ active }: DragMoveEvent) {
    const data = dataOf(active);
    const rect = active.rect.current.translated;
    if (data.container === "shields" || !rect) return;
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    const target = [...(tree.current?.querySelectorAll<HTMLElement>("[data-shield-drop]") ?? [])].find((el) => {
      const r = el.getBoundingClientRect();
      return x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
    })?.dataset.shieldDrop;
    const next = target && target !== data.shieldId ? target : undefined;
    if (next === forbidden) return;
    setForbidden(next);
    if (next) setAnnouncement(FORBIDDEN);
  }

  function onDragEnd({ active, over }: DragEndEvent) {
    setDragging(false);
    if (forbidden) {
      setForbidden(undefined);
      setAnnouncement(`${dataOf(active).label} was not moved. ${FORBIDDEN}`);
      return;
    }
    if (!over || over.id === active.id) return;
    const { container, shieldId, entity } = dataOf(active);
    const list = lists[container];
    const next = arrayMove(list, list.indexOf(entity), list.indexOf(dataOf(over).entity));
    if (container === "shields") run(() => reorderShields(next));
    else if (container.endsWith("|apps")) run(() => reorderApps(shieldId, next));
    else onMoveTerminals(shieldId, next);
  }

  const announcements: Announcements = {
    onDragStart: ({ active }) => `Picked up ${dataOf(active).label}.`,
    onDragOver: ({ active, over }) => (over ? `${dataOf(active).label} is over ${dataOf(over).label}.` : undefined),
    onDragEnd: ({ active, over }) => (over ? `${dataOf(active).label} was dropped over ${dataOf(over).label}.` : undefined),
    onDragCancel: ({ active }) => `Moving ${dataOf(active).label} was cancelled.`,
  };

  function groupAction(shield: ShieldView, group: Group) {
    const id = `g|${shield.id}|terminals`;
    const [label, action] =
      group === "terminals"
        ? ["New Mac shell", () => onNewSession(shield.id, { type: "host" })]
        : group === "apps"
          ? ["Add app", () => onIntent(shield.id, "app")]
          : ["New task", () => onOpen(shield.id, { kind: "taskEdit" })];
    return (
      <>
        <IconButton
          icon={<Plus size={12} />}
          label={label}
          tabIndex={-1}
          onClick={(e) => {
            e.stopPropagation();
            action();
          }}
        />
        {group === "terminals" && (
          <IconButton
            icon={<ChevronDown size={12} />}
            label="New terminal"
            aria-haspopup="menu"
            aria-expanded={menu === id}
            tabIndex={-1}
            onClick={(e) => {
              e.stopPropagation();
              setMenu(id);
            }}
          />
        )}
      </>
    );
  }

  return (
    <aside className={forbidden ? "rail forbidden" : "rail"}>
      {children}
      <div className="rail-tree" data-drop="rail">
        {dropFolder && (
          <Card variant="outlined" padding="sm" className="stack">
            <p>
              Create a shield for <span className="mono">{dropFolder.folder}</span>?
            </p>
            {dropFolder.dropped !== dropFolder.folder && (
              <p className="muted">
                <span className="mono">{dropFolder.dropped}</span> is a link to{" "}
                <span className="mono">{dropFolder.folder}</span>.
              </p>
            )}
            <div className="row end">
              <Button size="sm" variant="ghost" onClick={() => onDropDone(false)}>
                Cancel
              </Button>
              <Button size="sm" onClick={() => onDropDone(true)}>
                Create shield
              </Button>
            </div>
          </Card>
        )}
        <DndContext
          sensors={sensors}
          collisionDetection={collision}
          accessibility={{
            announcements,
            screenReaderInstructions: {
              draggable:
                "To move a row, press Space. Use the arrow keys to move it in its group, then press Space to drop it or Escape to cancel.",
            },
          }}
          onDragStart={() => setDragging(true)}
          onDragMove={onDragMove}
          onDragEnd={onDragEnd}
          onDragCancel={() => {
            setDragging(false);
            setForbidden(undefined);
          }}
        >
          <ul
            ref={tree}
            className="tree"
            role="tree"
            aria-label="Shields"
            onKeyDown={onKeyDown}
            onFocus={(e) => {
              const id = (e.target as HTMLElement).dataset.treeId;
              if (id) setFocused(id);
            }}
          >
            <SortableContext items={shields.map((s) => `s|${s.id}`)} strategy={verticalListSortingStrategy}>
              {content.map(({ shield, shieldRow, groups }) => {
                const shieldOpen = !collapsed.has(shieldRow);
                const running = shield.status.state === "running";
                return (
                  <TreeRow
                    key={shield.id}
                    id={shieldRow}
                    level={1}
                    drag={{ container: "shields", shieldId: shield.id, entity: shield.id, label: shield.name }}
                    expanded={shieldOpen}
                    selected={selectedRow === shieldRow}
                    tabStop={tabStop === shieldRow}
                    className={`tree-shield ${shield.id === selectedId ? "current" : ""} ${forbidden === shield.id ? "forbidden-target" : ""}`}
                    shieldDrop={shield.id}
                    onOpen={() => onSelect(shield.id)}
                    row={
                      <>
                        <IconButton
                          icon={<ChevronRight size={14} className={shieldOpen ? "tree-chevron open" : "tree-chevron"} />}
                          label={shieldOpen ? `Collapse ${shield.name}` : `Expand ${shield.name}`}
                          tabIndex={-1}
                          onClick={(e) => {
                            e.stopPropagation();
                            toggle(shieldRow);
                          }}
                        />
                        <span className={`dot ${shield.status.state}`} aria-label={shield.status.state} />
                        <span
                          className="grow tree-name"
                          title={shield.status.state === "error" ? shield.status.message : shield.folder}
                        >
                          {shield.name}
                        </span>
                        {blocked[shield.id] > 0 && (
                          <span className="rail-blocked" title="Blocked network events since you last saw them">
                            {blocked[shield.id]}
                          </span>
                        )}
                        <IconButton
                          icon={running ? <Square size={12} /> : <Play size={12} />}
                          label={running ? `Stop ${shield.name}` : `Start ${shield.name}`}
                          tabIndex={-1}
                          disabled={shield.status.state === "starting" || shield.status.state === "stopping"}
                          onClick={(e) => {
                            e.stopPropagation();
                            onToggle(shield);
                          }}
                        />
                      </>
                    }
                  >
                    {shieldOpen && (
                      <ul role="group">
                        {groups.map(({ group, label, id, container, items, open }) => (
                          <TreeRow
                            key={group}
                            id={id}
                            level={2}
                            expanded={items.length > 0 ? open : undefined}
                            selected={selectedRow === id}
                            tabStop={tabStop === id}
                            onOpen={() => items.length > 0 && toggle(id)}
                            row={
                              <>
                                <ChevronRight
                                  size={12}
                                  className={open ? "tree-chevron open" : "tree-chevron"}
                                  visibility={items.length > 0 ? "visible" : "hidden"}
                                />
                                <span className="grow">{label}</span>
                                <span className="muted tree-state">{items.length}</span>
                                {groupAction(shield, group)}
                              </>
                            }
                          >
                            {menu === id && (
                              <RowMenu
                                label="New terminal"
                                actions={[
                                  {
                                    label: "Mac shell",
                                    icon: <Laptop size={14} />,
                                    run: () => onNewSession(shield.id, { type: "host" }),
                                  },
                                  {
                                    label: "VM shell",
                                    icon: <Box size={14} />,
                                    run: () => onNewSession(shield.id, { type: "shell" }),
                                  },
                                  ...agentsOf(shield).map((agent) => {
                                    const Icon = AGENT_ICONS[agent];
                                    return {
                                      label: AGENT_NAMES[agent],
                                      icon: <Icon size={14} />,
                                      run: () => onNewSession(shield.id, { type: "agent", agent, resume: false }),
                                    };
                                  }),
                                ]}
                                onClose={closeMenu}
                              />
                            )}
                            {open && (
                              <ul role="group">
                                <SortableContext
                                  items={items.map((i) => `i|${container}|${i.entity}`)}
                                  strategy={verticalListSortingStrategy}
                                >
                                  {items.map((item) => {
                                    const rowId = `i|${container}|${item.entity}`;
                                    return (
                                      <TreeRow
                                        key={item.entity}
                                        id={rowId}
                                        level={3}
                                        // Tasks have no saved order.
                                        drag={group === "tasks" ? undefined : { container, shieldId: shield.id, entity: item.entity, label: item.name }}
                                        selected={selectedRow === rowId}
                                        tabStop={tabStop === rowId}
                                        onOpen={() => onOpen(shield.id, item.view)}
                                        onMenu={() => setMenu(rowId)}
                                        row={
                                          <>
                                            {item.icon}
                                            <span className="row grow" title={item.title ?? item.name}>
                                              {item.label}
                                            </span>
                                            <IconButton
                                              icon={<Ellipsis size={12} />}
                                              label={`Actions for ${item.name}`}
                                              aria-haspopup="menu"
                                              aria-expanded={menu === rowId}
                                              tabIndex={-1}
                                              onClick={(e) => {
                                                e.stopPropagation();
                                                setMenu(rowId);
                                              }}
                                            />
                                          </>
                                        }
                                      >
                                        {menu === rowId && (
                                          <RowMenu label={`Actions for ${item.name}`} actions={item.menu} onClose={closeMenu} />
                                        )}
                                      </TreeRow>
                                    );
                                  })}
                                </SortableContext>
                              </ul>
                            )}
                          </TreeRow>
                        ))}
                      </ul>
                    )}
                  </TreeRow>
                );
              })}
            </SortableContext>
          </ul>
        </DndContext>
        <div className="sr-only" aria-live="assertive">
          {announcement}
        </div>
      </div>
      <div className="row">
        <Button variant="ghost" size="sm" iconLeft={<Plus size={14} />} onClick={onNew}>
          New shield
        </Button>
        <Button variant="ghost" size="sm" iconLeft={<Settings size={14} />} onClick={onSettings}>
          Settings <kbd className="muted">⌘,</kbd>
        </Button>
      </div>
    </aside>
  );
}
