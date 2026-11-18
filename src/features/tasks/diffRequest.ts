/** Keeps only the newest request current. `drop()` makes all open requests stale. */
export function latestRequest() {
  let latest = 0;
  return {
    next() {
      const request = ++latest;
      return () => request === latest;
    },
    drop() {
      latest++;
    },
  };
}
