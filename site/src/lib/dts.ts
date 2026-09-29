import ts from 'typescript';

// A declaration-file model for the Node API page, parsed straight from
// `npm/devsandboxes/index.d.ts` (no Program: the file is self-contained and a
// syntax-level read keeps the build fast). Anything the renderer wouldn't know
// how to show throws instead of being dropped, so the page can't silently lag
// behind the typings.

export interface Member {
  name: string;
  /** Source text of the type, whitespace-collapsed. */
  type: string;
  optional: boolean;
  readonly: boolean;
  doc: string;
}

export interface Param {
  name: string;
  type: string;
  optional: boolean;
  rest: boolean;
  doc: string;
  /**
   * Set when the type spells out an object literal (`CommonOptions & { all?:
   * boolean }`): the named types it intersects with and the literal's members,
   * so the page can list the extra options instead of an opaque type.
   */
  inline?: { bases: string[]; members: Member[] };
}

export interface Signature {
  /** `<T = unknown>`, or empty. */
  typeParams: string;
  params: Param[];
  returns: string;
  doc: string;
}

export interface FunctionDecl {
  kind: 'function';
  name: string;
  /** One per overload, in source order. */
  signatures: Signature[];
}

export type Decl =
  | { kind: 'const'; name: string; type: string; doc: string }
  | { kind: 'type'; name: string; typeParams: string; type: string; doc: string }
  | { kind: 'interface'; name: string; typeParams: string; extends: string[]; members: Member[]; doc: string }
  | { kind: 'class'; name: string; extends: string[]; members: Member[]; doc: string }
  | FunctionDecl
  /** An exported const object of methods (`service.ls()`), shown like a namespace. */
  | { kind: 'namespace'; name: string; functions: FunctionDecl[]; doc: string };

export interface Group {
  /** From the file's `// ----` / `// Title` banner; null before the first one. */
  title: string | null;
  decls: Decl[];
}

class DtsError extends Error {
  constructor(sf: ts.SourceFile, node: ts.Node, what: string) {
    const { line } = sf.getLineAndCharacterOfPosition(node.getStart(sf));
    super(`${sf.fileName}:${line + 1}: ${what} (${ts.SyntaxKind[node.kind]}); teach src/lib/dts.ts about it`);
  }
}

const collapse = (s: string) => s.replace(/\s+/g, ' ').trim();

function docOf(node: ts.Node): string {
  const docs = ts.getJSDocCommentsAndTags(node).filter(ts.isJSDoc);
  const last = docs.at(-1);
  return last ? (ts.getTextOfJSDocComment(last.comment) ?? '').trim() : '';
}

function paramDoc(p: ts.ParameterDeclaration): string {
  const tag = ts.getJSDocParameterTags(p)[0];
  return tag ? (ts.getTextOfJSDocComment(tag.comment) ?? '').replace(/^-\s*/, '').trim() : '';
}

function hasModifier(node: ts.Node, kind: ts.SyntaxKind): boolean {
  return (ts.canHaveModifiers(node) && ts.getModifiers(node)?.some((m) => m.kind === kind)) || false;
}

function typeParamsOf(sf: ts.SourceFile, node: { typeParameters?: ts.NodeArray<ts.TypeParameterDeclaration> }): string {
  return node.typeParameters?.length ? `<${node.typeParameters.map((t) => t.getText(sf)).join(', ')}>` : '';
}

function typeText(sf: ts.SourceFile, owner: ts.Node, type: ts.TypeNode | undefined): string {
  if (!type) throw new DtsError(sf, owner, 'missing type annotation');
  return collapse(type.getText(sf));
}

function nameText(sf: ts.SourceFile, node: ts.Node, name: ts.PropertyName | ts.BindingName | undefined): string {
  if (!name || !(ts.isIdentifier(name) || ts.isStringLiteral(name))) throw new DtsError(sf, node, 'unsupported member name');
  return name.text;
}

function membersOf(sf: ts.SourceFile, elements: readonly (ts.TypeElement | ts.ClassElement)[]): Member[] {
  return elements.map((m) => {
    if (!ts.isPropertySignature(m) && !ts.isPropertyDeclaration(m)) throw new DtsError(sf, m, 'unsupported member');
    return {
      name: nameText(sf, m, m.name),
      type: typeText(sf, m, m.type),
      optional: !!m.questionToken,
      readonly: hasModifier(m, ts.SyntaxKind.ReadonlyKeyword),
      doc: docOf(m),
    };
  });
}

function inlineOf(sf: ts.SourceFile, type: ts.TypeNode | undefined): Param['inline'] {
  if (!type) return undefined;
  const parts = ts.isIntersectionTypeNode(type) ? type.types : [type];
  const bases: string[] = [];
  const members: Member[] = [];
  for (const t of parts) {
    if (ts.isTypeLiteralNode(t)) members.push(...membersOf(sf, t.members));
    else if (ts.isTypeReferenceNode(t)) bases.push(t.getText(sf));
    else return undefined;
  }
  return members.length ? { bases, members } : undefined;
}

