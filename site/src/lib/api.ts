import type { Element, ElementContent, Root, RootContent } from 'hast';
import type { ShikiTransformer } from 'shiki';
import type { Decl, FunctionDecl, Group, Member, Param, Signature } from './dts';

// Pure rendering helpers for the Node API page (src/pages/docs/node-api.astro):
// anchors, type-text highlighting with links, JSDoc → HTML, signature layout.

/** Anchor id of a declaration (`InstanceRow` → `instancerow`, `service.ls` → `service-ls`). */
export function anchorId(name: string): string {
  return name.toLowerCase().replace(/[^a-z0-9]+/g, '-');
}

export const escapeHtml = (s: string) =>
  s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

/** Names that get their own anchor on the page (types, classes), for linking. */
export function linkableTypes(groups: Group[]): Map<string, string> {
  const out = new Map<string, string>();
  for (const d of groups.flatMap((g) => g.decls)) {
    if (d.kind === 'interface' || d.kind === 'type' || d.kind === 'class') out.set(d.name, anchorId(d.name));
  }
  return out;
}

// Coloured like the Shiki theme colours the same words in signatures.
const KEYWORDS = new Set([
  'string', 'number', 'boolean', 'bigint', 'symbol', 'object', 'void', 'unknown', 'never', 'any',
  'readonly', 'keyof', 'typeof',
]);
const LITERALS = new Set(['null', 'undefined', 'true', 'false']);

export type TypeToken = { kind: 'ref' | 'kw' | 'lit' | 'str' | 'num' | 'name' | 'punct'; text: string };

/** Lex a type expression just enough to colour it and find type references. */
export function typeTokens(text: string, known: ReadonlyMap<string, string>): TypeToken[] {
  const out: TypeToken[] = [];
  const re = /('(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*")|(\d+(?:\.\d+)?)|([A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*)|(\s+|[^\w\s'"$]+)/g;
  for (const m of text.matchAll(re)) {
    const [t, str, num, ident] = m;
    if (str) out.push({ kind: 'str', text: t });
    else if (num) out.push({ kind: 'num', text: t });
    else if (ident) {
      out.push({ kind: KEYWORDS.has(t) ? 'kw' : LITERALS.has(t) ? 'lit' : known.has(t) ? 'ref' : 'name', text: t });
    }
    else out.push({ kind: 'punct', text: t });
  }
  return out;
}

/** Declared type names referenced by a type expression, in order, deduplicated. */
export function typeRefs(text: string, known: ReadonlyMap<string, string>): string[] {
  return [...new Set(typeTokens(text, known).filter((t) => t.kind === 'ref').map((t) => t.text))];
}

/** A type expression as inline HTML (`<code>` contents): coloured, declared types linked. */
export function typeHtml(text: string, known: ReadonlyMap<string, string>): string {
  return typeTokens(text, known)
    .map(({ kind, text: t }) =>
      kind === 'ref'
        ? `<a class="api-ref" href="#${known.get(t)}">${escapeHtml(t)}</a>`
        : kind === 'punct'
          ? escapeHtml(t)
          : `<span class="api-t-${kind}">${escapeHtml(t)}</span>`,
    )
    .join('');
}

/** Inline JSDoc markup: `code` spans and `{@link Name}` (linked when declared). */
function inlineDocHtml(text: string, known: ReadonlyMap<string, string>): string {
  return text
    .split(/(`[^`]*`|\{@link(?:code|plain)?\s+[^}\s]+(?:\s+[^}]*)?\})/)
    .map((part, i) => {
      if (i % 2 === 0) return escapeHtml(part);
      if (part.startsWith('`')) {
        const code = part.slice(1, -1);
        const html = `<code>${escapeHtml(code)}</code>`;
        return known.has(code) ? `<a href="#${known.get(code)}">${html}</a>` : html;
      }
      const [, target, label] = part.match(/^\{@link\w*\s+([^}\s]+)(?:\s+([^}]*))?\}$/)!;
      const inner = `<code>${escapeHtml(label?.trim() || target)}</code>`;
      return known.has(target) ? `<a href="#${known.get(target)}">${inner}</a>` : inner;
    })
    .join('');
}

/** A JSDoc comment as HTML paragraphs (blank lines separate them). */
export function docHtml(doc: string, known: ReadonlyMap<string, string>): string {
  return doc
    .split(/\n\s*\n/)
    .map((p) => p.replace(/\s*\n\s*/g, ' ').trim())
    .filter(Boolean)
    .map((p) => `<p>${inlineDocHtml(p, known)}</p>`)
    .join('');
}

