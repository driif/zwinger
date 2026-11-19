import { Splitter } from "@driif/motiv";

const MIN = 200;
const MAX = 480;
const DEFAULT = 256;

export const clampRail = (width: number) => Math.min(MAX, Math.max(MIN, Math.round(width)));

interface Props {
  width: number;
  onResize: (width: number) => void;
  /** Persists the completed resize or reset. */
  onDone: (width: number) => void;
}

export function RailHandle({ width, onResize, onDone }: Props) {
  return (
    <Splitter
      className="rail-handle"
      label="Resize the project list"
      min={MIN}
      max={MAX}
      value={width}
      aria-valuetext={`${width} pixels`}
      onChange={onResize}
      onCommit={onDone}
      onDoubleClick={() => {
        onResize(DEFAULT);
        onDone(DEFAULT);
      }}
    />
  );
}
