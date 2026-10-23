# zwinger docs

The user documentation of zwinger, built with Astro Starlight.

```sh
pnpm docs:dev     # local server
pnpm docs:build   # static build in site/dist
```

The pages are in `src/content/docs/`. The shortcuts page reads `SHORTCUTS` from `src/shared/useShortcuts.ts` of the
app. The build fails when a shortcut has no description in `src/components/Shortcuts.astro`.

## Cloudflare Pages

| setting | value |
|---|---|
| Root directory | empty (the repo root) |
| Build command | `pnpm --filter site build` |
| Build output directory | `site/dist` |
| Environment variable | `NODE_VERSION=24` |

The build must start at the repo root, because `src/components/Shortcuts.astro` imports `src/shared/useShortcuts.ts`
of the app, and that file needs the root install.

`public/_headers` sets the security headers.