/** First sentence of a JSDoc comment, inline (for the overview table). */
export function summaryHtml(doc: string, known: ReadonlyMap<string, string>): string {
  const flat = doc.replace(/\s+/g, ' ').trim();
  const m = flat.match(/^.*?[.;](?=\s|$)/);
  return inlineDocHtml((m ? m[0] : flat).replace(/[.;]$/, ''), known);
}

function paramText(p: Param): string {
  return `${p.rest ? '...' : ''}${p.name}${p.optional ? '?' : ''}: ${p.type}`;
}

/** Characters that fit the article column on desktop / on a phone. */
export const WIDE = 84;
export const NARROW = 44;

/**
 * `name<T>(a: A, b?: B): R`, one parameter per line once `prefix` + the
 * signature outgrows `max` characters.
 */
export function signatureCode(name: string, sig: Signature, max = WIDE, prefix = ''): string {
  const head = `${prefix}${name}${sig.typeParams}(`;
  const tail = `): ${sig.returns}`;
  const flat = `${head}${sig.params.map(paramText).join(', ')}${tail}`;
  if (flat.length <= max || sig.params.length === 0) return flat;
  return `${head}\n${sig.params.map((p) => `  ${paramText(p)},`).join('\n')}\n${tail}`;
}

/** `Promise<X>` → `X`: what an async API call resolves to. */
export function resolvesTo(returns: string): string {
  const m = returns.match(/^Promise<(.*)>$/);
  return m ? m[1] : returns;
}

/** Every function-like declaration with its qualified name (`service.ls`). */
export function allFunctions(groups: Group[]): { qualified: string; fn: FunctionDecl }[] {
  return groups
    .flatMap((g) => g.decls)
    .flatMap((d) =>
      d.kind === 'function'
        ? [{ qualified: d.name, fn: d }]
        : d.kind === 'namespace'
          ? d.functions.map((fn) => ({ qualified: `${d.name}.${fn.name}`, fn }))
          : [],
    );
}

/** Type texts a declaration mentions (members, params, returns, heritage). */
function mentionedTypes(d: Decl): string[] {
  switch (d.kind) {
    case 'interface':
    case 'class':
      return [...d.extends, ...d.members.map((m) => m.type)];
    case 'type':
    case 'const':
      return [d.type];
    case 'function':
      return d.signatures.flatMap((s) => [...s.params.map((p) => p.type), s.returns]);
    case 'namespace':
      return d.functions.flatMap(mentionedTypes);
  }
}

/**
 * For each declared type, the declarations that mention it (functions by
 * qualified name, `service.ls`), in source order.
 */
export function usedBy(groups: Group[], known: ReadonlyMap<string, string>): Map<string, string[]> {
  const out = new Map<string, string[]>();
  const add = (type: string, user: string) => {
    const list = out.get(type) ?? [];
    if (type !== user && !list.includes(user)) list.push(user);
    out.set(type, list);
  };
  for (const d of groups.flatMap((g) => g.decls)) {
    const users =
      d.kind === 'namespace' ? d.functions.map((fn) => ({ name: `${d.name}.${fn.name}`, d: fn as Decl })) : [{ name: d.name, d }];
    for (const u of users) for (const t of mentionedTypes(u.d).flatMap((x) => typeRefs(x, known))) add(t, u.name);
  }
  return out;
}

/** Members an interface inherits through `extends`, by base, following chains. */
export function inherited(groups: Group[], name: string): { from: string; members: string[] }[] {
  const decls = groups.flatMap((g) => g.decls);
  const find = (n: string) => decls.find((d) => (d.kind === 'interface' || d.kind === 'class') && d.name === n);
  const out: { from: string; members: string[] }[] = [];
  const walk = (n: string, seen: Set<string>) => {
    const d = find(n);
    if (!d || (d.kind !== 'interface' && d.kind !== 'class')) return;
    for (const base of d.extends) {
      const b = find(base);
      if (!b || seen.has(base) || (b.kind !== 'interface' && b.kind !== 'class')) continue;
      seen.add(base);
      out.push({ from: base, members: b.members.map((m) => m.name) });
      walk(base, seen);
    }
  };
  walk(name, new Set([name]));
  return out;
}

