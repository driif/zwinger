import { Button, IconButton } from "@driif/motiv";
import { Box, ChevronDown, Code, Copy, FolderOpen, History, Laptop } from "lucide-react";
import { ReactNode, useRef, useState } from "react";
import { AGENT_ICONS } from "../tasks/roomParts";
import { ActionMenu, MenuAction } from "../../shared/ActionMenu";
import type { LaunchAction } from "./actions";
import "./launcher.css";

function icon(action: LaunchAction) {
  if (action.type === "editor") return <Code size={14} />;
  if (action.type === "copyPath") return <Copy size={14} />;
  if (action.type === "reveal") return <FolderOpen size={14} />;
  const { kind } = action;
  if (kind.type === "host") return <Laptop size={14} />;
  if (kind.type === "shell") return <Box size={14} />;
  if (kind.resume) return <History size={14} />;
  const Icon = AGENT_ICONS[kind.agent];
  return <Icon size={14} />;
}

/** The launch actions as menu items. */
export const launchMenu = (actions: LaunchAction[], onLaunch: (action: LaunchAction) => void): MenuAction[] =>
  actions.map((action) => ({ label: action.label, icon: icon(action), run: () => onLaunch(action) }));

interface Props {
  label: string;
  icon: ReactNode;
  /** No main action: the button is disabled, and the menu still opens. */
  onClick?: () => void;
  menuLabel: string;
  menu: MenuAction[];
}

/** A button and a chevron that opens a menu of related actions. */
export function SplitLaunch({ label, icon, onClick, menuLabel, menu }: Props) {
  const [open, setOpen] = useState(false);
  const chevron = useRef<HTMLButtonElement>(null);
  return (
    <div className="split-launch">
      <Button size="sm" variant="outline" iconLeft={icon} disabled={!onClick} onClick={onClick}>
        {label}
      </Button>
      <IconButton
        ref={chevron}
        icon={<ChevronDown size={14} />}
        label={menuLabel}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen(true)}
      />
      {open && (
        <ActionMenu
          label={menuLabel}
          actions={menu}
          onClose={(refocus) => {
            setOpen(false);
            if (refocus) chevron.current?.focus();
          }}
        />
      )}
    </div>
  );
}
