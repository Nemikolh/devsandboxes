// @ts-check
import { readFileSync } from 'node:fs';
import mdx from '@astrojs/mdx';
import { satteri } from '@astrojs/markdown-satteri';
import sitemap from '@astrojs/sitemap';
import { defineConfig } from 'astro/config';
import { packageVersion } from './src/lib/cargo.ts';
import { codeChrome } from './src/lib/code-chrome.ts';
import { dropSkillTitle, headingAnchors, tableKeyWeight, tableScroll } from './src/lib/heading-anchors.ts';
import { pagefind } from './src/lib/pagefind-integration.ts';
import { devsandboxesTheme } from './src/lib/shiki-theme.ts';

// Resolved from this file, not the cwd, so `astro build --root site` works too.
const version = packageVersion(readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8'));

export default defineConfig({
  site: 'https://devsandboxes.com',
  integrations: [mdx(), sitemap(), pagefind()],
  markdown: {
    processor: satteri({ hastPlugins: [dropSkillTitle(), headingAnchors(), tableKeyWeight(), tableScroll()] }),
    shikiConfig: {
      theme: devsandboxesTheme,
      transformers: [codeChrome()],
    },
  },
  vite: {
    define: {
      __DEVSANDBOX_VERSION__: JSON.stringify(version),
    },
    build: {
      rolldownOptions: {
        onwarn(warning, warn) {
          // Astro's content-assets plugin prefixes every MDX entry's
          // `?astroPropagatedAssets` module with a `"use astro:head-inject"`
          // marker that Astro reads itself; rolldown warns that bundling may
          // drop it, once per MDX file. Upstream and harmless, so drop exactly
          // that warning.
          if (warning.code === 'MODULE_LEVEL_DIRECTIVE' && warning.message.includes('"use astro:head-inject"')) return;
          warn(warning);
        },
      },
    },
  },
});
