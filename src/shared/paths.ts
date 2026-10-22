/** `path` with `~` for the home folder. */
export function tildePath(path: string, home: string | undefined) {
  const root = home?.replace(/\/+$/, "");
  if (!root || (path !== root && !path.startsWith(`${root}/`))) return path;
  return `~${path.slice(root.length)}`;
}

/** At most `max` characters: a longer text keeps its start and end, with `…` between. */
export function truncateMiddle(text: string, max: number) {
  if (text.length <= max) return text;
  const tail = Math.floor((max - 1) / 2);
  return `${text.slice(0, max - 1 - tail)}…${text.slice(text.length - tail)}`;
}
