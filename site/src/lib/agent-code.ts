import type { Element, ElementContent, Root, RootContent } from 'hast';
import type { ShikiTransformer } from 'shiki';
import { type Agent, findAgent, termPattern } from './agents';
import { metaAttr, rawMeta } from './code-chrome';

/** An agent word: its colour, and a click opens the agent picker (`scripts/agent-picker.ts`). */
export function agentRef(agent: Agent, text: string): Element {
  return {
    type: 'element',
    tagName: 'button',
    properties: {
      type: 'button',
      className: ['agent-ref'],
      dataAgentTint: agent.id,
      dataAgentMenu: '',
      ariaHaspopup: 'menu',
      title: 'Switch coding agent',
    },
    children: [{ type: 'text', value: text }],
  };
}

/** Text split around the agent's terms, each match an `agentRef`. */
export function splitTerms(value: string, agent: Agent): ElementContent[] {
  const out: ElementContent[] = [];
  let last = 0;
  for (const m of value.matchAll(termPattern(agent))) {
    if (m.index > last) out.push({ type: 'text', value: value.slice(last, m.index) });
    out.push(agentRef(agent, m[0]));
    last = m.index + m[0].length;
  }
  if (last < value.length) out.push({ type: 'text', value: value.slice(last) });
  return out;
}

function wrapTerms(node: Element | Root, agent: Agent) {
  node.children = node.children.flatMap((child: RootContent): RootContent[] => {
    if (child.type === 'text') return splitTerms(child.value, agent);
    // Skip the chrome (label, copy button): only the code itself is agent text.
    if (child.type === 'element' && !(child.properties.className as string[] | undefined)?.includes('code-head'))
      wrapTerms(child, agent);
    return [child];
  });
}

/**
 * Shiki transformer for per-agent fences (` ```toml agent="codex" `): the
 * block only shows while that agent is picked, and its agent terms render as
 * agent refs. Must run after `codeChrome` so the `.code-block` wrapper exists.
 */
export function agentCode(): ShikiTransformer {
  return {
    name: 'devsandboxes:agent-code',
    root(root) {
      const id = metaAttr(rawMeta(this.options.meta), 'agent');
      if (id === undefined) return;
      const agent = findAgent(id);
      if (!agent) throw new Error(`code fence: unknown agent="${id}"`);
      const block = root.children.find((n): n is Element => n.type === 'element');
      if (block) block.properties.dataAgentOnly = agent.id;
      wrapTerms(root, agent);
    },
  };
}
