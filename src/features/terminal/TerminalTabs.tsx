import { Button, IconButton } from "@driif/motiv";
import { ActionMenu as NewMenu, ActionMenuItem } from "@driif/motiv/action-menu";
import { DndContext, DragEndEvent, PointerSensor, useSensor, useSensors } from "@dnd-kit/core";
import { arrayMove, horizontalListSortingStrategy, SortableContext, useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { ArrowLeft, ArrowRight, Ellipsis, History, Play, Plus, X } from "lucide-react";
import { ReactNode, useState } from "react";
import type { SessionView, TerminalKind } from "../../shared/api";
import type { Pane } from "../../App";
import { ActionMenu, MenuAction } from "../../shared/ActionMenu";
import { AgentStatusGlyph, KindIcon, paneName } from "./labels";
import { useTerminalTitles } from "./titles";

interface Props {
  session: SessionView;
  /** The terminals of the session, in their saved order. */
  panes: Pane[];
  /** The terminal of the focused pane, while the terminal view shows. */
  selected?: string;
  /** The launch actions for a new terminal or agent. */
  newItems: ActionMenuItem[];
  /** Shows the terminal in the focused pane. */
  onShow: (key: string) => void;
  onReopen: (pane: Pane) => void;
  onNewSession: (kind: TerminalKind) => void;
  /** Removes the terminal and ends its process. */
  onClose: (key: string) => void;
  /** Saves the order of all terminals of the session. */
  onMove: (ids: string[]) => void;
}

/** The terminals of a session as tabs above its panes. A drag reorders them; the menu of a tab moves it with the keyboard. */
export function TerminalTabs({ session, panes, selected, newItems, onShow, onReopen, onNewSession, onClose, onMove }: Props) {
  const titles = useTerminalTitles();
  const [menu, setMenu] = useState<string>();
  const sensors = useSensors(useSensor(PointerSensor, { activationConstraint: { distance: 4 } }));
  const ids = panes.map((p) => p.key);
  const move = (key: string, to: number) => onMove(arrayMove(ids, ids.indexOf(key), to));

  function onDragEnd({ active, over }: DragEndEvent) {
    if (over && over.id !== active.id) move(String(active.id), ids.indexOf(String(over.id)));
  }

  return (
    <div className="terminal-strip row">
      <DndContext sensors={sensors} onDragEnd={onDragEnd}>
        <SortableContext items={ids} strategy={horizontalListSortingStrategy}>
          <ul className="terminal-tabs" aria-label={`Terminals of ${session.name}`}>
            {panes.map((pane, index) => {
              const { kind } = pane;
              const process = session.processes.find((p) => p.terminalId === pane.key);
              const state = !process ? "dormant" : process.exitCode !== null ? `exited ${process.exitCode}` : undefined;
              // A Mac agent does not open again in a session that has a cage now.
              const hasCage = kind.type === "hostAgent" && !!state && !!session.effectiveCage;
              const name = paneName(pane, panes);
              // The actions keep the name: the title of a TUI changes often.
              const liveName = (pane.processId && titles[pane.processId]) || name;
              const agent = kind.type === "agent" || kind.type === "hostAgent";
              const actions: MenuAction[] = [
                ...(state ? [{ label: "Open", icon: <Play size={14} />, onSelect: () => onReopen(pane) }] : []),
                ...(agent
                  ? [{ label: "Continue last session", icon: <History size={14} />, onSelect: () => onNewSession({ ...kind, resume: true }) }]
                  : []),
                { label: "Move left", icon: <ArrowLeft size={14} />, disabled: index === 0, onSelect: () => move(pane.key, index - 1) },
                {
                  label: "Move right",
                  icon: <ArrowRight size={14} />,
                  disabled: index === panes.length - 1,
                  onSelect: () => move(pane.key, index + 1),
                },
                { label: "Close", icon: <X size={14} />, onSelect: () => onClose(pane.key) },
              ];
              return (
                <SortableTab
                  key={pane.key}
                  id={pane.key}
                  onMenu={() => setMenu(pane.key)}
                >
                  <Button
                    size="sm"
                    variant="ghost"
                    iconLeft={<KindIcon kind={kind} />}
                    aria-current={pane.key === selected || undefined}
                    title={hasCage ? `${name}: this session has a cage, so an agent with no cage does not open in it.` : liveName}
                    onClick={() => onShow(pane.key)}
                  >
                    <span className="terminal-tab-name project-mark">{liveName}</span>
                    {state && <span className={process?.exitCode ? "dot error" : "dot"} role="img" aria-label={state} title={state} />}
                    {hasCage && <span className="danger"> · the session has a cage</span>}
                  </Button>
                  <AgentStatusGlyph state={process?.exitCode === null ? process.agent : null} />
                  <ActionMenu
                    open={menu === pane.key}
                    onOpenChange={(open) => setMenu(open ? pane.key : undefined)}
                    trigger={<IconButton size="sm" className="terminal-tab-menu" icon={<Ellipsis size={12} />} label={`Actions for ${name}`} />}
                    actions={actions}
                  />
                </SortableTab>
              );
            })}
          </ul>
        </SortableContext>
      </DndContext>
      <NewMenu size="sm" variant="ghost" iconLeft={<Plus size={14} />} label="New" items={newItems} />
    </div>
  );
}

function SortableTab({ id, onMenu, children }: { id: string; onMenu: () => void; children: ReactNode }) {
  const sortable = useSortable({ id });
  return (
    <li
      ref={sortable.setNodeRef}
      className={sortable.isDragging ? "terminal-tab-item dragging" : "terminal-tab-item"}
      style={{ transform: CSS.Translate.toString(sortable.transform), transition: sortable.transition }}
      onPointerDown={(event) => sortable.listeners?.onPointerDown?.(event)}
      onContextMenu={(event) => {
        event.preventDefault();
        onMenu();
      }}
    >
      {children}
    </li>
  );
}
