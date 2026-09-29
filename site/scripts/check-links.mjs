#!/usr/bin/env node
// Internal link check over the built site: every `href="/…"` and `href="#…"`
// in `dist/**/*.html` must land on a built page and, with a fragment, on an
// element with that id there. Runs at the end of `pnpm build`; standalone as
// `pnpm check:links [dist dir]`. Regex over the HTML is enough: Astro's output
// is regular (double-quoted attributes), and a false alarm fails loudly.
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = process.argv[2] ?? fileURLToPath(new URL('../dist', import.meta.url));

function htmlFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return htmlFiles(p);
    return e.name.endsWith('.html') ? [p] : [];
  });
}

/** The URL path a built file is served at (`docs/x/index.html` → `/docs/x`). */
function pagePath(file) {
  const rel = relative(dist, file).split(sep).join('/');
  return '/' + rel.replace(/(^|\/)index\.html$/, '').replace(/\.html$/, '').replace(/\/$/, '');
}

const decode = (s) => s.replace(/&amp;/g, '&').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&lt;/g, '<').replace(/&gt;/g, '>');

/** Built file for a URL path, or undefined (directory index, `.html`, or a static asset). */
function resolveTarget(path) {
  const clean = decodeURIComponent(path).replace(/\/+$/, '') || '/';
  const candidates = [join(dist, clean, 'index.html'), join(dist, `${clean}.html`), join(dist, clean)];
  return candidates.find((c) => existsSync(c) && statSync(c).isFile());
}

const files = htmlFiles(dist);
const ids = new Map();
const idsOf = (file) => {
  if (!ids.has(file)) ids.set(file, new Set([...readFileSync(file, 'utf8').matchAll(/\sid="([^"]*)"/g)].map((m) => decode(m[1]))));
  return ids.get(file);
};

const broken = [];
let checked = 0;
for (const file of files) {
  const html = readFileSync(file, 'utf8');
  const from = pagePath(file);
  for (const [, raw] of html.matchAll(/\shref="([^"]*)"/g)) {
    const href = decode(raw);
    // External, protocol-relative, mailto: and friends are out of scope.
    if (!href.startsWith('#') && !(href.startsWith('/') && !href.startsWith('//'))) continue;
    checked++;
    const [pathAndQuery, fragment] = href.split('#', 2);
    const path = pathAndQuery.split('?')[0];
    const target = path ? resolveTarget(path) : file;
    if (!target) {
      broken.push(`${from}: ${href} (no such page)`);
      continue;
    }
    if (fragment && target.endsWith('.html') && !idsOf(target).has(decodeURIComponent(fragment))) {
      broken.push(`${from}: ${href} (no #${fragment} on ${pagePath(target)})`);
    }
  }
}

if (broken.length) {
  const unique = [...new Set(broken)];
  console.error(`check-links: ${unique.length} broken internal link(s):\n  ${unique.join('\n  ')}`);
  process.exit(1);
}
console.log(`check-links: ${checked} internal links across ${files.length} pages, all resolve`);
