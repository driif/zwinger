import { Button, Card } from "@driif/motiv";
import { X } from "lucide-react";
import { ReactNode } from "react";

interface Props {
  icon: ReactNode;
  children: ReactNode;
  /** Buttons on the right. */
  actions?: ReactNode;
  /** An informational notice can be dismissed. */
  onDismiss?: () => void;
}

/** A compact notice: icon, text, then the actions on the right. */
export function Notice({ icon, children, actions, onDismiss }: Props) {
  return (
    <Card variant="outlined" padding="sm" className="notice" role="status">
      {icon}
      <span className="notice-text" tabIndex={0} ref={(el) => el?.setAttribute("title", el.textContent ?? "")}>
        {children}
      </span>
      {(actions || onDismiss) && (
        <span className="notice-actions row">
          {actions}
          {onDismiss && <Button size="sm" variant="ghost" aria-label="Dismiss" iconLeft={<X size={14} />} onClick={onDismiss} />}
        </span>
      )}
    </Card>
  );
}
