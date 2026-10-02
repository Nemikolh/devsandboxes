# devsandboxes docs site

Static [Astro](https://astro.build) + MDX site for <https://devsandboxes.com>.
Standalone pnpm project (Node >= 22.12), not part of the Cargo workspace.

```sh
pnpm i
pnpm dev        # http://localhost:4321, live reload
pnpm build      # static output in dist/, then the internal link check
pnpm preview    # serve dist/
pnpm check      # astro check (types + .astro diagnostics)
pnpm test       # vitest over the pure helpers in src/lib/
pnpm check:links  # scripts/check-links.mjs alone: every href="/…" / "#…" in
                  # dist/ must hit a built page and an existing id
```

## Deployment

CI (`.github/workflows/ci.yml`, job `site`) runs check, test and build on
every PR and push to main. Deployment to GitHub Pages is
`.github/workflows/pages.yml`: `release.yml` calls it on each release tag,
after the GitHub release and npm publish, so the docs match what users can
install. For docs-only changes between releases, run it by hand (Actions →
Pages → Run workflow, pick the branch or tag to deploy).

## Layout

- `src/pages/` — routes. `src/layouts/` — `Base.astro` (head, header,
  footer, mobile drawer) and `Docs.astro` (sidebar / article / TOC shell).
- `src/components/` — header, sidebar, TOC, code block, callout, resource
  link, prev/next `Pager` (reading order from `nav.ts`), mobile drawer, accessible `Tabs` (panels are named
  slots), `InstallTabs` (install commands built from the Cargo.toml version).
- `src/styles/` — plain CSS: `tokens.css` (palette, fonts, radii), `base.css`,
  `components.css`, `home.css`, `docs.css`. No Tailwind.
- `src/lib/` — build-time helpers:
  - `shiki-theme.ts` — the syntax theme.
  - `code-chrome.ts` — Shiki transformer wrapping every highlighted block in
    the `.code-block` chrome (label from `title="…"` in the fence meta, else
    the language; copy button). Applies to `.md`, `.mdx` and `CodeBlock.astro`.
  - `heading-anchors.ts` — Sätteri (Astro's Markdown pipeline) plugins for
    heading ids + `#` permalinks and scrollable table wrappers.
  - `nav.ts` — header nav, sidebar groups, and the reading order behind
    every prev/next pager (docs → references → examples index → examples).
  - `pagefind-integration.ts` — builds the Pagefind index over `dist/` after
    `astro build`; `pnpm dev` serves the last build's `dist/pagefind/`, so
    search in dev needs one `pnpm build` first.
  - `search.ts` — pure helpers of the search dialog (`Search.astro` +
    `scripts/search.ts`). Indexed regions are marked `data-pagefind-body`
    in the layouts; chrome inside them is excluded in the integration.
- `public/og.png` — the 1200×630 social card every page's `og:image` points
  at. A committed PNG (rendered once from HTML in the brand style); re-render
  it by hand if the brand changes.
- The header version chip is the `[package] version` of `../Cargo.toml`, read
  in `astro.config.mjs` at build time.

## Content

Hand-written docs pages are MDX in the `docs` content collection
(`src/content/docs/<slug>.mdx` → `/docs/<slug>`, frontmatter `title`,
`description`, `eyebrow`, `order`; schema in `src/content.config.ts`). The
landing page links to `/docs/quick-start#the-dashboard`, so keep that heading's
text. Two pages are generated at build time
from files elsewhere in the repo, so edit those, not the site:

- `/docs/config` — rendered from `../skills/config-toml-spec/SKILL.md`.
- `/docs/node-api` — generated from `../npm/devsandboxes/index.d.ts` with the
  TypeScript compiler API.

Every config snippet on the site must be valid: write it to a temp dir as
`devsandboxes.toml` and run `cargo run -q -- -C <dir> ls` from the repo root.
