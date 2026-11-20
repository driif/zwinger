import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import ts from "typescript";

registerHooks({
  resolve(specifier, context, nextResolve) {
    // The module imports its modules with no extension, as Vite allows.
    if (context.parentURL?.endsWith("/features/projects/api.ts") && /^(\.\.?\/)+[^.]*$/.test(specifier)) {
      return nextResolve(`${specifier}.ts`, context);
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    // Transpiled, so its imports of types go.
    if (url.endsWith("/features/projects/api.ts")) {
      const source = readFileSync(fileURLToPath(url), "utf8");
      const options = { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 };
      return { format: "module", source: ts.transpileModule(source, { compilerOptions: options }).outputText, shortCircuit: true };
    }
    return nextLoad(url, context);
  },
});

/** The order of the listener and the pulls. */
const steps: string[] = [];
const handlers = new Map<number, () => void>();
Object.assign(globalThis, {
  window: {
    __TAURI_INTERNALS__: {
      transformCallback: (callback: () => void) => {
        handlers.set(handlers.size, callback);
        return handlers.size - 1;
      },
      invoke: async (command: string, args: { event?: string }) => {
        steps.push(`${command} ${args.event ?? ""}`.trim());
        return 7;
      },
    },
  },
});
const { onAppNotices } = await import("./api.ts");

test("the notices are pulled once the listener is on, and at each signal", async () => {
  const unlisten = await onAppNotices(() => steps.push("pull"));
  assert.deepEqual(steps, ["plugin:event|listen app-notices", "pull"]);
  handlers.get(0)!();
  handlers.get(0)!();
  assert.deepEqual(steps.slice(2), ["pull", "pull"]);
  assert.equal(typeof unlisten, "function");
});
