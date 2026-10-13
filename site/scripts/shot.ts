// Screenshot built pages in headless Chromium, to check diagrams and layout
// by eye without a browser. Serves `dist/` itself on a free port (not
// `astro preview`, which refuses to start while it thinks another preview
// runs), or uses `--base`; then writes one PNG per matching element per
// width (`--full`: the whole page instead) and prints the paths.
//
//   pnpm build && pnpm shot /architecture/bridge /examples/babysit-pr --width 1440,420
//
// Options: `--width 1440,1024,420` (default 1440,420), `--selector figure`
// (default; no match falls back to the full page), `--full`, `--out <dir>`
// (default /tmp/site-shots), `--base http://localhost:4321` (a running
// `pnpm dev` / `pnpm preview`). Needs the browser once:
// `pnpm exec playwright install chromium` (`--with-deps` on a bare Linux box).
import { existsSync, mkdirSync, readFileSync, statSync } from 'node:fs';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const DIST = fileURLToPath(new URL('../dist', import.meta.url));
const TYPES: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css',
  '.js': 'text/javascript',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.woff2': 'font/woff2',
  '.wasm': 'application/wasm',
  '.xml': 'application/xml',
};
const USAGE = 'usage: pnpm shot <path>... [--width 1440,420] [--selector figure] [--full] [--out DIR] [--base URL]';

const args = process.argv.slice(2);
const paths: string[] = [];
let widths = [1440, 420];
let selector = 'figure';
let full = false;
let out = '/tmp/site-shots';
let base: string | undefined;
for (let i = 0; i < args.length; i++) {
  const a = args[i];
  const value = () => {
    const v = args[++i];
    if (v === undefined) throw new Error(`${a} needs a value\n${USAGE}`);
    return v;
  };
  if (a === '--width') widths = value().split(',').map(Number);
  else if (a === '--selector') selector = value();
  else if (a === '--full') full = true;
  else if (a === '--out') out = value();
  else if (a === '--base') base = value().replace(/\/+$/, '');
  else if (a.startsWith('--')) throw new Error(`unknown option ${a}\n${USAGE}`);
  else paths.push(a.startsWith('/') ? a : `/${a}`);
}
if (paths.length === 0 || widths.some((w) => !Number.isInteger(w) || w <= 0)) {
  console.error(USAGE);
  process.exit(2);
}

/** The built file for a URL path, resolved like `check-links.mjs` does. */
function resolve(urlPath: string): string | undefined {
  const clean = normalize(decodeURIComponent(urlPath.split('?')[0])).replace(/\/+$/, '');
  if (clean.split('/').includes('..')) return undefined;
  const candidates = [join(DIST, clean, 'index.html'), join(DIST, `${clean}.html`), join(DIST, clean)];
  return candidates.find((c) => existsSync(c) && statSync(c).isFile());
}

let server: Server | undefined;
if (!base) {
  if (!existsSync(join(DIST, 'index.html'))) throw new Error(`no build in ${DIST}: run pnpm build first`);
  server = createServer((req, res) => {
    const file = resolve(req.url ?? '/');
    res.writeHead(file ? 200 : 404, { 'content-type': TYPES[extname(file ?? '.html')] ?? 'application/octet-stream' });
    res.end(file ? readFileSync(file) : 'not found');
  });
  await new Promise<void>((ok) => server!.listen(0, '127.0.0.1', ok));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
}

mkdirSync(out, { recursive: true });
const browser = await chromium.launch();
try {
  for (const width of widths) {
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    for (const path of paths) {
      await page.goto(base + path, { waitUntil: 'networkidle' });
      const slug = path.replace(/^\/|\/$/g, '').replace(/\//g, '-') || 'index';
      const matches = full ? [] : await page.$$(selector);
      if (matches.length === 0) {
        const file = join(out, `${width}-${slug}.png`);
        await page.screenshot({ path: file, fullPage: true });
        console.log(file);
      }
      for (const [i, el] of matches.entries()) {
        const file = join(out, `${width}-${slug}-${i}.png`);
        await el.screenshot({ path: file });
        console.log(file);
      }
    }
    await page.close();
  }
} finally {
  await browser.close();
  server?.close();
}
