import { describe, expect, it } from 'vitest';
import { EXAMPLES_INDEX, pagerKicker, readingNeighbors, readingOrder, type ReadingPage, sourceUrl } from './nav';

const page = (href: string, kind: ReadingPage['kind']): ReadingPage => ({ href, title: href, description: '', kind });
const quickStart = page('/docs/quick-start', 'docs');
const first = page('/examples/a', 'example');
const last = page('/examples/b', 'example');
const order = readingOrder([quickStart], [first, last]);

describe('readingOrder', () => {
  it('runs docs, references, the examples index, then examples', () => {
    expect(order.map((p) => p.href)).toEqual(['/docs/quick-start', '/docs/config', '/docs/node-api', '/examples', '/examples/a', '/examples/b']);
  });
});

describe('readingNeighbors', () => {
  it('has no prev at the start and no next at the end', () => {
    expect(readingNeighbors(order, '/docs/quick-start')).toEqual({ prev: undefined, next: order[1] });
    expect(readingNeighbors(order, '/examples/b/')).toEqual({ prev: first, next: undefined });
  });

  it('hands the last reference over to the examples index, and the index to the first example', () => {
    expect(readingNeighbors(order, '/docs/node-api').next).toBe(EXAMPLES_INDEX);
    expect(readingNeighbors(order, '/examples')).toEqual({ prev: order[2], next: first });
  });

  it('rejects a page outside the order', () => {
    expect(() => readingNeighbors(order, '/docs/nope')).toThrow(/not in it/);
  });
});

describe('pagerKicker', () => {
  it('names examples and the index, generic otherwise', () => {
    expect(pagerKicker(first, 'prev')).toBe('PREVIOUS EXAMPLE');
    expect(pagerKicker(last, 'next')).toBe('NEXT EXAMPLE');
    expect(pagerKicker(EXAMPLES_INDEX, 'prev')).toBe('ALL EXAMPLES');
    expect(pagerKicker(EXAMPLES_INDEX, 'next')).toBe('UP NEXT');
    expect(pagerKicker(quickStart, 'prev')).toBe('PREVIOUS');
  });
});

describe('sourceUrl', () => {
  it('links the GitHub editor or blob view on main', () => {
    expect(sourceUrl('site/src/content/docs/quick-start.mdx', 'edit')).toBe(
      'https://github.com/Nemikolh/devsandboxes/edit/main/site/src/content/docs/quick-start.mdx',
    );
    expect(sourceUrl('npm/devsandboxes/index.d.ts', 'view')).toBe('https://github.com/Nemikolh/devsandboxes/blob/main/npm/devsandboxes/index.d.ts');
  });
});
