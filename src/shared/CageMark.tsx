import { useId } from "react";

const Z = "M9.5 9.5h5l-5 6h5";

/** The zwinger mark, like the app icon: a cage in the primary color with a Z in the secondary color. */
export function CageMark({ size }: { size: number }) {
  // The mask cuts a gap in the cage around the Z, so the mark reads on any background.
  const mask = useId();
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <mask id={mask} maskUnits="userSpaceOnUse" x="0" y="0" width="24" height="24">
        <rect width="24" height="24" fill="white" />
        <path d={Z} stroke="black" strokeWidth={5} />
      </mask>
      <g stroke="var(--motiv-color-primary)" mask={`url(#${mask})`}>
        <circle cx="12" cy="2.5" r="1.5" />
        <path d="M5 20v-9a7 7 0 0 1 14 0v9" />
        <path d="M9 4.7V20M12 4v16M15 4.7V20" />
        <path d="M3 20h18" />
      </g>
      <path d={Z} stroke="var(--motiv-color-secondary)" strokeWidth={2.5} />
    </svg>
  );
}
