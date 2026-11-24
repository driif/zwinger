import { Badge, Button, Card, Dialog, IconButton, Select, TextField } from "@driif/motiv";
import {
  Active,
  Announcements,
  closestCenter,
  CollisionDetection,
  DndContext,
  DragEndEvent,
  DragOverEvent,
  DragOverlay,
  DragStartEvent,
  KeyboardCoordinateGetter,
  KeyboardSensor,
  Over,
  PointerSensor,
  useDroppable,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { arrayMove, SortableContext, useSortable, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { Box, ChevronRight, Ellipsis, Fence, FolderInput, Hourglass, Pencil, Play, Plus, RotateCw, Settings, Square, Trash2 } from "lucide-react";
import { CSSProperties, KeyboardEvent, ReactNode, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  arrangeWorkspaces,
  createWorkspace,
  deleteWorkspace,
  renameSession,
  renameWorkspace,
  setWorkspaceCage,
} from "./api";
import { CageView, SessionCage, SessionView, INVALID_ORDER, Workspace } from "../../shared/api";
import { errorCode } from "../../shared/errorMessage";
import type { DroppedFolder, Run, VmControl } from "../../App";
import { ActionMenu, MenuAction } from "../../shared/ActionMenu";
import { tildePath, truncateMiddle } from "../../shared/paths";
import { LaunchAction, launchActions, projectEditors } from "../launcher/actions";
import { Editors } from "../launcher/api";
import type { Detection } from "../settings/api";
import { launchMenu } from "../launcher/launchMenu";
import { AgentStatusGlyph } from "../terminal/labels";
import { StartProgress } from "./StartProgress";
import { cageChanged, restartPending } from "../cages/sessionCage";
import { mostUrgent } from "./agentState";
import { cageAfterMove, moveSession, stepSession, stepWorkspace } from "./tree";
import "./rail.css";

const COLLAPSED_KEY = "zwinger.tree.collapsed";
/**
 * What dnd-kit carries per row. `container` is "workspaces" for a workspace and the workspace id for a session. A
 * `drop` is the header of a collapsed or empty workspace.
 */
interface DragData {
  kind: "workspace" | "session" | "drop";
  container: string;
  entity: string;
  label: string;
}

const dataOf = (item: Active | Over) => item.data.current as DragData;

interface RowProps {
  id: string;
  level: number;
  drag?: DragData;
  expanded?: boolean;
  selected: boolean;
  tabStop: boolean;
  className?: string;
  /** The project's accent, as `--zw-project-accent`. */
  accent?: string | null;
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
  const { id, level, drag, expanded, selected, tabStop, className, accent, workspaceDrop, drop, row, onOpen, onMenu, onRename, children } = props;
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
      data-workspace-drop={workspaceDrop}
      className={`tree-item ${className ?? ""} ${sortable.isDragging ? "dragging" : ""} ${droppable.isOver ? "drop-target" : ""}`}
      style={
        {
          transform: CSS.Translate.toString(sortable.transform),
          transition: sortable.transition,
          "--zw-project-accent": accent ?? undefined,
        } as CSSProperties
      }
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
        label="Name"
        labelPosition="hidden"
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
  density: "default" | "compact";
  sidebarFontSize: number | null;
  sessions: SessionCage[];
  /** All cages. */
  cages: CageView[];
  /** Each session is in exactly one workspace. */
  workspaces: Workspace[];
  selectedId?: string;
  /** Per session, the blocked events that the Network tab has not shown yet. */
  blocked: Record<string, number>;
  /** Per session, the requests of its VM that wait for an answer. */
  waiting: Record<string, number>;
  run: Run;
  vm: VmControl;
  /** Opens the terminal view of the session. */
  onOpen: (sessionId: string) => void;
  onToggle: (target: SessionCage) => void;
  /** Restarts the VM of the session to apply a cage change or a newer cage config. */
  onRestart: (sessionId: string) => void;
  onNew: () => void;
  /** Opens the app settings. */
  onSettings: () => void;
  /** Opens the list of cages. */
  onCages: () => void;
  /** A folder dropped from Finder on the rail. */
  dropFolder?: DroppedFolder;
  editors?: Editors;
  detection?: Detection;
  /** The home folder, shown as `~`. */
  home?: string;
  onLaunch: (sessionId: string, action: LaunchAction) => void;
  onDropDone: (create: boolean) => void;
  children: ReactNode;
}

/** The sessions as a tree: workspace → session. The session page shows the terminals and tasks of a session. */
export function ProjectRail({
  density,
  sidebarFontSize,
  sessions,
  cages,
  workspaces,
  selectedId,
  blocked,
  waiting,
  run,
  vm,
  onOpen,
  onToggle,
  onRestart,
  onNew,
  onSettings,
  onCages,
  dropFolder,
  onDropDone,
  editors,
  detection,
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
  /** The label of the row that the pointer drags. A keyboard drag moves the row itself, so it has no overlay. */
  const [overlay, setOverlay] = useState<string>();
  /** During a session drag and until its save is done: the workspaces with the session where it is now. */
  const [draft, setDraft] = useState<Workspace[]>();
  const [announcement, setAnnouncement] = useState("");
  /** The row whose menu is open. */
  const [menu, setMenu] = useState<string>();
  /** The row whose name is an input. */
  const [editing, setEditing] = useState<string>();
  /** The workspace whose default cage the picker shows. */
  const [picking, setPicking] = useState<string>();
  /** A dropped session whose move changes the cage of its running VM. The draft shows the move until the user chooses. */
  const [moving, setMoving] = useState<{ session: SessionView; next: Workspace[]; to: string | null }>();
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

  const expand = (id: string) =>
    setCollapsed((current) => {
      const next = new Set(current);
      next.delete(id);
      return next;
    });

  const shown = draft ?? workspaces;
  const byId = new Map(sessions.map((s) => [s.session.id, s]));

  // The rows in tree order, for the arrow keys, and each row's parent.
  const visible: string[] = [];
  const parentOf: Record<string, string> = {};
  const content = shown.map((workspace) => {
    const workspaceRow = `w|${workspace.id}`;
    visible.push(workspaceRow);
    const workspaceOpen = !collapsed.has(workspaceRow);
    const sessions = workspace.sessions.flatMap((id) => byId.get(id) ?? []).map((target) => {
      const sessionRow = `s|${target.session.id}`;
      if (workspaceOpen) {
        visible.push(sessionRow);
        parentOf[sessionRow] = workspaceRow;
      }
      return { target, session: target.session, sessionRow };
    });
    return { workspace, workspaceRow, workspaceOpen, sessions };
  });
  const tabStop = focused && visible.includes(focused) ? focused : (visible.find((id) => id === `s|${selectedId}`) ?? visible[0]);


  function focusRow(id: string) {
    const item = tree.current?.querySelector<HTMLElement>(`[data-tree-id="${window.CSS.escape(id)}"]`);
    if (!item) return;
    if (item === document.activeElement) revealRow(item);
    else item.focus({ preventScroll: true });
  }

  function revealRow(item: Element) {
    item.querySelector(":scope > .tree-row")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }

  function focus(id: string | undefined, event: KeyboardEvent) {
    if (!id) return;
    event.preventDefault();
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
        event.preventDefault();
        setMenu(id);
    }
  }

  function closeMenu(refocus: boolean) {
    if (refocus && menu) focusRow(menu);
    setMenu(undefined);
  }

  const toggleMenu = (id: string) => (menu === id ? closeMenu(true) : setMenu(id));

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
  // it is: the rows move in the DOM.
  keyboardStep.current = (event, args) => {
    if (!args.context.active) return;
    const data = dataOf(args.context.active);
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
      expand(`w|${workspace.id}`);
      const place = workspace.sessions.indexOf(data.entity) + 1;
      setAnnouncement(`${data.label} is in ${workspace.name} at place ${place} of ${workspace.sessions.length}.`);
    }
  };

  // A session can go to another workspace; a workspace only moves in the list of workspaces.
  const collision: CollisionDetection = (args) => {
    const active = dataOf(args.active);
    return closestCenter({
      ...args,
      droppableContainers: args.droppableContainers.filter((c) => {
        const data = c.data.current as DragData | undefined;
        if (!data) return false;
        if (active.kind === "session") return data.kind === "session" || data.kind === "drop";
        return data.kind === "workspace";
      }),
    });
  };

  function onDragStart({ active, activatorEvent }: DragStartEvent) {
    setDragging(true);
    keyboardDrag.current = activatorEvent instanceof globalThis.KeyboardEvent ? String(active.id) : undefined;
    if (!keyboardDrag.current) setOverlay(dataOf(active).label);
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
    setOverlay(undefined);
    const keyboard = keyboardDrag.current;
    keyboardDrag.current = undefined;
    const data = dataOf(active);
    let next = draft ?? workspaces;
    const session = data.kind === "session" ? data.entity : undefined;
    if (keyboard) return drop(next, session);
    if (!over) return setDraft(undefined);
    const target = dataOf(over);
    if (data.kind === "workspace") {
      const ids = next.map((w) => w.id);
      next = arrayMove(next, ids.indexOf(data.entity), ids.indexOf(target.entity));
    } else if (target.kind === "drop") {
      next = moveSession(next, data.entity, target.entity);
      expand(`w|${target.entity}`);
    } else {
      next = next.map((w) => {
        const from = w.sessions.indexOf(data.entity);
        const to = w.sessions.indexOf(target.entity);
        return from >= 0 && to >= 0 ? { ...w, sessions: arrayMove(w.sessions, from, to) } : w;
      });
    }
    drop(next, session);
  }

  /** Saves `next`, or first asks what to do when the move of `session` changes the cage of its running VM. */
  function drop(next: Workspace[], session?: string) {
    const moved = session ? byId.get(session) : undefined;
    const vm = moved?.vmCage?.status.state;
    const to = moved && vm && vm !== "error" && vm !== "stopped" ? cageAfterMove(moved.session, next) : undefined;
    if (!moved || to === undefined) return save(next);
    setDraft(next);
    setMoving({ session: moved.session, next, to });
  }

  function cancelMove() {
    setMoving(undefined);
    setDraft(undefined);
  }

  /**
   * Saves the order of `next`, then runs `then`. If the tree changed since the drag began, the backend refuses it and
   * the tree reloads. A refused save also reverts the draft.
   */
  function save(next: Workspace[], then?: () => Promise<unknown>) {
    setMoving(undefined);
    const order = (list: Workspace[]) => JSON.stringify(list.map((w) => [w.id, w.sessions]));
    if (order(next) === order(workspaces)) return setDraft(undefined);
    setDraft(next);
    run(() =>
      arrangeWorkspaces(next.map(({ id, sessions }) => ({ id, sessions })))
        .catch((e) => {
          throw errorCode(e) === INVALID_ORDER ? new Error("The sidebar changed. Try again.") : e;
        })
        .then(then),
    ).finally(() => setDraft(undefined));
  }

  const announcements: Announcements = {
    onDragStart: ({ active }) => `Picked up ${dataOf(active).label}.`,
    onDragOver: ({ active, over }) => (over ? `${dataOf(active).label} is over ${dataOf(over).label}.` : undefined),
    onDragEnd: ({ active, over }) => (over ? `${dataOf(active).label} was dropped over ${dataOf(over).label}.` : undefined),
    onDragCancel: ({ active }) => `Moving ${dataOf(active).label} was cancelled.`,
  };

  function workspaceMenu(workspace: Workspace): MenuAction[] {
    const index = workspaces.indexOf(workspace);
    // The sessions of a deleted workspace go to the workspace above it, else to the one below.
    const heir = workspaces[index - 1] ?? workspaces[index + 1];
    const remove = () => deleteWorkspace(workspace.id);
    return [
      renameAction(`w|${workspace.id}`),
      { label: "Default cage…", icon: <Fence size={14} />, run: () => setPicking(workspace.id) },
      {
        label: "Delete workspace",
        icon: <Trash2 size={14} />,
        disabled: !heir,
        confirm:
          workspace.sessions.length > 0 && heir
            ? `Delete ${workspace.name}? Its ${workspace.sessions.length} sessions move to ${heir.name}.` +
              (heir.defaultCage === workspace.defaultCage ? "" : " A session that inherits its cage gets the cage of its new workspace at its next start.")
            : undefined,
        run: () =>
          workspace.sessions.length > 0
            ? save(
                workspaces.map((w) =>
                  w === heir ? { ...w, sessions: [...w.sessions, ...workspace.sessions] } : w === workspace ? { ...w, sessions: [] } : w,
                ),
                remove,
              )
            : run(remove),
      },
    ];
  }

  function moveMenu(session: SessionView, from: Workspace): MenuAction[] {
    const others = workspaces.filter((w) => w.id !== from.id);
    if (others.length === 0) return [];
    return [
      {
        label: "Move to",
        icon: <FolderInput size={14} />,
        submenu: others.map((w) => ({
          label: w.name,
          icon: <ChevronRight size={14} />,
          run: () => {
            expand(`w|${w.id}`);
            drop(moveSession(workspaces, session.id, w.id), session.id);
          },
        })),
      },
    ];
  }

  const menuButton = (id: string, name: string) => (
    <IconButton
      size="sm"
      icon={<Ellipsis size={12} />}
      label={`Actions for ${name}`}
      aria-haspopup="menu"
      aria-expanded={menu === id}
      tabIndex={-1}
      onClick={(e) => {
        e.stopPropagation();
        toggleMenu(id);
      }}
    />
  );

  const dropWorkspace = workspaces.find((w) => w.id === dropFolder?.workspace);
  const pickWorkspace = workspaces.find((w) => w.id === picking);
  const cageName = (id: string | null) => cages.find((c) => c.id === id)?.name ?? "no cage";

  return (
    <aside
      className="rail"
      data-density={density}
      style={sidebarFontSize === null ? undefined : { "--motiv-font-size": `${sidebarFontSize}px` } as CSSProperties}
    >
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
        {pickWorkspace && (
          <Card variant="outlined" padding="sm" className="stack">
            <Select
              label={`Default cage of ${pickWorkspace.name}`}
              options={[{ value: "", label: "No default cage" }, ...cages.map((c) => ({ value: c.id, label: c.name }))]}
              value={pickWorkspace.defaultCage ?? ""}
              onChange={(id) => run(() => setWorkspaceCage(pickWorkspace.id, id || null))}
            />
            <p className="muted">The sessions of the workspace that inherit their cage use it.</p>
            <div className="row end">
              <Button size="sm" variant="ghost" onClick={() => setPicking(undefined)}>
                Done
              </Button>
            </div>
          </Card>
        )}
        {moving && (
          <Dialog
            open
            title="Change the cage of a running VM?"
            onClose={cancelMove}
          >
            <p>
              The VM of <strong>{moving.session.name}</strong> runs in cage {cageName(moving.session.vmCage)}. After the
              move, the project uses {moving.to ? `cage ${cageName(moving.to)}` : "no cage"}.
            </p>
            <div className="row end">
              <Button size="sm" variant="ghost" onClick={cancelMove}>
                Cancel
              </Button>
              <Button size="sm" variant="outline" onClick={() => save(moving.next)}>
                At next start
              </Button>
              <Button
                size="sm"
                onClick={() => {
                  const { session, next, to } = moving;
                  save(next, () => vm.stop(session.id).then(() => (to === null ? undefined : vm.start(session.id))));
                }}
              >
                Restart now
              </Button>
            </div>
          </Dialog>
        )}
        <DndContext
          sensors={sensors}
          collisionDetection={collision}
          accessibility={{
            announcements,
            screenReaderInstructions: {
              draggable:
                "To move a row, press Space. Use the arrow keys to move it, then press Space to drop it or Escape to cancel. A session can go to another workspace.",
            },
          }}
          onDragStart={onDragStart}
          onDragOver={onDragOver}
          onDragEnd={onDragEnd}
          onDragCancel={() => {
            keyboardDrag.current = undefined;
            setDragging(false);
            setOverlay(undefined);
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
              const item = e.target as HTMLElement;
              if (item.dataset.treeId) {
                setFocused(item.dataset.treeId);
                revealRow(item);
              }
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
                      <AgentStatusGlyph
                        state={mostUrgent(sessions.flatMap(({ session }) => session.processes.filter((p) => p.exitCode === null).map((p) => p.agent)))}
                      />
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
                      <SortableContext items={sessions.map((s) => s.sessionRow)} strategy={verticalListSortingStrategy}>
                        {sessions.map(({ target, session, sessionRow }) => {
                          const status = target.vmCage?.status ?? { state: "stopped" as const };
                          const launch = launchActions(target, detection, projectEditors(editors, session));
                          const running = status.state === "running";
                          const effective = session.effectiveCage;
                          const shared = running && target.vmCage && target.vmCage.members.length > 1 ? target.vmCage : null;
                          return (
                            <TreeRow
                              key={session.id}
                              id={sessionRow}
                              level={2}
                              drag={{ kind: "session", container: workspace.id, entity: session.id, label: session.name }}
                              selected={session.id === selectedId}
                              tabStop={tabStop === sessionRow}
                              className={session.id === selectedId ? "tree-project current" : "tree-project"}
                              accent={session.accent}
                              workspaceDrop={workspace.id}
                              onOpen={() => onOpen(session.id)}
                              onMenu={() => setMenu(sessionRow)}
                              onRename={() => setEditing(sessionRow)}
                              row={
                                <>
                                  <span className={`dot ${status.state}`} aria-label={status.state} />
                                  {editing === sessionRow ? (
                                    nameField(sessionRow, session.name, (name) => renameSession(session.id, name))
                                  ) : (
                                    <span
                                      className="grow tree-name"
                                      title={status.state === "error" ? status.message : session.folder}
                                    >
                                      {session.name}
                                      {status.state === "starting" ? (
                                        <span className="tree-path muted">
                                          <StartProgress phase={status.phase} />
                                        </span>
                                      ) : (
                                        <span className="tree-path muted" title={session.folder}>
                                          {truncateMiddle(tildePath(session.folder, home), 32)}
                                        </span>
                                      )}
                                    </span>
                                  )}
                                  {session.vm.kind === "own" && (
                                    <span className="muted" title={`This session runs in a VM of its own: ${session.vmKey ?? "a new key at the next start"}`}>
                                      <Box size={12} aria-label="Own VM" />
                                    </span>
                                  )}
                                  {shared ? (
                                    <span className="muted" title={`Shared VM of cage ${shared.name} with ${shared.members.length} sessions: ${shared.members.join(", ")}`}>
                                      <Fence size={12} aria-label={`Shared VM of cage ${shared.name}`} />
                                      {shared.members.length}
                                    </span>
                                  ) : (
                                    effective && (
                                      <span
                                        className="muted"
                                        title={`${cageName(effective.id)}${effective.inherited ? " (inherited)" : ""}, ${session.vm.kind === "own" ? "own VM" : "shared VM"}`}
                                      >
                                        <Fence size={12} aria-label={`Cage ${cageName(effective.id)}`} />
                                      </span>
                                    )
                                  )}
                                  {waiting[session.id] > 0 && (
                                    <Badge tone="warning" size="sm" title="Network requests that wait for your answer">
                                      <Hourglass size={10} aria-hidden="true" />
                                      {waiting[session.id]}
                                    </Badge>
                                  )}
                                  {blocked[session.id] > 0 && (
                                    <span className="rail-blocked" title="Blocked network events since you last saw them">
                                      {blocked[session.id]}
                                    </span>
                                  )}
                                  <AgentStatusGlyph
                                    state={mostUrgent(session.processes.filter((p) => p.exitCode === null).map((p) => p.agent))}
                                  />
                                  {restartPending(target) && (
                                    <IconButton
                                      size="sm"
                                      icon={<RotateCw size={12} className="warn" />}
                                      label={
                                        cageChanged(target)
                                          ? `Pending cage change: restart ${session.name} to use cage ${target.cage?.name}`
                                          : `Pending cage change: restart ${session.name} to apply the new cage config`
                                      }
                                      tabIndex={-1}
                                      onClick={(e) => {
                                        e.stopPropagation();
                                        onRestart(session.id);
                                      }}
                                    />
                                  )}
                                  <IconButton
                                    size="sm"
                                    icon={running ? <Square size={12} /> : <Play size={12} />}
                                    label={running ? `Stop ${session.name} cage` : `Start ${session.name} cage`}
                                    tabIndex={-1}
                                    disabled={(!target.cage && !running) || status.state === "starting" || status.state === "stopping"}
                                    onClick={(e) => {
                                      e.stopPropagation();
                                      onToggle(target);
                                    }}
                                  />
                                  {menuButton(sessionRow, session.name)}
                                </>
                              }
                            >
                              {menu === sessionRow && (
                                <ActionMenu
                                  label={`Actions for ${session.name}`}
                                  actions={[
                                    ...launchMenu(launch, (action) => onLaunch(session.id, action)),
                                    renameAction(sessionRow),
                                    ...moveMenu(session, workspace),
                                    ...(effective
                                      ? [
                                          session.vm.kind === "own"
                                            ? { label: "Use shared VM", icon: <Fence size={14} />, run: () => run(() => vm.setOwn(session.id, false)) }
                                            : { label: "Use own VM", icon: <Box size={14} />, run: () => run(() => vm.setOwn(session.id, true)) },
                                        ]
                                      : []),
                                  ]}
                                  onClose={closeMenu}
                                />
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
          {createPortal(
            <DragOverlay>
              {overlay && (
                <Card padding="sm" variant="outlined">
                  {overlay}
                </Card>
              )}
            </DragOverlay>,
            document.body,
          )}
        </DndContext>
        <div className="sr-only" aria-live="assertive">
          {announcement}
        </div>
      </div>
      <div className="row">
        <Button variant="ghost" size="sm" iconLeft={<Plus size={14} />} onClick={onNew}>
          New project
        </Button>
        <Button variant="ghost" size="sm" iconLeft={<Plus size={14} />} onClick={() => run(() => createWorkspace().then((id) => setEditing(`w|${id}`)))}>
          New workspace
        </Button>
        <Button variant="ghost" size="sm" iconLeft={<Fence size={14} />} onClick={onCages}>
          Cages
        </Button>
        <Button variant="ghost" size="sm" iconLeft={<Settings size={14} />} onClick={onSettings}>
          Settings <kbd className="muted">⌘,</kbd>
        </Button>
      </div>
    </aside>
  );
}
