import { createHash } from 'node:crypto';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import type { AstroIntegration } from 'astro';
import type { ShikiTransformer } from 'shiki';

/**
 * Local modules `astro.config.mjs` pulls in (Shiki transformers, hast plugins,
 * integrations) are loaded once per dev-server start and aren't in Vite's
 * module graph, so editing them changes nothing until `astro dev` restarts;
 * and `.md` entries render into `.astro/data-store.json`, which Astro only
 * clears when the JSON of the config changes (functions don't serialize), so
 * even a restart keeps their old HTML. `configDeps` finds those modules for
 * `watchConfigDeps` (restart on edit) and `configDepsStamp` (clear the store).
 */

/** Relative specifiers a module imports or re-exports (`from './x'`, `import './x'`). */
export function localImports(source: string): string[] {
  const out: string[] = [];
  for (const m of source.matchAll(/\b(?:from|import)\s*['"](\.\.?\/[^'"]+)['"]/g)) out.push(m[1]);
  return out;
}

function resolveModule(from: string, spec: string): string | undefined {
  const base = resolve(dirname(from), spec);
  return [base, `${base}.ts`, `${base}/index.ts`].find((p) => existsSync(p) && statSync(p).isFile());
}

/** `entry`'s transitive local imports (entry excluded), sorted. Packages are skipped: they don't change under dev. */
export function configDeps(entry: string): string[] {
  const seen = new Set([entry]);
  const queue = [entry];
  for (let file = queue.pop(); file; file = queue.pop()) {
    for (const spec of localImports(readFileSync(file, 'utf8'))) {
      const dep = resolveModule(file, spec);
      if (dep && !seen.has(dep)) {
        seen.add(dep);
        queue.push(dep);
      }
    }
  }
  seen.delete(entry);
  return [...seen].sort();
}

/** Restart `astro dev` when one of `files` changes, as it does for the config itself. */
export function watchConfigDeps(files: string[]): AstroIntegration {
  return {
    name: 'devsandboxes:watch-config-deps',
    hooks: {
      'astro:config:setup': ({ addWatchFile }) => {
        for (const f of files) addWatchFile(f);
      },
    },
  };
}

/**
 * A no-op transformer whose name carries a hash of `files`: the name is part
 * of the serialized config, so an edit to a dep clears the content store.
 */
export function configDepsStamp(files: string[]): ShikiTransformer {
  const hash = createHash('sha256');
  for (const f of files) hash.update(f).update('\0').update(readFileSync(f)).update('\0');
  return { name: `devsandboxes:config-deps-${hash.digest('hex').slice(0, 12)}` };
}
