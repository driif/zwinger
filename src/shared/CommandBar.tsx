import { CommandMenu, CommandMenuItem } from "@driif/motiv";
import {
  setNetworkDefault,
  setOffline,
} from "../features/network/api";
import { setMountReadonly } from "../features/settings/api";
import { SessionCage, SessionView } from "./api";
import type { Intent, Pane, Run, View } from "../App";
import { paneName } from "../features/projects/ProjectRail";
import { editorLabel } from "../features/terminal/folders";
import { isDefaultEditor, LaunchAction, launchActions, projectEditors, projectLaunchCommand } from "../features/launcher/actions";
import { Editors } from "../features/launcher/api";
import type { Detection } from "../features/settings/api";
import { shortcutLabel, ShortcutAction } from "./useShortcuts";
import { removeMountWithUndo, removeRuleWithUndo } from "./undo";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  sessions: SessionView[];
  /** The selected session and its cage. */
  selected?: SessionCage;
  panes: Pane[];
  /** The focused pane of the session. */
  focused?: string;
  /** The focused pane when the terminal view shows; the editor items open its folder. */
  here?: string;
  /** The home folder, shown as `~`. */
  home?: string;
  actions: Record<ShortcutAction, () => void>;
  onSelect: (id: string) => void;
  onShowPane: (key: string) => void;
  onView: (view: View) => void;
  onToggle: (target: SessionCage) => void;
  /** Restarts the cage of the session. */
  onRestart: (sessionId: string) => void;
  onIntent: (kind: Intent["kind"]) => void;
  onOpenRound: () => void;
  editors?: Editors;
  detection?: Detection;
  onLaunch: (action: LaunchAction) => void;
  run: Run;
}

export function CommandBar({
  open,
  onOpenChange,
  sessions,
  selected,
  panes,
  focused,
  here,
  home,
  actions,
  onSelect,
  onShowPane,
  onView,
  onToggle,
  onRestart,
  onIntent,
  onOpenRound,
  editors,
  detection,
  onLaunch,
  run,
}: Props) {
  const action = (id: ShortcutAction, label: string): CommandMenuItem => ({
    id,
    label,
    shortcut: shortcutLabel(id),
    onSelect: actions[id],
  });
  const intent = (kind: Intent["kind"], label: string): CommandMenuItem => ({
    id: kind,
    label,
    onSelect: () => onIntent(kind),
  });

  const commands: CommandMenuItem[] = [];
  if (selected) {
    const { session, cage, vmCage } = selected;
    const state = vmCage?.status.state ?? (cage ? "stopped" : undefined);
    if (state === "running" || state === "stopped" || state === "error")
      commands.push({
        id: "toggle",
        label: state === "running" ? `Stop ${session.name} cage` : `Start ${session.name} cage`,
        onSelect: () => onToggle(selected),
      });
    if (cage && (vmCage?.pendingRestart || (state === "running" && cage.id !== vmCage?.id)))
      commands.push({ id: "restart", label: `Restart ${session.name} cage`, onSelect: () => onRestart(session.id) });
    commands.push(action("newAgent", "New agent"));
    const herePane = panes.find((p) => p.key === here);
    const ownEditors = projectEditors(editors, session);
    for (const launch of launchActions(selected, detection, ownEditors)) {
      const shortcut =
        launch.type === "session" && (launch.kind.type === "host" || launch.kind.type === "shell")
          ? shortcutLabel(launch.kind.type === "host" ? "newShell" : "newVmShell")
          : isDefaultEditor(launch, ownEditors)
            ? shortcutLabel("openEditor")
            : undefined;
      const label =
        launch.type !== "editor"
          ? launch.label
          : herePane
            ? editorLabel(launch.editor.name, herePane.folder, session.folder, home)
            : `Open ${session.name} in ${launch.editor.name}`;
      commands.push(projectLaunchCommand(launch, label, shortcut, ownEditors, onLaunch, actions.openEditor));
    }
    panes.forEach((pane) =>
      commands.push({
        id: `pane-${pane.key}`,
        label: `Show ${paneName(pane, panes)}`,
        onSelect: () => onShowPane(pane.key),
      }),
    );
    if (focused)
      commands.push(
        action("close", "Hide pane"),
        action("splitRight", "Split right with a Mac shell"),
        action("splitDown", "Split down with a Mac shell"),
      );
    if (panes.length > 1) commands.push(action("next", "Next session"), action("previous", "Previous session"));
    commands.push(
      action("overview", "Overview"),
      action("agents", "Agent"),
      action("terminal", "Terminal"),
      action("network", "Network"),
      action("settings", "Settings"),
      intent("addRule", "Add a network rule…"),
      intent("rename", "Rename project…"),
      intent("mount", "Add mount…"),
      intent("harness", "Set harness…"),
      intent("delete", "Delete project…"),
    );
    commands.push({ id: "new-task", label: "New task…", onSelect: () => onView({ kind: "taskEdit" }) });
    for (const task of session.tasks)
      commands.push({ id: `task-${task.id}`, label: `Show task ${task.title}`, onSelect: () => onView({ kind: "task", taskId: task.id }) });
  }
  if (selected?.cage) {
    const { id, network, mounts } = selected.cage;
    const other = network.default === "allow" ? "block" : "allow";
    commands.push(
      {
        id: "network-default",
        label: `Network: ${other} everything else`,
        onSelect: () => run(() => setNetworkDefault(id, other)),
      },
      {
        id: "network-offline",
        label: network.offline ? "Network: go online" : "Network: go offline",
        onSelect: () => run(() => setOffline(id, !network.offline)),
      },
    );
    for (const rule of network.rules) {
      const label = rule.target.kind === "host" ? rule.target.host : rule.target.id;
      commands.push({ id: `rule-${rule.id}`, label: `Remove network rule ${label}`, onSelect: () => removeRuleWithUndo(run, id, rule.id, label) });
    }
    for (const mount of mounts) {
      commands.push({
        id: `access-${mount.guest}`,
        label: `Make ${mount.guest} ${mount.readonly ? "read-write" : "read-only"}`,
        onSelect: () => run(() => setMountReadonly(id, mount.guest, !mount.readonly)),
      });
      commands.push({ id: `unmount-${mount.guest}`, label: `Remove mount ${mount.guest}`, onSelect: () => removeMountWithUndo(run, id, mount.guest) });
    }
  }
  commands.push(
    action("newProject", "New project"),
    { id: "open-round", label: "Open round folder…", onSelect: onOpenRound },
    action("appSettings", "Settings: agents on this Mac, updates, storage"),
  );
  for (const other of sessions)
    if (other.id !== selected?.session.id)
      commands.push({ id: `project-${other.id}`, label: `Switch to ${other.name}`, onSelect: () => onSelect(other.id) });

  return <CommandMenu open={open} onOpenChange={onOpenChange} className="command-bar" hotkey placeholder="Actions" commands={commands} />;
}
