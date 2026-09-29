import { describe, expect, it } from 'vitest';
import { splitTerms } from './agent-code';
import { AGENTS, fillAgent, findAgent } from './agents';

const claude = findAgent('claude')!;
const codex = findAgent('codex')!;

const matches = (text: string, agent = claude) =>
  splitTerms(text, agent).flatMap((n) =>
    n.type === 'element' && n.children[0]?.type === 'text' ? [n.children[0].value] : [],
  );

describe('agents', () => {
  it('has unique ids', () => {
    expect(new Set(AGENTS.map((a) => a.id)).size).toBe(AGENTS.length);
  });

  it('fills <agent> with the command in code and the name in prose', () => {
    expect(fillAgent('Install <agent>, then run `<agent> --version`.', codex)).toBe(
      'Install Codex, then run `codex --version`.',
    );
  });
});

describe('splitTerms', () => {
  it('prefers the longest term', () => {
    expect(matches('npm install -g @anthropic-ai/claude-code')).toEqual(['@anthropic-ai/claude-code']);
    expect(matches('target=/root/.claude,type=bind')).toEqual(['.claude']);
  });

  it('skips terms glued to a longer word', () => {
    expect(matches('claudes claude-ish xclaude')).toEqual([]);
    expect(matches('exec -it web claude')).toEqual(['claude']);
  });

  it('keeps the text around matches', () => {
    const nodes = splitTerms('run codex now', codex);
    expect(nodes.map((n) => (n.type === 'text' ? n.value : 'REF'))).toEqual(['run ', 'REF', ' now']);
  });
});
