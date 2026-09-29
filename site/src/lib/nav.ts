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
