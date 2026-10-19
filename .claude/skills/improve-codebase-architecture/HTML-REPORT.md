# HTML Report Format

A temporary, self-contained HTML file. Not product UI. Only Tailwind and Mermaid load from CDNs; all else is inline, static. Mix Mermaid with handmade divs or inline SVG.

## Scaffold

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>Architecture review — {{repo}}</title>
    <script src="https://cdn.tailwindcss.com"></script>
    <script type="module">
      import mermaid from "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs";
      mermaid.initialize({ startOnLoad: true, theme: "neutral", securityLevel: "loose" });
    </script>
    <style>
      .seam { stroke-dasharray: 4 4; }
      .leak { stroke: #dc2626; }
      .deep { background: linear-gradient(135deg, #0f172a, #1e293b); }
    </style>
  </head>
  <body class="bg-stone-50 text-slate-900 font-sans">
    <main class="max-w-5xl mx-auto px-6 py-12 space-y-12">
      <header>...</header>
      <section id="candidates" class="space-y-10">...</section>
      <section id="top-recommendation">...</section>
    </main>
  </body>
</html>
```

**Header**: repo, date, legend (solid box = module, dashed line = seam, red arrow = leakage, thick dark box = deep module). No introduction.

## Candidate card

One `<article>` per candidate: mostly diagrams, little prose.

- **Title**: the deepening.
- **Badges**: strength (`Strong` emerald, `Worth exploring` amber, `Speculative` slate) and dependency category (`in-process`, `framework-substitutable`, `ports & adapters`, `mock`).
- **Files** in `font-mono text-sm`.
- **Before/After diagrams** side by side, ~320px high.
- **Problem** and **Solution**, one sentence each.
- **Wins**: ≤6-word bullets in glossary terms ("locality: bugs concentrate in one module", "leverage: one interface, N call sites"). Never "cleaner code".
- **Decision callout** (amber, one line) for an applicable ADR.

If a diagram needs a paragraph, redraw it.

## Diagram patterns (vary across cards)

- **Mermaid flowchart/sequence**: call flow, dependencies, "6 round-trips → 1". White bordered card; `classDef` leak edges red, deep module dark.
- **Handmade boxes and arrows**: divs and overlay SVG; thick-bordered deep module, faded internals.
- **Cross-section**: stacked bands per call layer: many thin → one thick.
- **Mass diagram**: interface and implementation rectangles: shallow (equal) → deep (short over tall).
- **Call-graph collapse**: nested call boxes merged, internals faded.

## Style

- Editorial, much whitespace. One accent (emerald or indigo), red for leakage, amber for warnings. Labels: `text-xs uppercase tracking-wider`.
- **Top recommendation**: a larger card: name, one-sentence reason, anchor link.
- Use only `/codebase-design` glossary terms; never _Avoid_ words, layer or wrapper.
- Do not hedge. Use bullets and minimal text.
