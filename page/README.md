# Raddy documentation site

The Raddy docs site, built with [Astro Starlight](https://starlight.astro.build).
English content lives at the site root; Simplified Chinese is under `/zh-CN/`.

The site deploys to GitHub Pages on every `main` push via
`.github/workflows/pages.yml` (in the repository root). It is served from the
`/raddy/` sub-path — `site` and `base` are set in `astro.config.mjs`, and
internal links in the content are **relative** so they resolve under any base.

## Project structure

```
.
├── astro.config.mjs        # Starlight config: i18n, sidebar, base path
├── src/
│   ├── content/
│   │   └── docs/           # English content (root locale)
│   │       └── zh-cn/      # Simplified Chinese content (mirrored paths)
│   └── content.config.ts
├── public/favicon.svg
├── package.json
└── tsconfig.json
```

Each page under `src/content/docs/` maps to a route. The English tree is the
canonical source; the `zh-cn/` subdirectory mirrors translated paths where they
exist, and missing translations fall back to English until written.

## Commands

Run from this directory:

| Command | Action |
| :------ | :----- |
| `pnpm install` | Install dependencies |
| `pnpm dev` | Start the local dev server at `localhost:4321` |
| `pnpm build` | Build the production site to `./dist/` |
| `pnpm preview` | Preview the production build locally |
| `pnpm astro -- --help` | Astro CLI help |

## Writing docs

- Write new or rewritten documentation in English first. Keep the existing
  `zh-cn/` translations aligned when localization work is scheduled.
- Use **relative** links between pages (never `/config/…` absolute paths) so
  they keep working under the `/raddy/` base.
- Raddyfile code blocks use the `caddyfile` language tag — a lightweight grammar
  is registered in `astro.config.mjs`.
