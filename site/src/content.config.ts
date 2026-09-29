import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { z } from 'astro/zod';

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

export const collections = { docs };
