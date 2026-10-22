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
      {/* The text is cut to one line, so the title shows all of it. */}
      <span className="notice-text" ref={(el) => el?.setAttribute("title", el.textContent ?? "")}>
        {children}
      </span>
      <span className="notice-actions row">
        {actions}
        {onDismiss && <Button size="sm" variant="ghost" aria-label="Dismiss" iconLeft={<X size={14} />} onClick={onDismiss} />}
      </span>
    </Card>
  );
}
