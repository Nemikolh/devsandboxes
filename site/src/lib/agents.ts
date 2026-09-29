// The coding agents a reader can pick. The pick is one page-wide setting
// (`<html data-agent>`, persisted), so an example shows every agent-specific
// block, word and prompt for that agent only. Adding an agent: an entry here,
// its `--agent-<id>` colour in tokens.css and its hide rule in components.css.

export interface Agent {
  id: string;
  /** Product name, in prose. */
  name: string;
  /** The command that starts it, in code. */
  cmd: string;
  /**
   * Agent-specific strings highlighted in `agent="<id>"` code fences (package,
   * config dir, env var…); matched whole, longest first.
   */
  terms: string[];
}

export const AGENTS: Agent[] = [
  {
    id: 'claude',
    name: 'Claude Code',
    cmd: 'claude',
    terms: ['Claude Code', '@anthropic-ai/claude-code', 'CLAUDE_CONFIG_DIR', '.claude', 'claude'],
  },
  {
    id: 'codex',
    name: 'Codex',
    cmd: 'codex',
    terms: ['Codex', '@openai/codex', '.codex', 'codex'],
  },
];

export const DEFAULT_AGENT = AGENTS[0];
export const AGENT_STORAGE_KEY = 'devsandboxes.agent';

export function findAgent(id: string | null | undefined): Agent | undefined {
  return AGENTS.find((a) => a.id === id);
}

const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/** Matches any of the agent's terms, not glued to a longer word (`claude` in `claudes`). */
export function termPattern(agent: Agent): RegExp {
  const alts = [...agent.terms].sort((a, b) => b.length - a.length).map(escape);
  return new RegExp(`(?<![\\w-])(?:${alts.join('|')})(?![\\w-])`, 'g');
}

/** The `<agent>` placeholder of agent prompts. */
export const AGENT_PLACEHOLDER = '<agent>';

/**
 * Prompt text for one agent: `<agent>` becomes its command inside `code`
 * runs (odd backtick runs) and its name elsewhere.
 */
export function fillAgent(text: string, agent: Agent): string {
  return text
    .split('`')
    .map((run, i) => run.replaceAll(AGENT_PLACEHOLDER, i % 2 === 1 ? agent.cmd : agent.name))
    .join('`');
}
