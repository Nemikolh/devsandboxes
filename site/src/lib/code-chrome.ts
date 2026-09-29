import type { Element, ElementContent } from 'hast';
import type { ShikiTransformer } from 'shiki';

// Shells render as a terminal, like the prototype's default label.
const TERMINAL_LANGS = new Set(['bash', 'sh', 'shell', 'zsh', 'console', 'shellscript']);

/** `<key>="…"` (or `<key>='…'`) from a fence's meta string. */
export function metaAttr(meta: string | undefined, key: string): string | undefined {
  const m = meta?.match(new RegExp(`(?:^|\\s)${key}=(?:"([^"]*)"|'([^']*)')`));
  return m ? (m[1] ?? m[2]) : undefined;
}

/** Whether a fence's meta has the bare word `key` (` ```sh collapsed `), not inside a quoted value. */
export function metaFlag(meta: string | undefined, key: string): boolean {
  if (!meta) return false;
  return meta
    .replace(/"[^"]*"|'[^']*'/g, '""')
    .split(/\s+/)
    .includes(key);
}

/** Lines a `collapsed` block shows before its "Show all" toggle. */
export const COLLAPSED_LINES = 10;

/** The collapse toggle's label while the block is collapsed. */
export function expandLabel(lines: number): string {
  return `Show all ${lines} lines`;
}

export const COLLAPSE_LABEL = 'Show less';

/** Shiki's lines: one `span.line` per source line, directly under `<code>`. */
export function countLines(pre: Element): number {
  const code = pre.children.find((n): n is Element => n.type === 'element' && n.tagName === 'code');
  if (!code) return 0;
  // Shiki sets `class: 'line'` (a string), not hast's `className` array.
  return code.children.filter((n) => n.type === 'element' && String(n.properties.class).split(' ').includes('line'))
    .length;
}

/** A page-unique-enough id for `aria-controls`: a hash of the block's code. */
export function codeId(text: string): string {
  let h = 5381;
  for (let i = 0; i < text.length; i++) h = ((h * 33) ^ text.charCodeAt(i)) >>> 0;
  return `code-${h.toString(36)}`;
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
// Lucide `chevron-down` (rotated by CSS when expanded).
const CHEVRON_ICON: IconNode = [['path', { d: 'm6 9 6 6 6-6' }]];

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

/**
 * The collapse toggle under a `collapsed` block. The markup is the collapsed
 * state, but CSS only clips while `<html data-js>` (set before first paint), so
 * without JS the block reads in full and the toggle is hidden.
 */
function collapseFoot(id: string, lines: number): Element {
  return el('div', { className: ['code-foot'] }, [
    el(
      'button',
      {
        type: 'button',
        className: ['collapse-button'],
        dataCollapseToggle: '',
        ariaExpanded: 'false',
        ariaControls: [id],
      },
      [
        icon(CHEVRON_ICON, 'icon-chevron'),
        el('span', { className: ['collapse-text'] }, [{ type: 'text', value: expandLabel(lines) }]),
      ],
    ),
  ]);
}

/** The `.code-block` chrome (header label + copy button) around a `<pre>`. */
export function codeChromeWrap(
  pre: Element,
  label: string,
  lang: string | undefined,
  collapse?: { id: string; lines: number },
): Element {
  if (collapse) pre.properties.id = collapse.id;
  const props: Element['properties'] = { className: ['code-block'], dataLang: lang ?? '' };
  if (collapse) Object.assign(props, { dataCollapsed: '', dataLines: String(collapse.lines) });
  return el('div', props, [
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
    ...(collapse ? [collapseFoot(collapse.id, collapse.lines)] : []),
  ]);
}

/**
 * Shiki transformer wrapping every highlighted block in the code chrome. A
 * transformer (not a rehype plugin) so it applies uniformly to `.md`, `.mdx`
 * (both go through Astro's Sätteri pipeline, which has no rehype) and direct
 * `codeToHtml` calls. `title` overrides the fence meta for direct calls.
 * A `collapsed` fence longer than `COLLAPSED_LINES` gets a show-all toggle.
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
      const meta = rawMeta(this.options.meta);
      const label = opts.title ?? codeLabel(lang, meta);
      const lines = countLines(pre);
      const collapse =
        metaFlag(meta, 'collapsed') && lines > COLLAPSED_LINES
          ? { id: codeId(`${meta}\n${this.source}`), lines }
          : undefined;
      root.children = [codeChromeWrap(pre, label, lang, collapse)];
    },
  };
}
