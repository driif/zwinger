import { Button, Card } from "@driif/motiv";
import { KeyboardEvent, ReactNode, useRef } from "react";
import { Plus } from "lucide-react";
import { ShieldView } from "./api";

interface Props {
  shields: ShieldView[];
  selectedId?: string;
  /** Per shield, the blocked events that the Network tab has not shown yet. */
  blocked: Record<string, number>;
  onSelect: (id: string) => void;
  onNew: () => void;
  children: ReactNode;
}

export function ShieldRail({ shields, selectedId, blocked, onSelect, onNew, children }: Props) {
  const rows = useRef<HTMLElement[]>([]);

  // Roving tabindex: the selected row is in the tab order, the arrow keys move between rows.
  function onKeyDown(event: KeyboardEvent, index: number) {
    const step = { ArrowDown: 1, ArrowUp: -1 }[event.key];
    if (step !== undefined) {
      event.preventDefault();
      rows.current[(index + step + shields.length) % shields.length]?.focus();
    } else if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      onSelect(shields[index].id);
    }
  }

  return (
    <aside className="rail">
      {children}
      <ul className="rail-list" aria-label="Shields">
        {shields.map((shield, index) => (
          <li key={shield.id}>
            <Card
              variant={shield.id === selectedId ? "outlined" : "subtle"}
              padding="sm"
              interactive
              role="button"
              aria-current={shield.id === selectedId}
              tabIndex={shield.id === selectedId ? 0 : -1}
              title={shield.status.state === "error" ? shield.status.message : shield.folder}
              ref={(el: HTMLElement | null) => {
                if (el) rows.current[index] = el;
              }}
              onClick={() => onSelect(shield.id)}
              onKeyDown={(event) => onKeyDown(event, index)}
            >
              <div className="row">
                <span className={`dot ${shield.status.state}`} aria-label={shield.status.state} />
                <span className="grow">{shield.name}</span>
                {blocked[shield.id] > 0 && (
                  <span className="rail-blocked" title="Blocked network events since you last saw the Network tab">
                    {blocked[shield.id]}
                  </span>
                )}
              </div>
              <p className="rail-counts muted">
                {shield.mounts.length + 1} folders ·{" "}
                {shield.network.mode === "public"
                  ? "public internet"
                  : shield.network.mode === "deny"
                    ? "no egress"
                    : `${shield.network.domains.length} hosts`}
              </p>
            </Card>
          </li>
        ))}
      </ul>
      <Button variant="ghost" size="sm" iconLeft={<Plus size={14} />} onClick={onNew}>
        New shield
      </Button>
    </aside>
  );
}
