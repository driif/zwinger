import { Button, Card } from "@driif/motiv";
import { ChevronRight } from "lucide-react";
import { KeyboardEvent, ReactNode, useEffect, useRef, useState } from "react";
import "./actionMenu.css";

/** A menu action. `confirm` asks first. `reason` shows why a disabled action cannot run. `submenu` opens more actions. */
export type MenuAction = {
  label: string;
  icon: ReactNode;
  disabled?: boolean;
  reason?: string;
} & ({ run: () => void; confirm?: string } | { submenu: MenuAction[] });

type RunAction = Extract<MenuAction, { run: () => void }>;

/** True for a target in the menu or on its open opener. The opener is in the element before the menu. */
function inside(menu: HTMLElement | null, target: Element) {
  const opener = target.closest("[aria-haspopup=menu][aria-expanded=true]");
  return menu?.contains(target) || (opener !== null && menu?.previousElementSibling?.contains(opener));
}

/**
 * A popover of actions below its positioned parent. Arrow keys move between them. Escape, a click outside and focus that
 * moves outside close it. The opener does not count as outside: the opener closes the menu itself.
 * `onClose(true)` asks the caller to focus its opener again.
 */
export function ActionMenu({ label, actions, onClose }: { label: string; actions: MenuAction[]; onClose: (refocus: boolean) => void }) {
  const card = useRef<HTMLElement>(null);
  const [step, setStep] = useState<RunAction>();
  const [sub, setSub] = useState<MenuAction[]>();
  const close = useRef(onClose);
  close.current = onClose;

  useEffect(() => {
    card.current?.querySelector<HTMLElement>("button:not(:disabled)")?.focus();
  }, [step, sub]);

  useEffect(() => {
    const outside = (event: Event) => {
      if (event.target instanceof Element && !inside(card.current, event.target)) close.current(false);
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("focusin", outside);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("focusin", outside);
    };
  }, []);

  function onKeyDown(event: KeyboardEvent) {
    // A tree and dnd-kit around the menu do not see its keys.
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
    if ("submenu" in action) return setSub(action.submenu);
    if (action.confirm) return setStep(action);
    onClose(true);
    action.run();
  }

  return (
    <Card ref={card} padding="sm" className="action-menu" tabIndex={-1} onKeyDown={onKeyDown}>
      {!step ? (
        <div role="menu" aria-label={label} className="stack">
          {(sub ?? actions).map((action, i) => (
            <Button
              key={i}
              role="menuitem"
              size="sm"
              variant="ghost"
              iconLeft={action.icon}
              iconRight={"submenu" in action && <ChevronRight size={14} />}
              aria-haspopup={"submenu" in action ? "menu" : undefined}
              disabled={action.disabled}
              onClick={() => choose(action)}
            >
              {action.label}
              {action.reason && <span className="muted"> · {action.reason}</span>}
            </Button>
          ))}
        </div>
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