/** One row of a parameter / property table; `doc` is HTML. */
export interface Row {
  name: string;
  /** Member of an inline options object (`opts.all`), indented under it. */
  sub?: boolean;
  optional: boolean;
  readonly?: boolean;
  type: string;
  doc: string;
}

/** `<code>` list of names, declared ones linked. */
export function linkList(names: string[], known: ReadonlyMap<string, string>): string {
  return names
    .map((n) => (known.has(n) ? `<a href="#${known.get(n)}"><code>${escapeHtml(n)}</code></a>` : `<code>${escapeHtml(n)}</code>`))
    .join(', ');
}

/** Parameter rows, with inline option objects expanded as `opts.x` rows. */
export function paramRows(sig: Signature, known: ReadonlyMap<string, string>): Row[] {
  return sig.params.flatMap((p): Row[] => {
    const head: Row = { name: p.rest ? `...${p.name}` : p.name, optional: p.optional, type: p.type, doc: docHtml(p.doc, known) };
    if (!p.inline) return [head];
    if (!head.doc) {
      head.doc = p.inline.bases.length ? `<p>Every option of ${linkList(p.inline.bases, known)}, plus:</p>` : '<p>An object with:</p>';
    }
    return [
      head,
      ...p.inline.members.map((m) => ({
        name: `${p.name}.${m.name}`,
        sub: true,
        optional: m.optional,
        type: m.type,
        doc: docHtml(m.doc, known),
      })),
    ];
  });
}

export function memberRows(members: Member[], known: ReadonlyMap<string, string>): Row[] {
  return members.map((m) => ({ name: m.name, optional: m.optional, readonly: m.readonly, type: m.type, doc: docHtml(m.doc, known) }));
}

/** The first fenced block of `lang` under the `## heading` of a Markdown file. */
export function markdownFence(md: string, heading: string, lang: string): string {
  const section = md.split(/^## /m).find((s) => s.startsWith(`${heading}\n`));
  const m = section?.match(new RegExp('^```' + lang + '\\n([\\s\\S]*?)^```', 'm'));
  if (!m) throw new Error(`README: no \`\`\`${lang} block under "## ${heading}"`);
  return m[1];
}

/** The Markdown paragraph starting with `start`. */
export function markdownParagraph(md: string, start: string): string {
  const p = md.split(/\n\s*\n/).find((s) => s.trimStart().startsWith(start));
  if (!p) throw new Error(`README: no paragraph starting "${start}"`);
  return p.trim();
}

function isElement(n: RootContent | ElementContent): n is Element {
  return n.type === 'element';
}

/**
 * Shiki transformer: declared type names in highlighted code become links to
 * their anchors. Works on the text, not on tokens, because Shiki merges
 * adjacent same-coloured tokens (`ServiceRow[]` is one span).
 */
export function linkTypes(known: ReadonlyMap<string, string>): ShikiTransformer {
  const re = /\b[A-Za-z_$][\w$]*\b/g;
  const visit = (node: Element) => {
    node.children = node.children.flatMap((child): ElementContent[] => {
      if (isElement(child)) {
        visit(child);
        return [child];
      }
      if (child.type !== 'text') return [child];
      const parts: ElementContent[] = [];
      let last = 0;
      for (const m of child.value.matchAll(re)) {
        const id = known.get(m[0]);
        if (!id) continue;
        if (m.index > last) parts.push({ type: 'text', value: child.value.slice(last, m.index) });
        parts.push({
          type: 'element',
          tagName: 'a',
          properties: { href: `#${id}`, className: ['api-ref'] },
          children: [{ type: 'text', value: m[0] }],
        });
        last = m.index + m[0].length;
      }
      if (!parts.length) return [child];
      if (last < child.value.length) parts.push({ type: 'text', value: child.value.slice(last) });
      return parts;
    });
  };
  return {
    name: 'devsandboxes:link-types',
    root(root: Root) {
      for (const n of root.children) if (isElement(n)) visit(n);
    },
  };
}

/**
 * Shiki transformer keeping only lines `from..to` (1-based, inclusive): lets a
 * fragment (a method signature) be highlighted inside the context that gives
 * it the right grammar (`declare const x: { … }`) without showing the context.
 */
export function keepLines(from: number, to: number): ShikiTransformer {
  return {
    name: 'devsandboxes:keep-lines',
    code(code) {
      const lines = code.children.filter(isElement);
      const kept = lines.slice(from - 1, to);
      code.children = kept.flatMap((l, i): ElementContent[] => (i ? [{ type: 'text', value: '\n' }, l] : [l]));
    },
  };
}
