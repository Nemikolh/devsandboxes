// Pure helpers behind the examples collection: ordering, prev/next, related
// lookups and the agent prompt. No `astro:content` import, so vitest can load it.

/**
 * Lucide icons an example may name in its frontmatter. The schema accepts only
 * these, and `ExampleIcon.astro` maps each to its component through a
 * `Record<ExampleIconName, …>`, so a new name fails `astro check` until mapped.
 */
export const EXAMPLE_ICONS = [
  'git-branch',
  'database',
  'network',
  'hard-drive',
  'key-round',
  'plug',
  'code',
  'package',
  'git-pull-request',
  'file-code',
] as const;

export type ExampleIconName = (typeof EXAMPLE_ICONS)[number];

/** The slice of an example entry these helpers need. */
export interface ExampleMeta {
  id: string;
  data: {
    title: string;
    description: string;
    order: number;
    icon: ExampleIconName;
    tags: string[];
    related?: string[];
  };
}

/** Reading order: `order`, then slug, so equal orders stay deterministic. */
export function sortExamples<T extends ExampleMeta>(examples: T[]): T[] {
  return [...examples].sort((a, b) => a.data.order - b.data.order || a.id.localeCompare(b.id));
}

/** `EXAMPLE 03`-style number for the 0-based position in reading order. */
export function exampleNumber(index: number): string {
  return String(index + 1).padStart(2, '0');
}

export interface Neighbors<T> {
  prev?: T;
  next?: T;
}

/** Previous and next example in reading order (none past either end). */
export function exampleNeighbors<T>(sorted: T[], index: number): Neighbors<T> {
  return { prev: sorted[index - 1], next: sorted[index + 1] };
}

/**
 * Resolve `related` slugs against the collection. An unknown or self slug
 * throws, so a renamed example breaks the build instead of dropping a card.
 */
export function resolveRelated<T extends ExampleMeta>(entry: T, all: T[]): T[] {
  return (entry.data.related ?? []).map((slug) => {
    const found = all.find((e) => e.id === slug);
    if (!found) throw new Error(`example \`${entry.id}\`: related slug \`${slug}\` is not an example`);
    if (found === entry) throw new Error(`example \`${entry.id}\` lists itself as related`);
    return found;
  });
}

export const SKILLS_URL = 'https://github.com/Nemikolh/devsandboxes/tree/main/skills';

export interface AgentPromptSpec {
  /** What to set up, one or two sentences. */
  goal: string;
  /** The use-case-specific edits, in order. */
  steps: string[];
  /** How to bring it up and check it once `devsandbox ls` is clean. */
  verify: string[];
}

/**
 * The full prompt: the use-case steps framed by the parts every example
 * shares (read the skills, find the config dir, validate, ask before
 * destructive actions), so all prompts stay consistent.
 */
export function buildAgentPrompt(spec: AgentPromptSpec): string {
  return [spec.goal, '', ...agentPromptItems(spec).map((s, i) => `${i + 1}. ${s}`)].join('\n');
}

/** The numbered items of `buildAgentPrompt`, for rendering them as a list. */
export function agentPromptItems({ steps, verify }: AgentPromptSpec): string[] {
  return [
    `Read the devsandbox-cli and config-toml-spec skills first (load them if they are installed, otherwise read them from ${SKILLS_URL}). Follow them over your own assumptions: unknown config keys are hard errors.`,
    'Find my devsandbox config dir: the folder holding config.toml, passed to devsandbox as `-C <config dir>`. If you cannot tell which one, ask me.',
    ...steps,
    'Validate with `devsandbox -C <config dir> ls` and fix every error it reports before going on.',
    ...verify,
    'Ask me before anything destructive: devsandbox rm, gc, rebuild --force, service rebuild, deleting files or branches.',
  ];
}

export interface PromptPart {
  text: string;
  placeholder: boolean;
}

/** Split prompt text into plain runs and `<placeholder>` runs (kept with brackets). */
export function promptParts(text: string): PromptPart[] {
  return text
    .split(/(<[a-z][^<>\n]*>)/i)
    .filter((t) => t !== '')
    .map((t) => ({ text: t, placeholder: /^<[a-z][^<>\n]*>$/i.test(t) }));
}

export interface PromptRun {
  code: boolean;
  parts: PromptPart[];
}

/** Backtick runs (odd ones are `code`), each split by `promptParts`. */
export function promptRuns(text: string): PromptRun[] {
  return text
    .split('`')
    .map((run, i) => ({ code: i % 2 === 1, parts: promptParts(run) }))
    .filter((r) => r.parts.length > 0);
}
