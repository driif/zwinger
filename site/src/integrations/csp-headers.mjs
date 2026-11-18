import { createHash } from "node:crypto";
import { readdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { parse } from "parse5";

// Cloudflare ignores header lines above about 2,000 characters.
const MAX_LINE = 2000;
const PLACEHOLDER = "{{inline-script-hashes}}";

// The HTML standard runs an inline script with one of these types. Import maps and
// speculation rules are not JavaScript, but CSP also checks them against script-src.
const JS_MIME = new Set([
  "application/ecmascript",
  "application/javascript",
  "application/x-ecmascript",
  "application/x-javascript",
  "text/ecmascript",
  "text/javascript",
  "text/javascript1.0",
  "text/javascript1.1",
  "text/javascript1.2",
  "text/javascript1.3",
  "text/javascript1.4",
  "text/javascript1.5",
  "text/jscript",
  "text/livescript",
  "text/x-ecmascript",
  "text/x-javascript",
]);
const NON_JS_TYPES = new Set(["module", "importmap", "speculationrules"]);

function isInlineScript(attrs) {
  if (attrs.has("src")) return false;
  let type = attrs.get("type");
  if (type === undefined) {
    const language = attrs.get("language");
    if (!language) return true;
    type = `text/${language}`;
  }
  type = type.trim().toLowerCase();
  return type === "" || JS_MIME.has(type) || NON_JS_TYPES.has(type);
}

/** Returns the CSP hash sources of the inline scripts and the inline event handlers in one HTML document. */
export function scanHtml(html) {
  const hashes = new Set();
  const handlers = [];
  const visit = (node) => {
    if (node.attrs) {
      const attrs = new Map(node.attrs.map((a) => [a.name, a.value]));
      for (const name of attrs.keys()) if (name.startsWith("on")) handlers.push(`<${node.tagName} ${name}>`);
      if (node.tagName === "script" && isInlineScript(attrs)) {
        const body = node.childNodes.map((c) => c.value ?? "").join("");
        hashes.add(`'sha256-${createHash("sha256").update(body, "utf8").digest("base64")}'`);
      }
    }
    for (const child of node.childNodes ?? []) visit(child);
    if (node.content) visit(node.content);
  };
  visit(parse(html));
  return { hashes, handlers };
}

async function htmlFiles(dir) {
  const entries = await readdir(dir, { withFileTypes: true, recursive: true });
  return entries.filter((e) => e.isFile() && e.name.endsWith(".html")).map((e) => join(e.parentPath, e.name));
}

/** Replaces the hash placeholder of the CSP in `dist/_headers` with the hashes of the inline scripts. */
export default function cspHeaders() {
  return {
    name: "csp-headers",
    hooks: {
      "astro:build:done": async ({ dir }) => {
        const root = fileURLToPath(dir);
        const hashes = new Set();
        for (const file of await htmlFiles(root)) {
          const page = scanHtml(await readFile(file, "utf8"));
          if (page.handlers.length > 0) {
            throw new Error(`csp-headers: inline event handler in ${file}: ${page.handlers.join(", ")}`);
          }
          for (const hash of page.hashes) hashes.add(hash);
        }

        const headersFile = join(root, "_headers");
        const lines = (await readFile(headersFile, "utf8")).split("\n");
        const at = lines.flatMap((l, i) => (l.includes(PLACEHOLDER) ? [i] : []));
        if (at.length !== 1) throw new Error(`csp-headers: _headers must contain ${PLACEHOLDER} once`);
        const line = lines[at[0]].replace(PLACEHOLDER, [...hashes].sort().join(" ")).replace(/ +;/, ";");
        if (line.length > MAX_LINE) {
          throw new Error(`csp-headers: the CSP line has ${line.length} characters, the limit is ${MAX_LINE}`);
        }
        lines[at[0]] = line;
        await writeFile(headersFile, lines.join("\n"));
      },
    },
  };
}
