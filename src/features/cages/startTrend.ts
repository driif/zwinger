import type { StartRecord } from "../../shared/api.ts";

export type Direction = "slower" | "faster" | "same";

/** The change from the start before. Equal totals are `same`. */
export function direction(before: number, total: number): Direction {
  if (total > before) return "slower";
  return total < before ? "faster" : "same";
}

/** A duration with its unit, at the recorded ms precision: ms below one second, else seconds. */
export function duration(ms: number): string {
  return ms < 1000 ? `${ms} ms` : `${ms / 1000} s`;
}

/** The latest total and its change from the start before. `history` is oldest first and not empty. */
export function trendSummary(history: StartRecord[]): string {
  const latest = history[history.length - 1].totalMs;
  if (history.length === 1) return `Latest ${duration(latest)}. There is no start before it.`;
  const before = history[history.length - 2].totalMs;
  const change = direction(before, latest);
  if (change === "same") return `Latest ${duration(latest)}, the same as the start before`;
  return `Latest ${duration(latest)}, ${duration(Math.abs(latest - before))} ${change} than the start before`;
}