function signatureOf(sf: ts.SourceFile, node: ts.FunctionDeclaration | ts.MethodSignature): Signature {
  return {
    typeParams: typeParamsOf(sf, node),
    params: node.parameters.map((p) => ({
      name: nameText(sf, p, p.name),
      type: typeText(sf, p, p.type),
      optional: !!p.questionToken || !!p.initializer,
      rest: !!p.dotDotDotToken,
      doc: paramDoc(p),
      inline: inlineOf(sf, p.type),
    })),
    returns: typeText(sf, node, node.type),
    doc: docOf(node),
  };
}

function extendsOf(sf: ts.SourceFile, node: ts.InterfaceDeclaration | ts.ClassDeclaration): string[] {
  const out: string[] = [];
  for (const clause of node.heritageClauses ?? []) {
    if (clause.token !== ts.SyntaxKind.ExtendsKeyword) throw new DtsError(sf, clause, 'unsupported heritage clause');
    out.push(...clause.types.map((t) => t.getText(sf)));
  }
  return out;
}

/** Push a function signature, merging consecutive overloads of one name. */
function pushFunction(list: FunctionDecl[] | Decl[], name: string, sig: Signature) {
  const prev = list.at(-1);
  if (prev?.kind === 'function' && prev.name === name) prev.signatures.push(sig);
  else (list as Decl[]).push({ kind: 'function', name, signatures: [sig] });
}

/** `// ----…` followed by `// Title` in a statement's leading comments. */
function bannerTitle(source: string, stmt: ts.Statement): string | null {
  const ranges = ts.getLeadingCommentRanges(source, stmt.getFullStart()) ?? [];
  let title: string | null = null;
  ranges.forEach((r, i) => {
    if (!/^\/\/\s*-{4,}\s*$/.test(source.slice(r.pos, r.end))) return;
    const next = ranges[i + 1];
    const m = next && source.slice(next.pos, next.end).match(/^\/\/\s*(.+?)\s*$/);
    if (m) title = m[1];
  });
  return title;
}

function declsOf(sf: ts.SourceFile, stmt: ts.Statement, out: Decl[]) {
  if (!hasModifier(stmt, ts.SyntaxKind.ExportKeyword)) throw new DtsError(sf, stmt, 'non-exported statement');
  if (ts.isInterfaceDeclaration(stmt)) {
    out.push({
      kind: 'interface',
      name: stmt.name.text,
      typeParams: typeParamsOf(sf, stmt),
      extends: extendsOf(sf, stmt),
      members: membersOf(sf, stmt.members),
      doc: docOf(stmt),
    });
  } else if (ts.isTypeAliasDeclaration(stmt)) {
    out.push({
      kind: 'type',
      name: stmt.name.text,
      typeParams: typeParamsOf(sf, stmt),
      // Kept as written: multi-line unions read better in their own layout.
      type: stmt.type.getText(sf),
      doc: docOf(stmt),
    });
  } else if (ts.isClassDeclaration(stmt) && stmt.name) {
    out.push({
      kind: 'class',
      name: stmt.name.text,
      extends: extendsOf(sf, stmt),
      members: membersOf(sf, stmt.members),
      doc: docOf(stmt),
    });
  } else if (ts.isFunctionDeclaration(stmt) && stmt.name) {
    pushFunction(out, stmt.name.text, signatureOf(sf, stmt));
  } else if (ts.isVariableStatement(stmt)) {
    const doc = docOf(stmt);
    for (const d of stmt.declarationList.declarations) {
      const name = nameText(sf, d, d.name);
      const t = d.type;
      if (t && ts.isTypeLiteralNode(t) && t.members.length && t.members.every(ts.isMethodSignature)) {
        const functions: FunctionDecl[] = [];
        for (const m of t.members as ts.NodeArray<ts.MethodSignature>) {
          pushFunction(functions, nameText(sf, m, m.name), signatureOf(sf, m));
        }
        out.push({ kind: 'namespace', name, functions, doc });
      } else {
        out.push({ kind: 'const', name, type: typeText(sf, d, t), doc });
      }
    }
  } else {
    throw new DtsError(sf, stmt, 'unsupported declaration');
  }
}

export function parseDts(source: string, fileName = 'index.d.ts'): Group[] {
  const sf = ts.createSourceFile(fileName, source, ts.ScriptTarget.Latest, true);
  const groups: Group[] = [{ title: null, decls: [] }];
  for (const stmt of sf.statements) {
    const title = bannerTitle(source, stmt);
    if (title !== null) groups.push({ title, decls: [] });
    declsOf(sf, stmt, groups[groups.length - 1].decls);
  }
  return groups.filter((g) => g.decls.length);
}
