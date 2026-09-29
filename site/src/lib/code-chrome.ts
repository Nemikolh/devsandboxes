import type { Element, ElementContent } from 'hast';
import type { ShikiTransformer } from 'shiki';

// Shells render as a terminal, like the prototype's default label.
const TERMINAL_LANGS = new Set(['bash', 'sh', 'shell', 'zsh', 'console', 'shellscript']);

/** `<key>="…"` (or `<key>='…'`) from a fence's meta string. */
export function metaAttr(meta: string | undefined, key: string): string | undefined {
  const m = meta?.match(new RegExp(`(?:^|\\s)${key}=(?:"([^"]*)"|'([^']*)')`));
  return m ? (m[1] ?? m[2]) : undefined;
}

/** `title="…"` (or `title='…'`) from a fence's meta string. */
export function parseTitle(meta: string | undefined): string | undefined {
  return metaAttr(meta, 'title');
}

/** Header label: the fence title verbatim, else the language uppercased. */
export function codeLabel(lang: string | undefined, meta: string | undefined): string {
  const title = parseTitle(meta);
  if (title) return title;
  const l = (lang ?? '').toLowerCase();
  if (!l || l === 'plaintext' || l === 'text' || l === 'txt') return 'CODE';
  return TERMINAL_LANGS.has(l) ? 'TERMINAL' : l.toUpperCase();
}

/**
 * Shiki's `meta` is `{ __raw }`, but Astro's Sätteri pipeline passes the raw
 * fence string itself; accept both.
 */
export function rawMeta(meta: unknown): string | undefined {
  if (typeof meta === 'string') return meta;
  if (meta && typeof meta === 'object' && '__raw' in meta) {
    const raw = (meta as { __raw?: unknown }).__raw;
    return typeof raw === 'string' ? raw : undefined;
  }
  return undefined;
}

type IconNode = [string, Record<string, string>][];

// Lucide `copy` / `check` (kept inline: this runs inside Shiki, not Astro).
const COPY_ICON: IconNode = [
  ['rect', { width: '14', height: '14', x: '8', y: '8', rx: '2', ry: '2' }],
  ['path', { d: 'M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2' }],
];
const CHECK_ICON: IconNode = [['path', { d: 'M20 6 9 17l-5-5' }]];

function el(tagName: string, properties: Element['properties'], children: ElementContent[] = []): Element {
  return { type: 'element', tagName, properties, children };
}

function icon(node: IconNode, className: string): Element {
  return el(
    'svg',
    {
      xmlns: 'http://www.w3.org/2000/svg',
      width: '14',
      height: '14',
      viewBox: '0 0 24 24',
      fill: 'none',
      stroke: 'currentColor',
      strokeWidth: '2',
      strokeLinecap: 'round',
      strokeLinejoin: 'round',
      ariaHidden: 'true',
      className: [className],
    },
    node.map(([tag, attrs]) => el(tag, attrs)),
  );
}

/** The `.code-block` chrome (header label + copy button) around a `<pre>`. */
export function codeChromeWrap(pre: Element, label: string, lang: string | undefined): Element {
  return el('div', { className: ['code-block'], dataLang: lang ?? '' }, [
    el('div', { className: ['code-head'] }, [
      el('span', {}, [el('span', { className: ['code-dot'], ariaHidden: 'true' }), { type: 'text', value: label }]),
      el(
        'button',
        {
          type: 'button',
          className: ['copy-button'],
          dataCopy: '',
          ariaLabel: `Copy ${label.toLowerCase()} code`,
        },
        [
          icon(COPY_ICON, 'icon-copy'),
          icon(CHECK_ICON, 'icon-check'),
          el('span', { className: ['copy-text'], ariaLive: 'polite' }, [{ type: 'text', value: 'Copy' }]),
        ],
      ),
    ]),
    pre,
  ]);
}

/**
 * Shiki transformer wrapping every highlighted block in the code chrome. A
 * transformer (not a rehype plugin) so it applies uniformly to `.md`, `.mdx`
 * (both go through Astro's Sätteri pipeline, which has no rehype) and direct
 * `codeToHtml` calls. `title` overrides the fence meta for direct calls.
 */
export function codeChrome(opts: { title?: string } = {}): ShikiTransformer {
  return {
    name: 'devsandboxes:code-chrome',
    pre(pre) {
      // Shiki serializes `meta` keys onto <pre>; a string meta yields "0", "1", … attributes.
      for (const k of Object.keys(pre.properties)) if (/^\d+$/.test(k)) delete pre.properties[k];
      pre.properties.tabindex = '0';
    },
    root(root) {
      const pre = root.children.find((n): n is Element => n.type === 'element' && n.tagName === 'pre');
      if (!pre) return;
      const lang = this.options.lang;
      const label = opts.title ?? codeLabel(lang, rawMeta(this.options.meta));
      root.children = [codeChromeWrap(pre, label, lang)];
    },
  };
}
