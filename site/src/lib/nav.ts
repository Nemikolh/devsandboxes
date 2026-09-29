export const REPO_URL = 'https://github.com/Nemikolh/devsandboxes';
export const VERSION = __DEVSANDBOX_VERSION__;

export interface NavLink {
  href: string;
  label: string;
  external?: boolean;
}

export interface NavGroup {
  label: string;
  links: NavLink[];
}

export const headerNav: NavLink[] = [
  { href: '/docs/quick-start', label: 'Docs' },
  { href: '/examples', label: 'Examples' },
  { href: '/docs/node-api', label: 'Node API' },
  { href: REPO_URL, label: 'GitHub', external: true },
];

/** Sidebar groups; `examples` are the collection's links in reading order. */
export function sidebarGroups(examples: NavLink[]): NavGroup[] {
  return [
    { label: 'GETTING STARTED', links: [{ href: '/docs/quick-start', label: 'Quick start' }] },
    { label: 'DASHBOARD', links: [{ href: '/docs/dashboard', label: 'Dashboard tour' }] },
    {
      label: 'REFERENCE',
      links: [
        { href: '/docs/config', label: 'Configuration' },
        { href: '/docs/node-api', label: 'Node API' },
      ],
    },
    { label: 'EXAMPLES', links: [{ href: '/examples', label: 'All examples' }, ...examples] },
  ];
}

/** One stop in the site's reading order, as its prev/next card shows it. */
export interface ReadingPage {
  href: string;
  title: string;
  description: string;
  /** Example pages get `… EXAMPLE` kickers, the index an `ALL EXAMPLES` one. */
  kind: 'docs' | 'examples-index' | 'example';
}

/** The pages that aren't collection entries, with their pager card text. */
export const REFERENCE_PAGES: ReadingPage[] = [
  { href: '/docs/config', title: 'Configuration', description: 'Every config.toml field, merge rule and variable.', kind: 'docs' },
  { href: '/docs/node-api', title: 'Node API', description: 'Drive devsandbox from TypeScript with the typed devsandboxes package.', kind: 'docs' },
];

export const EXAMPLES_INDEX: ReadingPage = {
  href: '/examples',
  title: 'Examples',
  description: 'Complete configs for real setups: parallel agents, shared services, caches and more.',
  kind: 'examples-index',
};

/**
 * The one reading order behind every prev/next pager: hand-written docs, the
 * generated references, the examples index, then each example. Both lists
 * come in already sorted.
 */
export function readingOrder(docs: ReadingPage[], examples: ReadingPage[]): ReadingPage[] {
  return [...docs, ...REFERENCE_PAGES, EXAMPLES_INDEX, ...examples];
}

/** Neighbours of `path` in `order`; a page missing from the order is a build error. */
export function readingNeighbors(order: ReadingPage[], path: string): { prev?: ReadingPage; next?: ReadingPage } {
  const p = normalizePath(path);
  const i = order.findIndex((page) => page.href === p);
  if (i < 0) throw new Error(`reading order: ${p} is not in it (add it in nav.ts)`);
  return { prev: order[i - 1], next: order[i + 1] };
}

/** The mono label on a pager card, from where it points and which way. */
export function pagerKicker(page: ReadingPage, dir: 'prev' | 'next'): string {
  if (page.kind === 'example') return dir === 'prev' ? 'PREVIOUS EXAMPLE' : 'NEXT EXAMPLE';
  if (page.kind === 'examples-index' && dir === 'prev') return 'ALL EXAMPLES';
  return dir === 'prev' ? 'PREVIOUS' : 'UP NEXT';
}

/** GitHub link for a repo-relative source path: its editor, or its blob view. */
export function sourceUrl(path: string, action: 'edit' | 'view'): string {
  return `${REPO_URL}/${action === 'edit' ? 'edit' : 'blob'}/main/${path}`;
}

/** Strip a trailing slash so `/docs/x/` and `/docs/x` compare equal. */
export function normalizePath(path: string): string {
  return path.length > 1 ? path.replace(/\/+$/, '') : path;
}

/**
 * Which header item is current for `path`. `Node API` lives under `/docs/`
 * but is its own header entry, so it wins over the generic `Docs` match.
 */
export function activeHeaderHref(path: string): string | undefined {
  const p = normalizePath(path);
  if (p === '/docs/node-api') return '/docs/node-api';
  if (p === '/examples' || p.startsWith('/examples/')) return '/examples';
  if (p.startsWith('/docs/')) return '/docs/quick-start';
  return undefined;
}
