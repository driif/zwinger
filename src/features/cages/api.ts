import { call, type PublishedPort } from "../../shared/api";
import type { HostCapacity } from "./hostCapacity";

/** A cage with no mounts, rules or agents. Resolves to its id. */
export const createCage = (name: string) => call<string>("create_cage", { name });

/** A copy with the mounts, network rules and agents of the cage, but no credential values. Resolves to its id. */
export const duplicateCage = (cageId: string, name: string) => call<string>("duplicate_cage", { cageId, name });

/** The name of a copy of the cage, within the 64-character limit. */
export const copyName = (name: string) => `${name.slice(0, 59)} copy`;

export const renameCage = (cageId: string, name: string) => call<void>("rename_cage", { cageId, name });

/** Only a cage that no session or workspace uses and whose VM does not run. Also deletes its secret values and state. */
export const deleteCage = (cageId: string) => call<void>("delete_cage", { cageId });

/** Applies on the next start of each VM of the cage. */
export const setCageResources = (cageId: string, cpus: number, memoryMib: number) =>
  call<void>("set_cage_resources", { cageId, cpus, memoryMib });

/** Replaces the published ports of the cage. Applies on the next start of each VM of the cage. */
export const setPublishedPorts = (cageId: string, ports: PublishedPort[]) =>
  call<void>("set_published_ports", { cageId, ports });

/** `null` if zwinger cannot read it. */
export const hostCapacity = () => call<HostCapacity | null>("host_capacity");

/** The CPU and memory use of a running VM. A `cpuPercent` of 100 is one CPU fully in use. */
export interface VmUsage {
  cpuPercent: number;
  memoryBytes: number;
  memoryLimitBytes: number;
}

/** The use of each running VM of the cage, by VM key. A VM with no sample yet is not in it. */
export const vmUsage = (cageId: string) => call<Record<string, VmUsage>>("vm_usage", { cageId });
