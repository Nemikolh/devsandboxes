import { describe, expect, it } from 'vitest';
import { browseGroups, isApplePlatform, searchSection, shortcutAction, siteHref, stepIndex, toHit, type KeyLike } from './search';

const key = (k: string, mods: Partial<KeyLike> = {}): KeyLike => ({
  key: k,
  metaKey: false,
  ctrlKey: false,
  altKey: false,
  editable: false,
  ...mods,
});

describe('searchSection', () => {
  it('maps paths to badges', () => {
    expect(searchSection('/')).toBe('HOME');
    expect(searchSection('/docs/quick-start/')).toBe('DOCS');
    expect(searchSection('/docs/config')).toBe('REFERENCE');
    expect(searchSection('/docs/node-api/')).toBe('REFERENCE');
    expect(searchSection('/examples')).toBe('EXAMPLES');
    expect(searchSection('/examples/caches/')).toBe('EXAMPLES');
  });
});

describe('browseGroups', () => {
  it('groups docs, reference and examples in sidebar order', () => {
    const groups = browseGroups([{ href: '/docs/a', title: 'A', description: '' }], [{ href: '/examples/x', title: 'X', description: '' }]);
    expect(groups.map((g) => g.label)).toEqual(['DOCS', 'REFERENCE', 'EXAMPLES']);
    expect(groups[1]?.items.map((i) => i.href)).toEqual(['/docs/config', '/docs/node-api']);
    expect(groups[2]?.items.map((i) => i.href)).toEqual(['/examples', '/examples/x']);
  });
});

describe('shortcutAction', () => {
  it('toggles on ⌘K and Ctrl+K, even while typing', () => {
    expect(shortcutAction(key('k', { metaKey: true }))).toBe('toggle');
    expect(shortcutAction(key('K', { ctrlKey: true, editable: true }))).toBe('toggle');
    expect(shortcutAction(key('k', { ctrlKey: true, altKey: true }))).toBeNull();
  });
  it('opens on / only outside editable fields', () => {
    expect(shortcutAction(key('/'))).toBe('open');
    expect(shortcutAction(key('/', { editable: true }))).toBeNull();
    expect(shortcutAction(key('/', { ctrlKey: true }))).toBeNull();
    expect(shortcutAction(key('k'))).toBeNull();
  });
});

describe('stepIndex', () => {
  it('wraps and starts from either end', () => {
    expect(stepIndex(-1, 1, 3)).toBe(0);
    expect(stepIndex(-1, -1, 3)).toBe(2);
    expect(stepIndex(2, 1, 3)).toBe(0);
    expect(stepIndex(0, -1, 3)).toBe(2);
    expect(stepIndex(0, 1, 0)).toBe(-1);
  });
});

describe('isApplePlatform', () => {
  it('detects macOS and iOS', () => {
    expect(isApplePlatform('MacIntel')).toBe(true);
    expect(isApplePlatform('macOS')).toBe(true);
    expect(isApplePlatform('iPhone')).toBe(true);
    expect(isApplePlatform('Linux x86_64')).toBe(false);
    expect(isApplePlatform('Win32')).toBe(false);
  });
});

describe('siteHref', () => {
  it('drops the directory slash, keeps the anchor', () => {
    expect(siteHref('/docs/config/#worktree-link')).toBe('/docs/config#worktree-link');
    expect(siteHref('/examples/')).toBe('/examples');
    expect(siteHref('/')).toBe('/');
  });
});

describe('toHit', () => {
  const loc = (balanced_score: number) => ({ weight: 1, balanced_score, location: 0 });
  it('points at the best-scoring sub-result heading', () => {
    const hit = toHit({
      url: '/docs/config/',
      excerpt: 'page',
      meta: { title: 'Configuration', section: 'REFERENCE' },
      sub_results: [
        { title: 'Configuration', url: '/docs/config/', excerpt: 'top', weighted_locations: [loc(1)] },
        { title: 'Sandboxes', url: '/docs/config/#sandboxes', excerpt: 'a <mark>b</mark>', anchor: { id: 'sandboxes' }, weighted_locations: [loc(2), loc(3)] },
      ],
    });
    expect(hit).toEqual({
      href: '/docs/config#sandboxes',
      title: 'Configuration',
      heading: 'Sandboxes',
      excerpt: 'a <mark>b</mark>',
      section: 'REFERENCE',
    });
  });
  it('falls back to the page and a path-derived section', () => {
    const hit = toHit({ url: '/examples/caches/', excerpt: 'x', meta: { title: 'Caches' } });
    expect(hit).toEqual({ href: '/examples/caches', title: 'Caches', excerpt: 'x', section: 'EXAMPLES' });
  });
});
