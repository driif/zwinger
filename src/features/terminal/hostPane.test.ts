import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { registerHooks } from "node:module";
import test from "node:test";
import { fileURLToPath } from "node:url";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import ts from "typescript";

Object.assign(globalThis, { paneTestReact: React });
const virtual = (source: string) => `data:text/javascript,${encodeURIComponent(source)}`;
const icon = "export const Code=()=>null, Columns2=()=>null, Ellipsis=()=>null, Play=()=>null, Rows2=()=>null, X=()=>null;";
const motiv = `const div=({children})=>globalThis.paneTestReact.createElement('div',null,children);
  export const Badge=div, Dialog=div, IconButton=div, IconButtonGroup=div;
  export const Button=({children})=>globalThis.paneTestReact.createElement('button',null,children);`;
const stubs: Record<string, string> = {
  "@driif/motiv": motiv,
  "lucide-react": icon,
  "../projects/ProjectRail": "export const KindIcon=()=>null; export const paneName=()=>'';",
  "../projects/StartProgress": "export const StartProgress=()=>null;",
  "./panes": "export const place=()=>({panes:[],splits:[]}); export const setRatio=(x)=>x;",
  "./folders": "export const folderLabel=()=>'';",
  "./titles": "export const useTerminalTitles=()=>({});",
  "./TerminalView": "export const TerminalView=({processId})=>globalThis.paneTestReact.createElement('div',{'data-live':processId});",
  "./HostHistory": "export const HostHistory=()=>globalThis.paneTestReact.createElement('div',{'data-history':true});",
  "./api": "export const terminalHistory=()=>Promise.resolve(null);",
};

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (context.parentURL?.endsWith("/PaneLayout.tsx") && stubs[specifier]) {
      return { url: virtual(stubs[specifier]), shortCircuit: true };
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    if (url.endsWith("/PaneLayout.tsx")) {
      const source = readFileSync(fileURLToPath(url), "utf8");
      return {
        format: "module",
        source: ts.transpileModule(source, { compilerOptions: {
          module: ts.ModuleKind.ESNext,
          target: ts.ScriptTarget.ES2022,
          jsx: ts.JsxEmit.ReactJSX,
        } }).outputText,
        shortCircuit: true,
      };
    }
    return nextLoad(url, context);
  },
});

const { HostPaneBody } = await import("./PaneLayout.tsx");
const pane = { key: "t1", sessionId: "s1", kind: { type: "host" as const }, exitCode: null, folder: null };
const props = {
  pane,
  name: "Mac shell 1",
  rect: { left: 0, top: 0, width: 1, height: 1 },
  visible: true,
  focused: true,
  focusRequest: 0,
  onFocus() {},
  onPaneExit() {},
  onPaneError() {},
  onReopen() {},
};

test("a dormant Mac shell without history keeps its card", () => {
  const html = renderToStaticMarkup(React.createElement(HostPaneBody, { ...props, runs: null }));
  assert.match(html, /Mac shell 1 is not running/);
  assert.doesNotMatch(html, /data-history/);
});

test("a live Mac shell mounts while history is still loading", () => {
  const html = renderToStaticMarkup(React.createElement(HostPaneBody, { ...props, pane: { ...pane, processId: "p1" } }));
  assert.match(html, /data-live="p1"/);
});

test("saved history is above the live shell and a dormant footer", () => {
  const live = renderToStaticMarkup(React.createElement(HostPaneBody, { ...props, pane: { ...pane, processId: "p1" }, runs: [[65], [66]] }));
  assert.ok(live.indexOf("data-history") < live.indexOf("data-live"));
  assert.equal(live.match(/data-history/g)?.length, 2);
  const dormant = renderToStaticMarkup(React.createElement(HostPaneBody, { ...props, runs: [[65]] }));
  assert.match(dormant, /data-history/);
  assert.match(dormant, /Not running/);
});
