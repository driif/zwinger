import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import ts from "typescript";

/** The props of each rendered Motiv Button, by its text. */
const buttons = new Map<string, { onClick: () => void }>();
Object.assign(globalThis, { exportTestReact: React, exportTestButtons: buttons });
const virtual = (source: string) => `data:text/javascript,${encodeURIComponent(source)}`;
const icons = ["Ban", "Bot", "Box", "Boxes", "Brain", "Check", "Container", "Download", "GitBranch", "Globe", "KeyRound", "Package", "Play", "Sparkles", "X"];
const stubs: Record<string, string> = {
  "lucide-react": icons.map((name) => `export const ${name}=()=>null;`).join(""),
  "@driif/motiv": `const R=globalThis.exportTestReact;
    const div=({children,actions})=>R.createElement('div',null,actions,children);
    export const Card=div, CardBody=div, CardHeader=div, FilterChips=()=>null;
    export const Button=(props)=>{globalThis.exportTestButtons.set(String(props.children),props);return R.createElement('button',null,props.children);};`,
};
const inNetwork = (url?: string) => url?.includes("/src/features/network/") ?? false;

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (inNetwork(context.parentURL) && stubs[specifier]) {
      return { url: virtual(stubs[specifier]), shortCircuit: true };
    }
    // The component imports its modules with no extension, as Vite allows.
    if (context.parentURL?.endsWith("/NetworkActivity.tsx") && /^\.\.?\/[^.]*$/.test(specifier)) {
      return nextResolve(`${specifier}.ts`, context);
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    if (url.endsWith("/NetworkActivity.tsx")) {
      const source = readFileSync(fileURLToPath(url), "utf8");
      return {
        format: "module",
        source: ts.transpileModule(source, {
          compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022, jsx: ts.JsxEmit.ReactJSX },
        }).outputText,
        shortCircuit: true,
      };
    }
    return nextLoad(url, context);
  },
});

const invoked: [string, unknown][] = [];
/** The answer of the backend: a value, or an error to throw. */
let saved: boolean | Error = true;
Object.assign(globalThis, {
  window: {
    dispatchEvent: () => true,
    __TAURI_INTERNALS__: {
      invoke: async (command: string, args: unknown) => {
        invoked.push([command, args]);
        if (saved instanceof Error) throw saved;
        return saved;
      },
    },
  },
});
const { NetworkActivity } = await import("./NetworkActivity.tsx");

const render = (vmKey: string | null, run: (action: () => Promise<unknown>) => Promise<unknown>) => {
  buttons.clear();
  return renderToStaticMarkup(
    React.createElement(NetworkActivity, {
      cage: null,
      vmKey,
      running: false,
      events: [],
      catalog: [],
      onStart: () => {},
      run,
    }),
  );
};

test("the Export button saves the log of the session's VM key through run", async () => {
  const results: unknown[] = [];
  const run = async (action: () => Promise<unknown>) => results.push(await action());
  render("cage-a-2", run);
  const exportButton = buttons.get("Export");
  assert.ok(exportButton);
  exportButton.onClick();
  await new Promise((resolve) => setImmediate(resolve));
  saved = false;
  exportButton.onClick();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(invoked, [
    ["export_net_events", { vmKey: "cage-a-2" }],
    ["export_net_events", { vmKey: "cage-a-2" }],
  ]);
  assert.deepEqual(results, [true, false]);
});

test("a session with no VM key shows no Export button", () => {
  assert.doesNotMatch(render(null, async () => {}), /Export/);
  assert.equal(buttons.has("Export"), false);
});

test("a failed export reaches run as an error, not as a saved file", async () => {
  saved = new Error("The saved network log of cage-a-2 could not be read");
  let failure: unknown;
  const run = (action: () => Promise<unknown>) => action().catch((e) => (failure = e));
  render("cage-a-2", run);
  buttons.get("Export")!.onClick();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(failure, saved);
});
