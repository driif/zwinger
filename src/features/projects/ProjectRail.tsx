import { Button, Card, IconButton, TextField } from "@driif/motiv";
import {
  Active,
  Announcements,
  closestCenter,
  CollisionDetection,
  DndContext,
  DragEndEvent,
  DragMoveEvent,
  DragOverEvent,
  DragStartEvent,
  KeyboardCoordinateGetter,
  KeyboardSensor,
  Over,
  PointerSensor,
  useDroppable,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { arrayMove, SortableContext, sortableKeyboardCoordinates, useSortable, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import {
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
  Settings,
  Square,
  Trash2,
  X,
} from "lucide-react";
import { KeyboardEvent, ReactNode, useEffect, useRef, useState } from "react";
import { arrangeWorkspaces, createWorkspace, deleteWorkspace, renameProject, renameWorkspace } from "./api";
import { isLive, removeTask, RunSummary, startTaskRun } from "../tasks/api";
import {
  AGENT_NAMES,
  SessionKind,
  ProjectView,
  INVALID_ORDER,
  Workspace,
} from "../../shared/api";
import { errorCode } from "../../shared/errorMessage";
import type { DroppedFolder, Pane, Run, View } from "../../App";
import { AGENT_ICONS } from "../tasks/roomParts";
import { ActionMenu, MenuAction } from "../../shared/ActionMenu";
import { tildePath, truncateMiddle } from "../../shared/paths";
import { LaunchAction, launchActions } from "../launcher/actions";
import { Editors } from "../launcher/api";
import { launchMenu } from "../launcher/SplitLaunch";
import { StartProgress } from "./StartProgress";
import { moveSession, stepSession, stepWorkspace } from "./tree";
import "./rail.css";

const COLLAPSED_KEY = "zwinger.tree.collapsed";
const FORBIDDEN = "Not allowed: each session has its own cage. Terminals stay in their session.";

type Group = "terminals" | "tasks";
const GROUPS: { group: Group; label: string }[] = [
  { group: "terminals", label: "Terminals" },
  { group: "tasks", label: "Tasks" },
];

const kindName = (kind: SessionKind) =>
  kind.type === "host" ? "Mac shell" : kind.type === "shell" ? "VM shell" : AGENT_NAMES[kind.agent];

/** Where a terminal runs: on this Mac, or in the cage. An agent always runs in the VM. */
export function KindIcon({ kind }: { kind: SessionKind }) {
  if (kind.type === "agent") {
    const Icon = AGENT_ICONS[kind.agent];
    return <Icon size={14} aria-label="In the VM" />;
  }
  return kind.type === "host" ? <Laptop size={14} aria-label="On this Mac" /> : <Box size={14} aria-label="In the VM" />;
}

/** The name of a terminal: its kind, and its number among the project's terminals of that kind. */
export function paneName(pane: Pane, panes: Pane[]) {
  const same = panes.filter((p) => p.projectId === pane.projectId && kindName(p.kind) === kindName(pane.kind));
  return `${kindName(pane.kind)} ${same.indexOf(pane) + 1}`;
}

/** The badge of a task: its latest run. */
function runBadge(run: RunSummary | undefined) {
  if (!run) return undefined;
  if (run.state === "running") return run.stage ?? "running";
  if (run.state === "done") return run.ship ? "ship" : "no ship";
  return run.state;
}

/**
 * What dnd-kit carries per row. `container` is "workspaces" for a workspace, the workspace id for a session and
 * `<project id>|terminals` for a terminal. A `drop` is the header of a collapsed or empty workspace.
 */
interface DragData {
  kind: "workspace" | "session" | "terminal" | "drop";
  container: string;
  projectId?: string;
  entity: string;
  label: string;
}

const dataOf = (item: Active | Over) => item.data.current as DragData;

interface Leaf {
  entity: string;
  label: ReactNode;
  name: string;
  title?: string;
  icon: ReactNode;
  view: View;
  menu: MenuAction[];
}

interface RowProps {
  id: string;
  level: number;
  drag?: DragData;
  expanded?: boolean;
  selected: boolean;
  tabStop: boolean;
  className?: string;
  projectDrop?: string;
  /** The workspace that a folder dropped from Finder on this row goes to. */
  workspaceDrop?: string;
  /** Makes the row itself a drop target for sessions. */
  drop?: DragData;
  row: ReactNode;
  onOpen: () => void;
  /** Opens the row menu on a right-click. */
  onMenu?: () => void;
  /** Starts a rename on a double-click. */
  onRename?: () => void;
  children?: ReactNode;
}

/** A treeitem. A sortable one is dragged by its own row only, so a drag in a child list does not move the parent. */
function TreeRow(props: RowProps) {
  const { id, level, drag, expanded, selected, tabStop, className, projectDrop, workspaceDrop, drop, row, onOpen, onMenu, onRename, children } =
    props;
  const sortable = useSortable({ id, data: drag, disabled: !drag });
  const droppable = useDroppable({ id: `d|${id}`, data: drop, disabled: !drop });
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
      data-project-drop={projectDrop}
      data-workspace-drop={workspaceDrop}
      className={`tree-item ${className ?? ""} ${sortable.isDragging ? "dragging" : ""} ${droppable.isOver ? "drop-target" : ""}`}
      style={{ transform: CSS.Translate.toString(sortable.transform), transition: sortable.transition }}
      onKeyDown={(event) => event.target === event.currentTarget && sortable.listeners?.onKeyDown?.(event)}
    >
      <div
        ref={(element) => {
          sortable.setActivatorNodeRef(element);
          droppable.setNodeRef(element);
        }}
        className={`tree-row level-${level}`}
        onPointerDown={(event) => sortable.listeners?.onPointerDown?.(event)}
        onClick={onOpen}
        onDoubleClick={(event) => !(event.target as Element).closest("button") && onRename?.()}
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

/**
 * The name of a row as an input. Enter or a blur saves, Escape cancels. `onDone` runs once: with the new name, or with
 * `undefined` on Escape. Its keys and clicks stay here, so the tree and dnd-kit do not see them.
 */
function NameField({ name, onDone }: { name: string; onDone: (name: string | undefined, refocus: boolean) => void }) {
  const done = useRef(false);
  const finish = (next: string | undefined, refocus: boolean) => {
    if (done.current) return;
    done.current = true;
    onDone(next, refocus);
  };
  const stop = (event: { stopPropagation: () => void }) => event.stopPropagation();
  return (
    <span className="grow tree-edit" onPointerDown={stop} onClick={stop} onDoubleClick={stop} onContextMenu={stop}>
      <TextField
        label={<span className="sr-only">Name</span>}
        defaultValue={name}
        maxLength={200}
        autoFocus
        onFocus={(event) => event.currentTarget.select()}
        onBlur={(event) => finish(event.currentTarget.value.trim(), false)}
        onKeyDown={(event) => {
          event.stopPropagation();
          if (event.key === "Enter") {
            event.preventDefault();
            finish(event.currentTarget.value.trim(), true);
          } else if (event.key === "Escape") {
            event.preventDefault();
            finish(undefined, true);
          }
        }}
      />
    </span>
  );
}

interface Props {
  projects: ProjectView[];
  /** Each project is in exactly one workspace. */
  workspaces: Workspace[];
  selectedId?: string;
  view: View;
  /** Per project, the blocked events that the Network tab has not shown yet. */
  blocked: Record<string, number>;
  /** Per project, the runs of its tasks. */
  runs: Record<string, RunSummary[]>;
  panes: Pane[];
  run: Run;
  onOpen: (projectId: string, view: View) => void;
  onRunsChange: (projectId: string) => void;
  onToggle: (project: ProjectView) => void;
  onNewSession: (projectId: string, kind: SessionKind) => void;
  onClosePane: (key: string) => void;
  /** Opens a dormant or exited terminal again. */
  onReopen: (pane: Pane) => void;
  /** Saves the order of all terminals of the project. */
  onMoveTerminals: (projectId: string, ids: string[]) => void;
  onNew: () => void;
  /** Opens the app settings. */
  onSettings: () => void;
  /** A folder dropped from Finder on the rail. */
  dropFolder?: DroppedFolder;
  editors?: Editors;
  /** The home folder, shown as `~`. */
  home?: string;
  onLaunch: (projectId: string, action: LaunchAction) => void;
  onDropDone: (create: boolean) => void;
  children: ReactNode;
}

/** The sessions as a tree: workspace → session → Terminals / Tasks → items. A session is a project. */
export function ProjectRail({
  projects,
  workspaces,
  selectedId,
  view,
  blocked,
  runs,
  panes,
  run,
  onOpen,
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
  editors,
  home,
  onLaunch,
  children,
}: Props) {
  const tree = useRef<HTMLUListElement>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(
    () => new Set(JSON.parse(localStorage.getItem(COLLAPSED_KEY) ?? "[]") as string[]),
  );
  const [focused, setFocused] = useState<string>();
  const [dragging, setDragging] = useState(false);
  /** During a session drag and until its save is done: the workspaces with the session where it is now. */
  const [draft, setDraft] = useState<Workspace[]>();
  /** The other session that a dragged terminal is over. */
  const [forbidden, setForbidden] = useState<string>();
  const [announcement, setAnnouncement] = useState("");
  /** The row whose menu is open. */
  const [menu, setMenu] = useState<string>();
  /** The row whose name is an input. */
  const [editing, setEditing] = useState<string>();
  /** The row of a keyboard drag. A keyboard drag moves the draft one step per arrow key, and its drop saves the draft. */
  const keyboardDrag = useRef<string | undefined>(undefined);
  const keyboardStep = useRef<KeyboardCoordinateGetter>(() => undefined);
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    // Enter opens a row, so only Space picks up and drops.
    useSensor(KeyboardSensor, {
      coordinateGetter: (event, args) => keyboardStep.current(event, args),
      keyboardCodes: { start: ["Space"], cancel: ["Escape"], end: ["Space"] },
    }),
  );

  useEffect(() => {
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...collapsed]));
  }, [collapsed]);

  // A session that moves to another workspace is a new element, so it gets the focus again.
  useEffect(() => {
    if (keyboardDrag.current) focusRow(keyboardDrag.current);
  }, [draft]);

  const toggle = (id: string) =>
    setCollapsed((current) => {
      const next = new Set(current);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  function leaves(project: ProjectView, group: Group): Leaf[] {
    const sid = project.id;
    if (group === "terminals")
      return panes
        .filter((p) => p.projectId === sid)
        .map((pane) => {
          const { kind } = pane;
          const session = project.sessions.find((s) => s.terminalId === pane.key);
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
    const cageRunning = project.status.state === "running";
    return project.tasks.map((task) => {
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
            disabled: !cageRunning || live,
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
  }

  const shown = draft ?? workspaces;
  const byId = new Map(projects.map((p) => [p.id, p]));

  // The rows in tree order, for the arrow keys, and each row's parent.
  const visible: string[] = [];
  const parentOf: Record<string, string> = {};
  const lists: Record<string, string[]> = {};
  const content = shown.map((workspace) => {
    const workspaceRow = `w|${workspace.id}`;
    visible.push(workspaceRow);
    const workspaceOpen = !collapsed.has(workspaceRow);
    const sessions = workspace.sessions.flatMap((id) => byId.get(id) ?? []).map((project) => {
      const sid = project.id;
      const projectRow = `s|${sid}`;
      const sessionOpen = workspaceOpen && !collapsed.has(projectRow);
      if (workspaceOpen) {
        visible.push(projectRow);
        parentOf[projectRow] = workspaceRow;
      }
      const groups = GROUPS.map(({ group, label }) => {
        const id = `g|${sid}|${group}`;
        const container = `${sid}|${group}`;
        const items = leaves(project, group);
        lists[container] = items.map((i) => i.entity);
        const open = items.length > 0 && !collapsed.has(id);
        if (sessionOpen) {
          visible.push(id);
          parentOf[id] = projectRow;
          if (open)
            for (const item of items) {
              visible.push(`i|${container}|${item.entity}`);
              parentOf[`i|${container}|${item.entity}`] = id;
            }
        }
        return { group, label, id, container, items, open };
      });
      return { project, projectRow, groups };
    });
    return { workspace, workspaceRow, workspaceOpen, sessions };
  });
  const tabStop = focused && visible.includes(focused) ? focused : (visible.find((id) => id === `s|${selectedId}`) ?? visible[0]);

  const selectedRow =
    view.kind === "terminal"
      ? `i|${selectedId}|terminals|${view.key}`
      : view.kind === "task" || (view.kind === "taskEdit" && view.taskId)
        ? `i|${selectedId}|tasks|${view.taskId}`
        : `s|${selectedId}`;

  const focusRow = (id: string) => tree.current?.querySelector<HTMLElement>(`[data-tree-id="${window.CSS.escape(id)}"]`)?.focus();

  function focus(id: string | undefined, event: KeyboardEvent) {
    if (!id) return;
    event.preventDefault();
    setFocused(id);
    focusRow(id);
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
        if (id.startsWith("g|")) return;
        event.preventDefault();
        setMenu(id);
    }
  }

  function closeMenu(refocus: boolean) {
    if (refocus && menu) focusRow(menu);
    setMenu(undefined);
  }

  /** The name input of row `id`. A changed name is saved; a blank one goes to the backend, which applies its rules. */
  function nameField(id: string, name: string, save: (name: string) => Promise<unknown>) {
    return (
      <NameField
        name={name}
        onDone={(next, refocus) => {
          setEditing(undefined);
          if (refocus) focusRow(id);
          if (next !== undefined && next !== name) run(() => save(next));
        }}
      />
    );
  }

  const renameAction = (id: string): MenuAction => ({ label: "Rename", icon: <Pencil size={14} />, run: () => setEditing(id) });

  // With the keyboard, a session or a workspace moves one step per arrow key in the draft. The drag itself stays where
  // it is: the rows move in the DOM. A terminal moves to the next terminal of its own session.
  keyboardStep.current = (event, args) => {
    if (!args.context.active) return;
    const data = dataOf(args.context.active);
    if (data.kind === "terminal") {
      const { droppableContainers } = args.context;
      const same = {
        getEnabled: () => droppableContainers.getEnabled().filter((c) => (c.data.current as DragData | undefined)?.container === data.container),
        get: (id: string) => droppableContainers.get(id),
      } as unknown as typeof droppableContainers;
      return sortableKeyboardCoordinates(event, { ...args, context: { ...args.context, droppableContainers: same } });
    }
    if (event.code !== "ArrowDown" && event.code !== "ArrowUp") return;
    event.preventDefault();
    const dir = event.code === "ArrowDown" ? 1 : -1;
    const list = draft ?? workspaces;
    const next = data.kind === "workspace" ? stepWorkspace(list, data.entity, dir) : stepSession(list, data.entity, dir);
    if (next === list) return;
    setDraft(next);
    const workspace = next.find((w) => (data.kind === "workspace" ? w.id === data.entity : w.sessions.includes(data.entity)))!;
    if (data.kind === "workspace") {
      setAnnouncement(`${data.label} is at place ${next.indexOf(workspace) + 1} of ${next.length}.`);
    } else {
      setCollapsed((current) => {
        const open = new Set(current);
        open.delete(`w|${workspace.id}`);
        return open;
      });
      const place = workspace.sessions.indexOf(data.entity) + 1;
      setAnnouncement(`${data.label} is in ${workspace.name} at place ${place} of ${workspace.sessions.length}.`);
    }
  };

  // A session can go to another workspace; a workspace and a terminal only move in their own list.
  const collision: CollisionDetection = (args) => {
    const active = dataOf(args.active);
    return closestCenter({
      ...args,
      droppableContainers: args.droppableContainers.filter((c) => {
        const data = c.data.current as DragData | undefined;
        if (!data) return false;
        if (active.kind === "session") return data.kind === "session" || data.kind === "drop";
        return data.kind === active.kind && data.container === active.container;
      }),
    });
  };

  // A terminal over another session shows a forbidden drop. The check uses the
  // dragged row's rect, so it works for the pointer and the keyboard.
  function onDragMove({ active }: DragMoveEvent) {
    const data = dataOf(active);
    const rect = active.rect.current.translated;
    if (data.kind !== "terminal" || !rect) return;
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    const target = [...(tree.current?.querySelectorAll<HTMLElement>("[data-project-drop]") ?? [])].find((el) => {
      const r = el.getBoundingClientRect();
      return x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
    })?.dataset.projectDrop;
    const next = target && target !== data.projectId ? target : undefined;
    if (next === forbidden) return;
    setForbidden(next);
    if (next) setAnnouncement(FORBIDDEN);
  }

  function onDragStart({ active, activatorEvent }: DragStartEvent) {
    setDragging(true);
    keyboardDrag.current = activatorEvent instanceof globalThis.KeyboardEvent ? String(active.id) : undefined;
  }

  // A session over a session of another workspace moves there at once, so the rows make room for it.
  function onDragOver({ active, over }: DragOverEvent) {
    const data = dataOf(active);
    if (keyboardDrag.current || data.kind !== "session" || !over) return;
    const target = dataOf(over);
    if (target.kind !== "session") return;
    const list = draft ?? workspaces;
    if (list.find((w) => w.sessions.includes(data.entity))?.id === target.container) return;
    setDraft(moveSession(list, data.entity, target.container, target.entity));
  }

  function onDragEnd({ active, over }: DragEndEvent) {
    setDragging(false);
    const keyboard = keyboardDrag.current;
    keyboardDrag.current = undefined;
    const data = dataOf(active);
    if (data.kind === "terminal") {
      if (forbidden) {
        setForbidden(undefined);
        setAnnouncement(`${data.label} was not moved. ${FORBIDDEN}`);
        return;
      }
      if (!over || over.id === active.id) return;
      const list = lists[data.container];
      onMoveTerminals(data.projectId!, arrayMove(list, list.indexOf(data.entity), list.indexOf(dataOf(over).entity)));
      return;
    }
    let next = draft ?? workspaces;
    if (keyboard) return save(next);
    if (!over) return setDraft(undefined);
    const target = dataOf(over);
    if (data.kind === "workspace") {
      const ids = next.map((w) => w.id);
      next = arrayMove(next, ids.indexOf(data.entity), ids.indexOf(target.entity));
    } else if (target.kind === "drop") {
      next = moveSession(next, data.entity, target.entity);
      setCollapsed((current) => {
        const open = new Set(current);
        open.delete(`w|${target.entity}`);
        return open;
      });
    } else {
      next = next.map((w) => {
        const from = w.sessions.indexOf(data.entity);
        const to = w.sessions.indexOf(target.entity);
        return from >= 0 && to >= 0 ? { ...w, sessions: arrayMove(w.sessions, from, to) } : w;
      });
    }
    save(next);
  }

  /** Saves the order of `next`. If the tree changed since the drag began, the backend refuses it and the tree reloads. */
  function save(next: Workspace[]) {
    const order = (list: Workspace[]) => JSON.stringify(list.map((w) => [w.id, w.sessions]));
    if (order(next) === order(workspaces)) return setDraft(undefined);
    setDraft(next);
    run(() =>
      arrangeWorkspaces(next.map(({ id, sessions }) => ({ id, sessions }))).catch((e) => {
        throw errorCode(e) === INVALID_ORDER ? new Error("The sidebar changed. Try again.") : e;
      }),
    ).finally(() => setDraft(undefined));
  }

  const announcements: Announcements = {
    onDragStart: ({ active }) => `Picked up ${dataOf(active).label}.`,
    onDragOver: ({ active, over }) => (over ? `${dataOf(active).label} is over ${dataOf(over).label}.` : undefined),
    onDragEnd: ({ active, over }) => (over ? `${dataOf(active).label} was dropped over ${dataOf(over).label}.` : undefined),
    onDragCancel: ({ active }) => `Moving ${dataOf(active).label} was cancelled.`,
  };

  function workspaceMenu(workspace: Workspace): MenuAction[] {
    return [
      renameAction(`w|${workspace.id}`),
      {
        label: "Delete workspace",
        icon: <Trash2 size={14} />,
        disabled: workspace.sessions.length > 0 || workspaces.length === 1,
        run: () => run(() => deleteWorkspace(workspace.id)),
      },
    ];
  }

  function groupAction(project: ProjectView, group: Group) {
    const id = `g|${project.id}|terminals`;
    const [label, action] =
      group === "terminals"
        ? ["New Mac shell", () => onNewSession(project.id, { type: "host" })]
        : ["New task", () => onOpen(project.id, { kind: "taskEdit" })];
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

  const menuButton = (id: string, name: string) => (
    <IconButton
      icon={<Ellipsis size={12} />}
      label={`Actions for ${name}`}
      aria-haspopup="menu"
      aria-expanded={menu === id}
      tabIndex={-1}
      onClick={(e) => {
        e.stopPropagation();
        setMenu(id);
      }}
    />
  );

  const dropWorkspace = workspaces.find((w) => w.id === dropFolder?.workspace);

  return (
    <aside className={forbidden ? "rail forbidden" : "rail"}>
      {children}
      <div className="rail-tree" data-drop="rail">
        {dropFolder && (
          <Card variant="outlined" padding="sm" className="stack">
            <p>
              Create a project for <span className="mono">{dropFolder.folder}</span>
              {dropWorkspace && (
                <>
                  {" "}
                  in <strong>{dropWorkspace.name}</strong>
                </>
              )}
              ?
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
                Create project
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
                "To move a row, press Space. Use the arrow keys to move it, then press Space to drop it or Escape to cancel. A session can go to another workspace. A terminal stays in its session.",
            },
          }}
          onDragStart={onDragStart}
          onDragMove={onDragMove}
          onDragOver={onDragOver}
          onDragEnd={onDragEnd}
          onDragCancel={() => {
            keyboardDrag.current = undefined;
            setDragging(false);
            setForbidden(undefined);
            setDraft(undefined);
          }}
        >
          <ul
            ref={tree}
            className="tree"
            role="tree"
            aria-label="Workspaces"
            onKeyDown={onKeyDown}
            onFocus={(e) => {
              const id = (e.target as HTMLElement).dataset.treeId;
              if (id) setFocused(id);
            }}
          >
            <SortableContext items={shown.map((w) => `w|${w.id}`)} strategy={verticalListSortingStrategy}>
              {content.map(({ workspace, workspaceRow, workspaceOpen, sessions }) => (
                <TreeRow
                  key={workspace.id}
                  id={workspaceRow}
                  level={1}
                  drag={{ kind: "workspace", container: "workspaces", entity: workspace.id, label: workspace.name }}
                  drop={
                    !workspaceOpen || sessions.length === 0
                      ? { kind: "drop", container: workspace.id, entity: workspace.id, label: workspace.name }
                      : undefined
                  }
                  expanded={workspaceOpen}
                  selected={false}
                  tabStop={tabStop === workspaceRow}
                  className="tree-workspace"
                  workspaceDrop={workspace.id}
                  onOpen={() => toggle(workspaceRow)}
                  onMenu={() => setMenu(workspaceRow)}
                  onRename={() => setEditing(workspaceRow)}
                  row={
                    <>
                      <ChevronRight size={12} className={workspaceOpen ? "tree-chevron open" : "tree-chevron"} />
                      {editing === workspaceRow ? (
                        nameField(workspaceRow, workspace.name, (name) => renameWorkspace(workspace.id, name))
                      ) : (
                        <span className="grow tree-name">{workspace.name}</span>
                      )}
                      <span className="muted tree-state">{sessions.length}</span>
                      {menuButton(workspaceRow, workspace.name)}
                    </>
                  }
                >
                  {menu === workspaceRow && (
                    <ActionMenu
                      label={`Actions for ${workspace.name}`}
                      actions={workspaceMenu(workspace)}
                      onClose={closeMenu}
                    />
                  )}
                  {workspaceOpen && (
                    <ul role="group">
                      <SortableContext items={sessions.map((s) => s.projectRow)} strategy={verticalListSortingStrategy}>
                        {sessions.map(({ project, projectRow, groups }) => {
                          const projectOpen = !collapsed.has(projectRow);
                          const running = project.status.state === "running";
                          return (
                            <TreeRow
                              key={project.id}
                              id={projectRow}
                              level={2}
                              drag={{
                                kind: "session",
                                container: workspace.id,
                                projectId: project.id,
                                entity: project.id,
                                label: project.name,
                              }}
                              expanded={projectOpen}
                              selected={selectedRow === projectRow}
                              tabStop={tabStop === projectRow}
                              className={`tree-project ${project.id === selectedId ? "current" : ""} ${forbidden === project.id ? "forbidden-target" : ""}`}
                              projectDrop={project.id}
                              workspaceDrop={workspace.id}
                              onOpen={() => onOpen(project.id, { kind: "terminal" })}
                              onMenu={() => setMenu(projectRow)}
                              onRename={() => setEditing(projectRow)}
                              row={
                                <>
                                  <IconButton
                                    icon={<ChevronRight size={14} className={projectOpen ? "tree-chevron open" : "tree-chevron"} />}
                                    label={projectOpen ? `Collapse ${project.name}` : `Expand ${project.name}`}
                                    tabIndex={-1}
                                    onClick={(e) => {
                                      e.stopPropagation();
                                      toggle(projectRow);
                                    }}
                                  />
                                  <span className={`dot ${project.status.state}`} aria-label={project.status.state} />
                                  {editing === projectRow ? (
                                    nameField(projectRow, project.name, (name) => renameProject(project.id, name))
                                  ) : (
                                    <span
                                      className="grow tree-name"
                                      title={project.status.state === "error" ? project.status.message : project.folder}
                                    >
                                      {project.name}
                                      {project.status.state === "starting" ? (
                                        <span className="tree-path muted">
                                          <StartProgress phase={project.status.phase} />
                                        </span>
                                      ) : (
                                        <span className="tree-path muted" title={project.folder}>
                                          {truncateMiddle(tildePath(project.folder, home), 32)}
                                        </span>
                                      )}
                                    </span>
                                  )}
                                  {blocked[project.id] > 0 && (
                                    <span className="rail-blocked" title="Blocked network events since you last saw them">
                                      {blocked[project.id]}
                                    </span>
                                  )}
                                  <IconButton
                                    icon={running ? <Square size={12} /> : <Play size={12} />}
                                    label={running ? `Stop ${project.name} cage` : `Start ${project.name} cage`}
                                    tabIndex={-1}
                                    disabled={project.status.state === "starting" || project.status.state === "stopping"}
                                    onClick={(e) => {
                                      e.stopPropagation();
                                      onToggle(project);
                                    }}
                                  />
                                  {menuButton(projectRow, project.name)}
                                </>
                              }
                            >
                              {menu === projectRow && (
                                <ActionMenu
                                  label={`Actions for ${project.name}`}
                                  actions={[
                                    ...launchMenu(launchActions(project, editors), (action) => onLaunch(project.id, action)),
                                    renameAction(projectRow),
                                  ]}
                                  onClose={closeMenu}
                                />
                              )}
                              {projectOpen && (
                                <ul role="group">
                                  {groups.map(({ group, label, id, container, items, open }) => (
                                    <TreeRow
                                      key={group}
                                      id={id}
                                      level={3}
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
                                          {groupAction(project, group)}
                                        </>
                                      }
                                    >
                                      {menu === id && (
                                        <ActionMenu
                                          label="New terminal"
                                          actions={launchMenu(
                                            launchActions(project, editors).filter((a) => a.type === "session"),
                                            (action) => onLaunch(project.id, action),
                                          )}
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
                                                  level={4}
                                                  // Tasks have no saved order.
                                                  drag={
                                                    group === "tasks"
                                                      ? undefined
                                                      : { kind: "terminal", container, projectId: project.id, entity: item.entity, label: item.name }
                                                  }
                                                  selected={selectedRow === rowId}
                                                  tabStop={tabStop === rowId}
                                                  onOpen={() => onOpen(project.id, item.view)}
                                                  onMenu={() => setMenu(rowId)}
                                                  row={
                                                    <>
                                                      {item.icon}
                                                      <span className="row grow" title={item.title ?? item.name}>
                                                        {item.label}
                                                      </span>
                                                      {menuButton(rowId, item.name)}
                                                    </>
                                                  }
                                                >
                                                  {menu === rowId && (
                                                    <ActionMenu label={`Actions for ${item.name}`} actions={item.menu} onClose={closeMenu} />
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
                  )}
                </TreeRow>
              ))}
            </SortableContext>
          </ul>
        </DndContext>
        <div className="sr-only" aria-live="assertive">
          {announcement}
        </div>
      </div>
      <div className="row">
        <Button variant="ghost" size="sm" iconLeft={<Plus size={14} />} onClick={onNew}>
          New project
        </Button>
        <Button variant="ghost" size="sm" iconLeft={<Plus size={14} />} onClick={() => run(createWorkspace)}>
          New workspace
        </Button>
        <Button variant="ghost" size="sm" iconLeft={<Settings size={14} />} onClick={onSettings}>
          Settings <kbd className="muted">⌘,</kbd>
        </Button>
      </div>
    </aside>
  );
}
