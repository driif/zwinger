import { call } from "../../shared/api";

/** A cage with no mounts, rules or agents. Resolves to its id. */
export const createCage = (name: string) => call<string>("create_cage", { name });

export const renameCage = (cageId: string, name: string) => call<void>("rename_cage", { cageId, name });

/** Only a cage that no session or workspace uses and whose VM does not run. Also deletes its secret values and state. */
export const deleteCage = (cageId: string) => call<void>("delete_cage", { cageId });
