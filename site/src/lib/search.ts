// Pure pieces of the search dialog: section badges, the "before typing" page
// list, keyboard decisions and Pagefind result shaping. No DOM, no Pagefind
// import, so vitest loads it and the client script stays thin.

import { normalizePath, REFERENCE_PAGES } from './nav';

export type SearchSection = 'HOME' | 'DOCS' | 'REFERENCE' | 'EXAMPLES';

/** Section badge (`data-pagefind-meta="section:…"`) of the page at `path`. */
export function searchSection(path: string): SearchSection {
  const p = normalizePath(path);
  if (p === '/') return 'HOME';
  if (p === '/examples' || p.startsWith('/examples/')) return 'EXAMPLES';
  if (REFERENCE_PAGES.some((r) => r.href === p)) return 'REFERENCE';
  return 'DOCS';
}

export interface BrowseItem {
  href: string;
  title: string;
  description: string;
}

export interface BrowseGroup {
  label: Exclude<SearchSection, 'HOME'>;
  items: BrowseItem[];
}

/** Every page, grouped like the sidebar; shown before the user types. */
export function browseGroups(docs: BrowseItem[], examples: BrowseItem[]): BrowseGroup[] {
  return [
    { label: 'DOCS', items: docs },
    { label: 'REFERENCE', items: REFERENCE_PAGES },
    {
      label: 'EXAMPLES',
      items: [{ href: '/examples', title: 'All examples', description: 'Every recipe on one page.' }, ...examples],
    },
  ];
}

export interface KeyLike {
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  /** Focus is in an input, textarea, select or contenteditable. */
  editable: boolean;
}

/** ⌘K / Ctrl+K toggle from anywhere; `/` opens unless the user is typing. */
export function shortcutAction(e: KeyLike): 'toggle' | 'open' | null {
  if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === 'k') return 'toggle';
  if (e.key === '/' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.editable) return 'open';
  return null;
}

/** Arrow navigation over `len` options, wrapping; -1 means none selected. */
export function stepIndex(current: number, delta: 1 | -1, len: number): number {
  if (len === 0) return -1;
  if (current < 0) return delta > 0 ? 0 : len - 1;
  return (current + delta + len) % len;
}

/** Apple platforms get ⌘, everyone else Ctrl. */
export function isApplePlatform(platform: string): boolean {
  return /mac|iphone|ipad|ipod/i.test(platform);
}

// ---- Pagefind (the slice of its result shape we use; it ships no client types)

export interface PagefindLocation {
  weight: number;
  balanced_score: number;
  location: number;
}

export interface PagefindSubResult {
  title: string;
  url: string;
  excerpt: string;
  anchor?: { id: string };
  weighted_locations?: PagefindLocation[];
}

export interface PagefindData {
  url: string;
  excerpt: string;
  meta: Record<string, string | undefined>;
  sub_results?: PagefindSubResult[];
}

export interface SearchHit {
  href: string;
  title: string;
  /** Heading the match sits under, when it isn't the page top. */
  heading?: string;
  /** Pagefind excerpt: escaped text with `<mark>` around matches. */
  excerpt: string;
  section: SearchSection;
}

const SECTIONS: SearchSection[] = ['HOME', 'DOCS', 'REFERENCE', 'EXAMPLES'];

/** Directory-style URLs (`/docs/config/#x`) → the site's own form (`/docs/config#x`). */
export function siteHref(url: string): string {
  const [path = '', hash] = url.split('#');
  const p = normalizePath(path) || '/';
  return hash ? `${p}#${hash}` : p;
}

const score = (s: PagefindSubResult) => (s.weighted_locations ?? []).reduce((n, l) => n + l.balanced_score, 0);

/**
 * One hit per page, pointing at its best-scoring sub-result (Pagefind lists
 * them in page order), so the link lands on the matching heading.
 */
export function toHit(data: PagefindData): SearchHit {
  const best = [...(data.sub_results ?? [])].sort((a, b) => score(b) - score(a))[0];
  const title = data.meta.title ?? siteHref(data.url);
  const section = SECTIONS.find((s) => s === data.meta.section) ?? searchSection(data.url);
  if (!best || !best.anchor) return { href: siteHref(best?.url ?? data.url), title, excerpt: best?.excerpt ?? data.excerpt, section };
  return { href: siteHref(best.url), title, heading: best.title, excerpt: best.excerpt, section };
}
