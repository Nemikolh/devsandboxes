import { describe, expect, it } from 'vitest';
import { AGENT_PROMPTS } from './agent-prompts';
import { AGENTS, fillAgent } from './agents';
import { buildAgentPrompt, exampleNumber, type ExampleMeta, resolveRelated, sortExamples } from './examples';

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

describe('exampleNumber', () => {
  it('pads the 1-based position', () => {
    expect(exampleNumber(0)).toBe('01');
    expect(exampleNumber(11)).toBe('12');
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
  it('frames the steps with the shared skill, specifics, validate and safety items', () => {
    const prompt = buildAgentPrompt({ summary: 'X.', goal: 'Set up X.', steps: ['Edit A.', 'Edit B.'], verify: ['Run C.'] });
    const lines = prompt.split('\n');
    expect(lines[0]).toBe('Set up X.');
    expect(lines[1]).toBe('');
    expect(lines[2]).toMatch(/^1\. Read the devsandbox-cli and config-toml-spec skills/);
    expect(lines[2]).toContain('https://github.com/Nemikolh/devsandboxes/tree/main/skills');
    expect(lines[3]).toMatch(/^2\. Work out the specifics yourself.*Ask me only for what you cannot determine\.$/);
    expect(lines[4]).toBe('3. Edit A.');
    expect(lines[5]).toBe('4. Edit B.');
    expect(lines[6]).toMatch(/^5\. Validate with `devsandbox ls`/);
    expect(lines[7]).toBe('6. Run C.');
    expect(lines[8]).toMatch(/^7\. Ask me before anything destructive: devsandbox rm, gc, rebuild --force/);
    expect(lines).toHaveLength(9);
  });
});

describe('AGENT_PROMPTS', () => {
  // The reader pastes a prompt as copied, so nothing in it is left to fill in.
  it.each(Object.entries(AGENT_PROMPTS))('%s has no placeholder once copied', (_, spec) => {
    for (const agent of AGENTS) {
      expect(fillAgent(buildAgentPrompt(spec), agent)).not.toMatch(/<[^>\n]*>/);
    }
  });
});
