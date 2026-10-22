import { CommandMenu, CommandMenuItem } from "@driif/motiv";
import { Ref } from "react";
import { startApp, stopApp } from "../features/apps/api";
import {
  removeRule,
  setNetworkDefault,
  setOffline,
} from "../features/network/api";
import { removeMount, setMountReadonly } from "../features/settings/api";
import { ShieldView } from "./api";
import type { Intent, Pane, Run, View } from "../App";
import { paneName } from "../features/shields/ShieldRail";
import { shortcutLabel, ShortcutAction } from "./useShortcuts";

interface Props {
  ref: Ref<HTMLDivElement>;
  shields: ShieldView[];
  shield?: ShieldView;
  panes: Pane[];
  /** The focused pane of the shield. */
  focused?: string;
  actions: Record<ShortcutAction, () => void>;
  onSelect: (id: string) => void;
  onShowPane: (key: string) => void;
  onView: (view: View) => void;
  onToggle: (shield: ShieldView) => void;
  onRestart: (id: string) => void;
  onIntent: (kind: Intent["kind"]) => void;
  onOpenRound: () => void;
  run: Run;
}

export function CommandBar({
  ref,
  shields,
  shield,
  panes,
  focused,
  actions,
  onSelect,
  onShowPane,
  onView,
  onToggle,
  onRestart,
  onIntent,
  onOpenRound,
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
  if (shield) {
    const { state } = shield.status;
    if (state === "running" || state === "stopped" || state === "error")
      commands.push({
        id: "toggle",
        label: state === "running" ? `Stop ${shield.name}` : `Start ${shield.name}`,
        onSelect: () => onToggle(shield),
      });
    if (shield.pendingRestart)
      commands.push({ id: "restart", label: `Restart ${shield.name}`, onSelect: () => onRestart(shield.id) });
    commands.push(action("newAgent", "New agent"), action("newShell", "New Mac shell"));
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
      intent("rename", "Rename shield…"),
      intent("mount", "Add mount…"),
      intent("harness", "Set harness…"),
      intent("delete", "Delete shield…"),
    );
    const { id, network } = shield;
    for (const app of shield.apps) {
      const running = shield.appRuns.some((r) => r.appId === app.id && r.exitCode === null);
      commands.push({ id: `app-${app.id}`, label: `Show app ${app.name}`, onSelect: () => onView({ kind: "app", appId: app.id }) });
      if (state === "running")
        commands.push({
          id: `app-run-${app.id}`,
          label: running ? `Stop app ${app.name}` : `Start app ${app.name}`,
          onSelect: () => run(() => (running ? stopApp(id, app.id) : startApp(id, app.id))),
        });
    }
    commands.push(intent("app", "Add app…"));
    commands.push({ id: "new-task", label: "New task…", onSelect: () => onView({ kind: "taskEdit" }) });
    for (const task of shield.tasks)
      commands.push({ id: `task-${task.id}`, label: `Show task ${task.title}`, onSelect: () => onView({ kind: "task", taskId: task.id }) });
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
      commands.push({ id: `rule-${rule.id}`, label: `Remove network rule ${label}`, onSelect: () => run(() => removeRule(id, rule.id)) });
    }
    for (const mount of shield.mounts) {
      commands.push({
        id: `access-${mount.guest}`,
        label: `Make ${mount.guest} ${mount.readonly ? "read-write" : "read-only"}`,
        onSelect: () => run(() => setMountReadonly(id, mount.guest, !mount.readonly)),
      });
      commands.push({ id: `unmount-${mount.guest}`, label: `Remove mount ${mount.guest}`, onSelect: () => run(() => removeMount(id, mount.guest)) });
    }
  }
  commands.push(
    action("newShield", "New shield"),
    { id: "open-round", label: "Open round folder…", onSelect: onOpenRound },
    action("appSettings", "Settings: agents on this Mac, updates, storage"),
  );
  for (const other of shields)
    if (other.id !== shield?.id)
      commands.push({ id: `shield-${other.id}`, label: `Switch to ${other.name}`, onSelect: () => onSelect(other.id) });

  return <CommandMenu ref={ref} className="command-bar" hotkey placeholder="Actions" commands={commands} />;
}
