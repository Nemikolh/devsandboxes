import { describe, expect, it } from 'vitest';
import {
  buildAgentPrompt,
  exampleNeighbors,
  exampleNumber,
  type ExampleMeta,
  promptParts,
  promptRuns,
  resolveRelated,
  sortExamples,
} from './examples';

const ex = (id: string, order: number, related?: string[]): ExampleMeta => ({
  id,
  data: { title: id, description: `${id} desc`, order, icon: 'database', tags: [], related },
});

describe('sortExamples', () => {
  it('orders by `order`, then slug, without mutating the input', () => {
    const input = [ex('c', 2), ex('b', 1), ex('a', 2)];
    expect(sortExamples(input).map((e) => e.id)).toEqual(['b', 'a', 'c']);
    expect(input.map((e) => e.id)).toEqual(['c', 'b', 'a']);
  });
});

describe('exampleNumber / exampleNeighbors', () => {
  it('pads the 1-based position', () => {
    expect(exampleNumber(0)).toBe('01');
    expect(exampleNumber(11)).toBe('12');
  });

  it('has no prev on the first and no next on the last', () => {
    const list = ['a', 'b', 'c'];
    expect(exampleNeighbors(list, 0)).toEqual({ prev: undefined, next: 'b' });
    expect(exampleNeighbors(list, 1)).toEqual({ prev: 'a', next: 'c' });
    expect(exampleNeighbors(list, 2)).toEqual({ prev: 'b', next: undefined });
  });
});

describe('resolveRelated', () => {
  it('returns entries in the listed order', () => {
    const all = [ex('a', 1, ['c', 'b']), ex('b', 2), ex('c', 3)];
    expect(resolveRelated(all[0], all).map((e) => e.id)).toEqual(['c', 'b']);
    expect(resolveRelated(all[1], all)).toEqual([]);
  });

  it('throws on an unknown or self slug', () => {
    const all = [ex('a', 1, ['nope']), ex('b', 2, ['b'])];
    expect(() => resolveRelated(all[0], all)).toThrow(/`nope` is not an example/);
    expect(() => resolveRelated(all[1], all)).toThrow(/lists itself/);
  });
});

describe('buildAgentPrompt', () => {
  it('frames the steps with the shared skill, validate and safety items', () => {
    const prompt = buildAgentPrompt({ goal: 'Set up X.', steps: ['Edit A.', 'Edit B.'], verify: ['Run C.'] });
    const lines = prompt.split('\n');
    expect(lines[0]).toBe('Set up X.');
    expect(lines[1]).toBe('');
    expect(lines[2]).toMatch(/^1\. Read the devsandbox-cli and config-toml-spec skills/);
    expect(lines[2]).toContain('https://github.com/Nemikolh/devsandboxes/tree/main/skills');
    expect(lines[3]).toMatch(/^2\. Find my devsandbox config dir.*ask me/);
    expect(lines[4]).toBe('3. Edit A.');
    expect(lines[5]).toBe('4. Edit B.');
    expect(lines[6]).toMatch(/^5\. Validate with `devsandbox -C <config dir> ls`/);
    expect(lines[7]).toBe('6. Run C.');
    expect(lines[8]).toMatch(/^7\. Ask me before anything destructive: devsandbox rm, gc, rebuild --force/);
    expect(lines).toHaveLength(9);
  });
});

describe('promptParts', () => {
  it('splits out <placeholders>, keeping the brackets', () => {
    expect(promptParts('cd <repo path> && ls')).toEqual([
      { text: 'cd ', placeholder: false },
      { text: '<repo path>', placeholder: true },
      { text: ' && ls', placeholder: false },
    ]);
  });

  it('leaves comparisons and non-word brackets alone', () => {
    expect(promptParts('a < b and c > d')).toEqual([{ text: 'a < b and c > d', placeholder: false }]);
    expect(promptParts('<config dir>')).toEqual([{ text: '<config dir>', placeholder: true }]);
  });
});

describe('promptRuns', () => {
  it('marks backtick runs as code and finds placeholders inside them', () => {
    expect(promptRuns('Run `ls -C <dir>` now')).toEqual([
      { code: false, parts: [{ text: 'Run ', placeholder: false }] },
      {
        code: true,
        parts: [
          { text: 'ls -C ', placeholder: false },
          { text: '<dir>', placeholder: true },
        ],
      },
      { code: false, parts: [{ text: ' now', placeholder: false }] },
    ]);
    expect(promptRuns('`a`')).toEqual([{ code: true, parts: [{ text: 'a', placeholder: false }] }]);
  });
});
