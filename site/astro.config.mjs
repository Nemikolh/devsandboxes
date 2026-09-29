// @ts-check
import { readFileSync } from 'node:fs';
import mdx from '@astrojs/mdx';
import { satteri } from '@astrojs/markdown-satteri';
import sitemap from '@astrojs/sitemap';
import { defineConfig } from 'astro/config';
import { packageVersion } from './src/lib/cargo.ts';
import { codeChrome } from './src/lib/code-chrome.ts';
import { headingAnchors, tableScroll } from './src/lib/heading-anchors.ts';
import { devsandboxesTheme } from './src/lib/shiki-theme.ts';

// Resolved from this file, not the cwd, so `astro build --root site` works too.
const version = packageVersion(readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8'));

export default defineConfig({
  site: 'https://devsandboxes.com',
  integrations: [mdx(), sitemap()],
  markdown: {
    processor: satteri({ hastPlugins: [headingAnchors(), tableScroll()] }),
    shikiConfig: {
      theme: devsandboxesTheme,
      transformers: [codeChrome()],
    },
  },
  vite: {
    define: {
      __DEVSANDBOX_VERSION__: JSON.stringify(version),
    },
  },
});
