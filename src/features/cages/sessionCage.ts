import type { CageView, SessionCage, SessionView } from "../../shared/api.ts";

/**
 * The effective cage of the session, for config edits, and its VM with the cage of the VM, for the VM state.
 * The VM is found by its key. A shared VM that starts, runs or failed for other sessions is not the session's VM.
 */
export function sessionCage(session: SessionView, cages: CageView[]): SessionCage {
  const cage = cages.find((c) => c.id === session.effectiveCage?.id) ?? null;
  const ofVm = cages.find((c) => c.vms.some((v) => v.key === session.vmKey));
  const vm = ofVm?.vms.find((v) => v.key === session.vmKey);
  const mine = vm && (session.vmCage !== null || vm.own || vm.status.state === "stopped");
  return { session, cage, vmCage: ofVm && vm && mine ? { ...ofVm, ...vm } : null };
}

/** The cage whose agents a new terminal can use: the running VM, else the effective cage. */
export const launchCage = ({ cage, vmCage }: SessionCage) => (vmCage?.status.state === "running" ? vmCage : cage);
