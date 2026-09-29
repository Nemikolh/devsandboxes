import type { Element } from 'hast';
import { codeToHast } from 'shiki';
import { describe, expect, it } from 'vitest';
import { codeChrome, codeId, countLines, metaFlag } from './code-chrome';

describe('metaFlag', () => {
  it('matches a bare word anywhere in the meta', () => {
    expect(metaFlag('collapsed', 'collapsed')).toBe(true);
    expect(metaFlag('title="a.sh" agent="claude" collapsed', 'collapsed')).toBe(true);
    expect(metaFlag('collapsed title="a.sh"', 'collapsed')).toBe(true);
  });

  it('ignores partial words, quoted values and key=value attrs', () => {
    expect(metaFlag(undefined, 'collapsed')).toBe(false);
    expect(metaFlag('uncollapsed collapsedx', 'collapsed')).toBe(false);
    expect(metaFlag('title="collapsed demo"', 'collapsed')).toBe(false);
    expect(metaFlag("title='x collapsed'", 'collapsed')).toBe(false);
    expect(metaFlag('collapsed="no"', 'collapsed')).toBe(false);
  });
});

describe('codeId', () => {
  it('is stable and differs per code', () => {
    expect(codeId('a')).toBe(codeId('a'));
    expect(codeId('a')).not.toBe(codeId('b'));
    expect(codeId('a')).toMatch(/^code-[0-9a-z]+$/);
  });
});

const lines = (n: number) => Array.from({ length: n }, (_, i) => `echo ${i}`).join('\n');

async function render(code: string, meta: string) {
  const root = await codeToHast(code, {
    lang: 'sh',
    theme: 'github-dark',
    meta: { __raw: meta },
    transformers: [codeChrome()],
  });
  return root.children[0] as Element;
}

const pre = (block: Element) => block.children.find((n): n is Element => n.type === 'element' && n.tagName === 'pre')!;

describe('codeChrome collapsed', () => {
  it('counts one line per source line', async () => {
    expect(countLines(pre(await render(lines(12), '')))).toBe(12);
  });

  it('adds the toggle to a long collapsed block', async () => {
    const block = await render(lines(12), 'collapsed');
    expect(block.properties.dataCollapsed).toBe('');
    expect(block.properties.dataLines).toBe('12');
    const foot = block.children.at(-1) as Element;
    const button = foot.children[0] as Element;
    expect(button.properties.ariaExpanded).toBe('false');
    expect(button.properties.ariaControls).toEqual([pre(block).properties.id]);
  });

  it('leaves short or unflagged blocks alone', async () => {
    for (const block of [await render(lines(10), 'collapsed'), await render(lines(12), '')]) {
      expect(block.properties.dataCollapsed).toBeUndefined();
      expect(block.children).toHaveLength(2);
    }
  });
});
