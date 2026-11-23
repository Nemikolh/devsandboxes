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
  'hard-drive',
  'key-round',
  'plug',
  'file-code',
  'globe-lock',
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
  /** What the prompt sets up, a few words, shown on the copy button. */
  summary: string;
  /** What to set up, one or two sentences. */
  goal: string;
  /** The use-case-specific edits, in order. */
  steps: string[];
  /** How to bring it up and check it once `devsandbox ls` is clean. */
  verify: string[];
}

/**
 * The full prompt: the use-case steps framed by the parts every example
 * shares (read the skills, work out the specifics, validate, ask before
 * destructive actions), so all prompts stay consistent.
 */
export function buildAgentPrompt({ goal, steps, verify }: AgentPromptSpec): string {
  const items = [
    `Read the devsandbox-cli and config-toml-spec skills first (load them if they are installed, otherwise read them from ${SKILLS_URL}). Follow them over your own assumptions: unknown config keys are hard errors.`,
    "Work out the specifics yourself: the repo is the one you are running in; my devsandbox config dir is the folder holding devsandboxes.toml (devsandbox runs in it, or takes it with `-C`), usually the one whose sandboxes' `folder` points at this repo; `devsandbox ls` there lists its sandboxes and `devsandbox ps -a` the instances and services that exist. Pick names that fit the ones already there. Ask me only for what you cannot determine.",
    ...steps,
    'Validate with `devsandbox ls` in the config dir and fix every error it reports before going on.',
    ...verify,
    'Ask me before anything destructive: devsandbox rm, gc, rebuild --force, service rebuild, deleting files or branches.',
  ];
  return [goal, '', ...items.map((s, i) => `${i + 1}. ${s}`)].join('\n');
}
