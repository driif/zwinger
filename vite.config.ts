import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
// @ts-expect-error type error without @types/node package
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(() => {
  // Only the root app build gets the first paint entry. Other HTML inputs build as usual.
  let appHtml = "";
  return {
    plugins: [
      react(),
      {
        name: "first-paint-entry",
        configResolved(config) {
          appHtml = `${config.root}/index.html`;
        },
        transformIndexHtml: {
          order: "post",
          handler(html, context) {
            if (context.filename !== appHtml) return html;
            if (context.server) {
              return html.replace(
                '<script type="module" src="/src/main.tsx"></script>',
                '<script type="module" src="/src/features/appearance/firstPaint.ts"></script>\n    $&',
              );
            }
            if (!context.bundle) return html;
            const early = Object.values(context.bundle).find((asset) => asset.type === "chunk" && asset.name === "firstPaint");
            if (!early) throw new Error("missing first paint entry");
            const css = Object.values(context.bundle).find((asset) => asset.type === "asset" && asset.fileName.startsWith("assets/app-") && asset.fileName.endsWith(".css"));
            if (!css) throw new Error("missing first paint styles");
            const main = html.match(/<script type="module" crossorigin src="[^"]+"><\/script>/)?.[0];
            if (!main) throw new Error("missing main entry");
            const stylesheets = html.match(/<link rel="stylesheet"[^>]*>/g) ?? [];
            if (!stylesheets.some((link) => link.includes(`href="/${css.fileName}"`))) {
              stylesheets.push(`<link rel="stylesheet" crossorigin href="/${css.fileName}">`);
            }
            return html.replace(/\s*<link rel="stylesheet"[^>]*>/g, "").replace(main, [
              ...stylesheets,
              `<script type="module" crossorigin src="/${early.fileName}"></script>`,
              main,
            ].join("\n    "));
          },
        },
        writeBundle(_options, bundle) {
          const chunks = Object.values(bundle).filter((output) => output.type === "chunk");
          const main = chunks.find((chunk) => chunk.isEntry && chunk.facadeModuleId === appHtml);
          if (!main) return;
          const html = bundle["index.html"];
          if (html?.type !== "asset") throw new Error("missing index.html");
          const source = String(html.source);
          const scripts = [...source.matchAll(/<script type="module"/g)].map((match) => match.index);
          const links = [...source.matchAll(/<link rel="stylesheet"/g)].map((match) => match.index);
          if (scripts.length !== 2) throw new Error("index.html must load two module entries");
          if (links.some((link) => link > Math.min(...scripts))) throw new Error("a stylesheet link comes after a module script");

          const early = chunks.find((chunk) => chunk.isEntry && chunk.name === "firstPaint");
          if (!early || early === main) throw new Error("the first paint and main entries must be separate chunks");
          const reaches = (from: string, to: string, seen = new Set<string>()): boolean => {
            if (from === to) return true;
            if (seen.has(from)) return false;
            seen.add(from);
            const chunk = bundle[from];
            return chunk?.type === "chunk" && [...chunk.imports, ...chunk.dynamicImports].some((next) => reaches(next, to, seen));
          };
          if (reaches(main.fileName, early.fileName) || reaches(early.fileName, main.fileName)) {
            throw new Error("the first paint and main entries must run without each other");
          }
          for (const entry of [early, main]) {
            if (!source.includes(`src="/${entry.fileName}"`)) throw new Error(`index.html does not load ${entry.fileName}`);
          }
        },
      },
    ],
    build: {
      rolldownOptions: {
        input: ["index.html", "src/features/appearance/firstPaint.ts"],
        output: {
          manualChunks(id) {
            if (id.includes("/@tauri-apps/api/") || id.includes("/src/shared/api.ts")
              || id.includes("/src/shared/errorMessage.ts") || id.includes("/src/features/appearance/builtIns.ts")
              || id.includes("/src/features/appearance/themeCache.ts")
              || id.includes("/src/features/appearance/reveal.ts")) return "startupShared";
          },
        },
      },
    },
    // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
    //
    // 1. prevent Vite from obscuring rust errors
    clearScreen: false,
    // 2. tauri expects a fixed port, fail if that port is not available
    server: {
      port: 1420,
      strictPort: true,
      host: host || false,
      hmr: host
        ? {
            protocol: "ws",
            host,
            port: 1421,
          }
        : undefined,
      watch: {
        // 3. tell Vite to ignore watching `src-tauri` and the task folders (test temp dirs hold symlink loops)
        ignored: ["**/src-tauri/**", "**/docs/tasks/**"],
      },
    },
  };
});
