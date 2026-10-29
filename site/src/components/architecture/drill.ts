/**
 * The data behind the drill-down components (`Drill`, `Deck`,
 * `CommandTable`): a card read high-level first, each layer one level deeper
 * (what → how → module → code), its sources linked at the end.
 */
import { REPO_URL } from '../../lib/nav';

/** A file in the repo; `rev` pins a commit or tag (default `main`). */
export type SourceLink = { label: string; path: string; rev?: string };

/** A code excerpt; `title` is its path, `rev` shown as a badge when set. */
export type Snippet = { title: string; lang: string; code: string; rev?: string };

/** One level of detail. `body` is short trusted HTML (inline code, em, a, ul). */
export type Layer = { label: string; body: string; snippets?: Snippet[] };

/** `summary` is short trusted HTML; `tag` an eyebrow (`7716cb8`, `Step 7`). */
export type Card = { id: string; title: string; summary: string; tag?: string; layers?: Layer[]; links?: SourceLink[] };

/** GitHub blob URL of `path` at `rev` (`sourceUrl` in `nav.ts` only knows main). */
export function blobUrl(path: string, rev?: string): string {
  return `${REPO_URL}/blob/${rev ?? 'main'}/${path}`;
}

/** A commit-looking rev shortened for a badge; tags and branches as-is. */
export function shortRev(rev: string): string {
  return /^[0-9a-f]{12,40}$/.test(rev) ? rev.slice(0, 7) : rev;
}
