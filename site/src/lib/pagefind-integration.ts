import { existsSync, readFileSync } from 'node:fs';
import { extname, join, normalize, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { AstroIntegration } from 'astro';
import { close, createIndex } from 'pagefind';

/**
 * Chrome that sits inside indexed regions but isn't content: code-block
 * headers (label + copy button), the agent prompt's copy button, heading `#`
 * permalinks, the decorative title period. One list here instead of
 * `data-pagefind-ignore` sprinkled over every component that renders them.
 */
export const EXCLUDE_SELECTORS = ['.code-head', '.agent-prompt-copy', '.heading-anchor', '.title-period'];

const MIME: Record<string, string> = {
  '.js': 'text/javascript',
  '.json': 'application/json',
  '.css': 'text/css',
  '.wasm': 'application/wasm',
};

/**
 * Pagefind over the built site. A plain `astro:build:done` hook calling the
 * Node API (not `astro-pagefind`, which also pulls in Pagefind's stock UI we
 * don't use). `pnpm dev` has no built HTML to index, so it serves the index
 * from the last `pnpm build` under `/pagefind/`; without one the search
 * dialog says so instead of failing.
 */
export function pagefind(): AstroIntegration {
  let outDir: string | undefined;
  return {
    name: 'devsandboxes:pagefind',
    hooks: {
      'astro:config:done': ({ config }) => {
        outDir = fileURLToPath(config.outDir);
      },
      'astro:server:setup': ({ server }) => {
        server.middlewares.use((req, res, next) => {
          const path = req.url?.split('?')[0] ?? '';
          if (!outDir || !path.startsWith('/pagefind/')) return next();
          const root = join(outDir, 'pagefind');
          const file = normalize(join(outDir, decodeURIComponent(path)));
          if (!file.startsWith(root + sep) || !existsSync(file)) {
            res.statusCode = 404;
            res.end('No search index: run `pnpm build` once.');
            return;
          }
          res.setHeader('Content-Type', MIME[extname(file)] ?? 'application/octet-stream');
          res.end(readFileSync(file));
        });
      },
      'astro:build:done': async ({ dir, logger }) => {
        const site = fileURLToPath(dir);
        const { index, errors } = await createIndex({ excludeSelectors: EXCLUDE_SELECTORS });
        if (!index) throw new Error(`pagefind: ${errors.join('; ')}`);
        const added = await index.addDirectory({ path: site });
        if (added.errors.length) throw new Error(`pagefind: ${added.errors.join('; ')}`);
        const written = await index.writeFiles({ outputPath: join(site, 'pagefind') });
        if (written.errors.length) throw new Error(`pagefind: ${written.errors.join('; ')}`);
        await close();
        logger.info(`indexed ${added.page_count} pages`);
      },
    },
  };
}
