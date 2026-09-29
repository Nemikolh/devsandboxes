import type { SatteriProcessorOptions } from '@astrojs/markdown-satteri';
import GithubSlugger from 'github-slugger';

type HastPluginEntry = NonNullable<SatteriProcessorOptions['hastPlugins']>[number];

/** Depths that get a visible `#` permalink (the TOC shows the same two). */
const ANCHORED = new Set(['h2', 'h3']);

/**
 * Sätteri hast plugin: ids + `#` permalinks on headings.
 *
 * Astro's own heading-ids plugin runs after user plugins, so the ids are
 * assigned here with the same slugger and text Astro would use; it then keeps
 * them (it honours an existing `id`), so `getHeadings()` and the anchors
 * agree. Every depth is slugged to keep the dedupe sequence (`-1`, `-2`)
 * identical to Astro's. The anchor is empty (the `#` is CSS) so it doesn't
 * leak into the heading text Astro collects for the TOC.
 */
export function headingAnchors(): HastPluginEntry {
  return () => {
    const slugger = new GithubSlugger();
    return {
      name: 'devsandboxes:heading-anchors',
      element: {
        filter: ['h1', 'h2', 'h3', 'h4', 'h5', 'h6'],
        visit(node, ctx) {
          const text = ctx.textContent(node);
          const existing = node.properties?.id;
          const slug = typeof existing === 'string' ? existing : slugger.slug(text);
          if (typeof existing !== 'string') ctx.setProperty(node, 'id', slug);
          if (!ANCHORED.has(node.tagName)) return;
          ctx.prependChild(node, {
            type: 'element',
            tagName: 'a',
            properties: {
              href: `#${slug}`,
              className: ['heading-anchor'],
              ariaLabel: `Link to section: ${text}`,
            },
            children: [],
          });
        },
      },
    };
  };
}

/**
 * Sätteri hast plugin: wrap tables in a scroll container, so wide tables
 * scroll on their own on mobile while staying real full-width tables.
 */
export function tableScroll(): HastPluginEntry {
  return {
    name: 'devsandboxes:table-scroll',
    element: {
      filter: ['table'],
      visit(node, ctx) {
        ctx.wrapNode(node, {
          type: 'element',
          tagName: 'div',
          properties: { className: ['table-scroll'], tabIndex: 0, role: 'region', ariaLabel: 'Table' },
          children: [],
        });
      },
    },
  };
}
