import { CommandMenu, CommandMenuItem } from "@driif/motiv";
import { Ref } from "react";
import { NetworkMode, removeDomain, removeMount, setMountReadonly, setNetworkMode, ShieldView } from "./api";
import type { Intent, Pane, Run } from "./App";
import { shortcutLabel, ShortcutAction } from "./useShortcuts";

interface Props {
  ref: Ref<HTMLDivElement>;
  shields: ShieldView[];
  shield?: ShieldView;
  panes: Pane[];
  activePane?: string;
  actions: Record<ShortcutAction, () => void>;
  onSelect: (id: string) => void;
  onShowPane: (key: string) => void;
  onToggle: (shield: ShieldView) => void;
  onRestart: (id: string) => void;
  onIntent: (kind: Intent["kind"]) => void;
  run: Run;
}

export function CommandBar({
  ref,
  shields,
  shield,
  panes,
  activePane,
  actions,
  onSelect,
  onShowPane,
  onToggle,
  onRestart,
  onIntent,
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
    commands.push(action("newPi", "Open pi agent"), action("newShell", "New shell"));
    panes.forEach((pane, index) =>
      commands.push({
        id: `pane-${pane.key}`,
        label: `Show ${pane.kind} ${index + 1}`,
        onSelect: () => onShowPane(pane.key),
      }),
    );
    if (activePane) commands.push(action("close", "Close session"));
    if (panes.length > 1) commands.push(action("next", "Next session"), action("previous", "Previous session"));
    commands.push(
      action("overview", "Overview"),
      action("agents", "Agents"),
      action("terminal", "Terminal"),
      action("network", "Network"),
      action("settings", "Settings"),
      intent("allowList", "Edit allow list…"),
      intent("rename", "Rename shield…"),
      intent("mount", "Add mount…"),
      intent("harness", "Set harness…"),
      intent("delete", "Delete shield…"),
    );
    const { id, network } = shield;
    const modes: [NetworkMode, string][] = [
      ["deny", "Network: deny all"],
      ["list", "Network: allow list only"],
      ["public", "Network: public internet"],
    ];
    for (const [mode, label] of modes)
      if (network.mode !== mode)
        commands.push({ id: `mode-${mode}`, label, onSelect: () => run(() => setNetworkMode(id, mode)) });
    for (const domain of network.domains)
      commands.push({ id: `domain-${domain}`, label: `Remove host ${domain}`, onSelect: () => run(() => removeDomain(id, domain)) });
    for (const mount of shield.mounts) {
      commands.push({
        id: `access-${mount.guest}`,
        label: `Make ${mount.guest} ${mount.readonly ? "read-write" : "read-only"}`,
        onSelect: () => run(() => setMountReadonly(id, mount.guest, !mount.readonly)),
      });
      commands.push({ id: `unmount-${mount.guest}`, label: `Remove mount ${mount.guest}`, onSelect: () => run(() => removeMount(id, mount.guest)) });
    }
  }
  commands.push(action("newShield", "New shield"));
  for (const other of shields)
    if (other.id !== shield?.id)
      commands.push({ id: `shield-${other.id}`, label: `Switch to ${other.name}`, onSelect: () => onSelect(other.id) });

  return <CommandMenu ref={ref} className="command-bar" hotkey placeholder="Actions" commands={commands} />;
}
