import type { CageView, SessionCage, SessionView } from "../../shared/api.ts";

/** The effective cage of the session, for config edits, and the cage of its VM, for the VM state. */
export function sessionCage(session: SessionView, cages: CageView[]): SessionCage {
  const cage = cages.find((c) => c.id === session.effectiveCage?.id) ?? null;
  const vmCage = cages.find((c) => c.id === session.vmCage) ?? null;
  return { session, cage, vmCage };
}

/** The cage whose agents a new terminal can use: the running VM, else the effective cage. */
export const launchCage = ({ cage, vmCage }: SessionCage) => (vmCage?.status.state === "running" ? vmCage : cage);
