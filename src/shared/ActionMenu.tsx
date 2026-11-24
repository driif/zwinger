import { Confirm } from "@driif/motiv";
import { ActionMenu as Menu, ActionMenuItem, ActionMenuProps } from "@driif/motiv/action-menu";
import { ReactElement, useId, useState } from "react";

/** A menu item. `confirm` asks this question before the item runs. */
export type MenuAction = ActionMenuItem & { confirm?: string };

type Step = { label: string; question: string; run: () => void };

/**
 * The motiv menu of `actions`, opened by `trigger`, with a confirmation for each action that has `confirm`.
 * Focus goes back to the trigger, or to `returnFocus` when the caller has a better place.
 */
export function ActionMenu({
  actions,
  returnFocus,
  ...props
}: Omit<ActionMenuProps, "items" | "onCloseAutoFocus" | "id"> & {
  trigger: ReactElement;
  actions: MenuAction[];
  returnFocus?: () => void;
}) {
  const id = useId();
  const [step, setStep] = useState<Step>();
  const refocus = returnFocus ?? (() => document.getElementById(id)?.focus());
  const items = (list: MenuAction[]): ActionMenuItem[] =>
    list.map(({ confirm, ...item }) =>
      "submenu" in item
        ? { ...item, submenu: items(item.submenu) }
        : confirm
          ? { ...item, onSelect: () => setStep({ label: item.label, question: confirm, run: item.onSelect }) }
          : item,
    );

  return (
    <>
      <Menu
        {...props}
        id={id}
        items={items(actions)}
        onCloseAutoFocus={
          returnFocus &&
          ((event) => {
            event.preventDefault();
            returnFocus();
          })
        }
      />
      <Confirm
        open={!!step}
        title={step?.question ?? ""}
        confirmLabel={step?.label}
        onCancel={() => setStep(undefined)}
        onConfirm={() => {
          setStep(undefined);
          step!.run();
        }}
        onClose={refocus}
      />
    </>
  );
}
