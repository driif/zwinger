# zwinger docs

The user documentation of zwinger, built with Astro Starlight. The UI comes from `@driif/motiv`, as in the app.

```sh
pnpm docs:dev     # local server
pnpm docs:build   # static build in site/dist
```

The pages are in `src/content/docs/`. The shortcuts page reads `SHORTCUTS` from `src/shared/useShortcuts.ts` of the
app. The build fails when a shortcut has no description in `src/components/Shortcuts.astro`.

## motiv

Starlight renders the pages. These overrides in `src/components/` replace Starlight UI with motiv components:

- `Sidebar.astro`: the sidebar links as a motiv `GlideNav` (`DocsNav.tsx`). Groups are flattened.
- `ThemeSelect.astro`: a motiv `ThemeToggle` (`ThemeSwitch.tsx`). It keeps the `starlight-theme` key of Starlight.
- `Hero.astro`: the hero actions as links with the motiv button classes.
- `Features.tsx`: the motiv cards on the home page.

`src/styles/theme.css` sets the layer order (Starlight, then motiv) and maps the Starlight colors and font to the
`--motiv-*` tokens. It must stay first in `customCss`. Change colors in motiv, not here.

## Cloudflare

Cloudflare Workers Builds deploys the site on each push to `main`. `wrangler.jsonc` in the repo root holds the
settings: it builds the site (`pnpm --filter site build`) and serves `site/dist` as static assets. In the dashboard,
keep the root directory empty (the repo root), the build command empty and the deploy command `npx wrangler deploy`.
The Worker name in the dashboard must be `zwinger`, the `name` in `wrangler.jsonc`.

The build must start at the repo root, because `src/components/Shortcuts.astro` imports `src/shared/useShortcuts.ts`
of the app, and that file needs the root install.

`public/_headers` sets the security headers, including the `Content-Security-Policy`. Its `{{inline-script-hashes}}`
placeholder is filled at build time by `src/integrations/csp-headers.mjs`: it parses each built page, hashes each inline
script and writes the result to `dist/_headers`. To review the final policy, inspect `dist/_headers` after a build. The
build fails on an inline event handler attribute. `astro dev` does not enforce the CSP.
