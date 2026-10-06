import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { anchorId, docHtml, inherited, linkableTypes, markdownFence, signatureCode, typeHtml, usedBy } from './api';
import { parseDts } from './dts';

const SAMPLE = `
/** Version. */
export declare const SCHEMA: 1;

// ----------------------------------------------------------------------------
// Payloads

/** Liveness. */
export type Status =
  | { state: 'running'; text: string }
  | { state: 'missing' };

export interface Base {
  /** Config root. */
  dir?: string;
}

export interface Opts extends Base {
  /** Reject with {@link Oops} on failure. */
  reject?: boolean;
}

export declare class Oops extends Error {
  readonly code: number | null;
}

// ----------------------------------------------------------------------------
// API

/** First. */
export declare function f(a: string): Promise<Status>;
/** Second. */
export declare function f(a: number, opts?: Base & { all?: boolean }): Promise<void>;
export declare function g<T = unknown>(name: string, opts?: Opts): Promise<T>;

export declare const svc: {
  /** Listing. */
  ls(opts?: Base): Promise<Status[]>;
};
`;

describe('parseDts', () => {
  const groups = parseDts(SAMPLE);

  it('keeps banner groups and declaration order', () => {
    expect(groups.map((g) => [g.title, g.decls.map((d) => `${d.kind}:${d.name}`)])).toEqual([
      [null, ['const:SCHEMA']],
      ['Payloads', ['type:Status', 'interface:Base', 'interface:Opts', 'class:Oops']],
      ['API', ['function:f', 'function:g', 'namespace:svc']],
    ]);
  });

  it('reads members, heritage and JSDoc', () => {
    const opts = groups[1].decls[2];
    expect(opts).toMatchObject({
      kind: 'interface',
      extends: ['Base'],
      members: [{ name: 'reject', type: 'boolean', optional: true, doc: 'Reject with {@link Oops} on failure.' }],
    });
    expect(groups[1].decls[3]).toMatchObject({
      kind: 'class',
      extends: ['Error'],
      members: [{ name: 'code', type: 'number | null', readonly: true, optional: false }],
    });
  });

  it('merges overloads and expands inline option objects', () => {
    const f = groups[2].decls[0];
    if (f.kind !== 'function') throw new Error('not a function');
    expect(f.signatures.map((s) => s.doc)).toEqual(['First.', 'Second.']);
    expect(f.signatures[1].params[1]).toMatchObject({
      name: 'opts',
      optional: true,
      type: 'Base & { all?: boolean }',
      inline: { bases: ['Base'], members: [{ name: 'all', type: 'boolean', optional: true }] },
    });
    expect(groups[2].decls[1]).toMatchObject({ signatures: [{ typeParams: '<T = unknown>', returns: 'Promise<T>' }] });
  });

  it('models a const object of methods as a namespace', () => {
    expect(groups[2].decls[2]).toMatchObject({
      kind: 'namespace',
      functions: [{ name: 'ls', signatures: [{ doc: 'Listing.', returns: 'Promise<Status[]>' }] }],
    });
  });

  it('fails on declarations it cannot render', () => {
    expect(() => parseDts('export declare enum E { A }')).toThrow(/index\.d\.ts:1: unsupported declaration/);
    expect(() => parseDts('interface Private {}')).toThrow(/non-exported/);
    expect(() => parseDts('export interface I { (x: number): void }')).toThrow(/unsupported member/);
  });

  it('parses the published typings', () => {
    const src = readFileSync(new URL('../../../npm/devsandboxes/index.d.ts', import.meta.url), 'utf8');
    const titles = parseDts(src).map((g) => g.title);
    expect(titles).toEqual([null, 'Payloads', 'Options & results', 'API', 'Daemon API']);
  });
});

describe('rendering helpers', () => {
  const groups = parseDts(SAMPLE);
  const known = linkableTypes(groups);

  it('links declared types and colours the rest', () => {
    expect(typeHtml("Array<[string, Status]> | 'x'", known)).toBe(
      '<span class="api-t-name">Array</span>&lt;[<span class="api-t-kw">string</span>, ' +
        '<a class="api-ref" href="#status">Status</a>]&gt; | <span class="api-t-str">\'x\'</span>',
    );
  });

  it('renders JSDoc code spans and links', () => {
    expect(docHtml('Reject with {@link Oops}.\nDefault `true`.', known)).toBe(
      '<p>Reject with <a href="#oops"><code>Oops</code></a>. Default <code>true</code>.</p>',
    );
    expect(docHtml('{@link Missing} a<b', known)).toBe('<p><code>Missing</code> a&lt;b</p>');
  });

  it('wraps long signatures one parameter per line', () => {
    const f = groups[2].decls[0];
    if (f.kind !== 'function') throw new Error('not a function');
    expect(signatureCode('f', f.signatures[0])).toBe('f(a: string): Promise<Status>');
    expect(signatureCode('f', f.signatures[1], 72, 'function ')).toBe(
      'function f(a: number, opts?: Base & { all?: boolean }): Promise<void>',
    );
    expect(signatureCode('f', f.signatures[1], 44, 'function ')).toBe(
      'function f(\n  a: number,\n  opts?: Base & { all?: boolean },\n): Promise<void>',
    );
  });

  it('indexes users and inherited members', () => {
    const users = usedBy(groups, known);
    expect(users.get('Base')).toEqual(['Opts', 'f', 'svc.ls']);
    expect(users.get('Status')).toEqual(['f', 'svc.ls']);
    expect(inherited(groups, 'Opts')).toEqual([{ from: 'Base', members: ['dir'] }]);
    expect(anchorId('svc.ls')).toBe('svc-ls');
  });

  it('extracts README fences', () => {
    expect(markdownFence('## A\n\n```ts\nx\n```\n## B\n', 'A', 'ts')).toBe('x\n');
    expect(() => markdownFence('## B\n```ts\nx\n```', 'A', 'ts')).toThrow();
  });
});
