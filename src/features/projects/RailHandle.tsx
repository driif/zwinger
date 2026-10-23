import { PointerEvent, useRef, useState } from "react";

const MIN = 200;
const MAX = 480;
const DEFAULT = 256;

export const clampRail = (width: number) => Math.min(MAX, Math.max(MIN, Math.round(width)));

interface Props {
  width: number;
  /** Each step of a drag. */
  onResize: (width: number) => void;
  /** The end of a drag, or a reset. */
  onDone: (width: number) => void;
}

/** Drags the right edge of the rail. A double-click resets the width. */
export function RailHandle({ width, onResize, onDone }: Props) {
  const start = useRef<{ x: number; width: number }>(undefined);
  const [dragging, setDragging] = useState(false);
  const at = (event: PointerEvent) => clampRail(start.current!.width + event.clientX - start.current!.x);

  return (
    <div
      className={dragging ? "rail-handle dragging" : "rail-handle"}
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize the project list"
      aria-valuenow={width}
      aria-valuemin={MIN}
      aria-valuemax={MAX}
      onPointerDown={(event) => {
        event.currentTarget.setPointerCapture(event.pointerId);
        start.current = { x: event.clientX, width };
        setDragging(true);
      }}
      onPointerMove={(event) => dragging && onResize(at(event))}
      onPointerUp={(event) => {
        if (!dragging) return;
        setDragging(false);
        onDone(at(event));
      }}
      onPointerCancel={() => setDragging(false)}
      onDoubleClick={() => {
        onResize(DEFAULT);
        onDone(DEFAULT);
      }}
    />
  );
}
