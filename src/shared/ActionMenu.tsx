import { Button, Card, FloatingField } from "@driif/motiv";
import { FormEvent, KeyboardEvent, ReactNode, useEffect, useRef, useState } from "react";
import "./actionMenu.css";

/** A menu action. `confirm` asks first; `rename` asks for a new name first. */
export type MenuAction = { label: string; icon: ReactNode; disabled?: boolean } & (
  | { run: () => void; confirm?: string }
  | { rename: string; onRename: (name: string) => void }
);

/**
 * A popover of actions below its positioned parent. Arrow keys move between them; Escape and a click outside close it.
 * `onClose(true)` asks the caller to focus its opener again.
 */
export function ActionMenu({ label, actions, onClose }: { label: string; actions: MenuAction[]; onClose: (refocus: boolean) => void }) {
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
    <Card ref={card} padding="sm" className="action-menu" onKeyDown={onKeyDown}>
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
