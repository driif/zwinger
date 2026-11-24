/** The logical CPUs and the physical memory of this Mac. */
export interface HostCapacity {
  cpus: number;
  memoryMib: number;
}

/** macOS and the app need this much memory next to a VM. */
export const HOST_RESERVE_MIB = 2048;

export function capacityText({ cpus, memoryMib }: HostCapacity) {
  return `This Mac: ${cpus} CPUs, ${Math.round(memoryMib / 1024)} GiB`;
}

/** Why the values can prevent a VM start on this Mac, or `undefined`. */
export function resourceWarning(cpus: number, memoryMib: number, host: HostCapacity) {
  const problems = [];
  if (cpus > host.cpus) problems.push(`more CPUs than this Mac has (${host.cpus})`);
  const safe = host.memoryMib - HOST_RESERVE_MIB;
  if (memoryMib > safe) problems.push(`more memory than this Mac can give a VM (${Math.max(safe, 0)} MiB)`);
  if (problems.length === 0) return undefined;
  return `The VM can fail to start: ${problems.join(" and ")}.`;
}
