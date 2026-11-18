import type { SessionView } from "../../shared/api";
import type { Prefs } from "../settings/api";

/** Tracks named editor opens until they finish. Each project override and the app default have separate choices. */
export class PendingEditorChoices {
  private pending = new Map<string | null, { sessionId: string; editorId: string }>();

  begin(project: Pick<SessionView, "id" | "editor">, editorId: string) {
    const scope = project.editor ? project.id : null;
    const choice = { sessionId: project.id, editorId };
    this.pending.set(scope, choice);
    const current = () => this.pending.get(scope) === choice;
    return {
      current,
      finish: () => {
        if (current()) this.pending.delete(scope);
      },
    };
  }

  projectChanged(sessionId: string) {
    for (const [scope, choice] of this.pending) {
      if (choice.sessionId === sessionId) this.pending.delete(scope);
    }
  }

  preferencesChanged(before: Prefs["editors"], after: Prefs["editors"]) {
    for (const [scope, choice] of this.pending) {
      const changedDefault = scope === null && before.default !== after.default;
      const turnedOff = after.enabled !== null && !after.enabled.includes(choice.editorId);
      if (changedDefault || turnedOff) this.pending.delete(scope);
    }
  }
}
