import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { z } from 'astro/zod';
import { EXAMPLE_ICONS } from './lib/examples';

// Hand-written docs pages, routed to `/docs/<id>` by `pages/docs/[...slug].astro`.
const docs = defineCollection({
  loader: glob({ base: './src/content/docs', pattern: '**/*.{md,mdx}' }),
  schema: z.object({
    title: z.string(),
    description: z.string(),
    eyebrow: z.string(),
    /** Reading order within the docs (lower first). */
    order: z.number(),
  }),
});

// The config reference: the agent skill *is* the spec, so the site renders it
// as-is (`pages/docs/config.astro`) instead of keeping a second copy.
const spec = defineCollection({
  loader: glob({ base: '../skills/config-toml-spec', pattern: 'SKILL.md' }),
  schema: z.object({
    name: z.string(),
    description: z.string(),
  }),
});

// One MDX file per use case, routed to `/examples/<id>` by
// `pages/examples/[slug].astro` through `layouts/Example.astro`.
const examples = defineCollection({
  loader: glob({ base: './src/content/examples', pattern: '*.mdx' }),
  schema: z.object({
    title: z.string(),
    /** One line: card text, lead and meta description. */
    description: z.string(),
    /** Reading order (lower first); also the `EXAMPLE 0N` number. */
    order: z.number(),
    icon: z.enum(EXAMPLE_ICONS),
    tags: z.array(z.string()).min(1),
    /** Slugs of other examples; unknown ones fail the build. */
    related: z.array(z.string()).optional(),
  }),
});

export const collections = { docs, spec, examples };
