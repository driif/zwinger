import type { ActionMenuItem } from "@driif/motiv/action-menu";
import { Box, Copy, FolderOpen, History, Laptop, ShieldOff } from "lucide-react";
import { AGENT_ICONS } from "../tasks/roomParts";
import type { LaunchAction } from "./actions";
import { EditorIcon } from "./EditorIcon";

function icon(action: LaunchAction) {
  if (action.type === "editor") return <EditorIcon icon={action.editor.icon} />;
  if (action.type === "copyPath") return <Copy size={14} />;
  if (action.type === "reveal") return <FolderOpen size={14} />;
  const { kind } = action;
  if (kind.type === "host") return <Laptop size={14} />;
  if (kind.type === "shell") return <Box size={14} />;
  if (kind.resume) return <History size={14} />;
  if (kind.type === "hostAgent") return <ShieldOff size={14} />;
  const Icon = AGENT_ICONS[kind.agent];
  return <Icon size={14} />;
}

/** The launch actions as menu items. */
export const launchItems = (actions: LaunchAction[], onLaunch: (action: LaunchAction) => void): ActionMenuItem[] =>
  actions.map((action) => ({
    label: action.label,
    icon: icon(action),
    disabled: !!action.disabled,
    disabledReason: action.disabled,
    onSelect: () => onLaunch(action),
  }));
