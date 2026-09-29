import { codeToHtml } from 'shiki';
import { keepLines, linkTypes } from './api';
import { devsandboxesTheme } from './shiki-theme';

/**
 * A compact TypeScript snippet (signature, type alias) highlighted with the
 * site theme, declared type names linked. No code-block chrome: these sit
 * inline in the reference. `context` wraps the code in lines that give it the
 * right grammar (a method signature inside an object type) and is dropped
 * from the output.
 */
export async function highlightTs(
  code: string,
  known: ReadonlyMap<string, string>,
  context?: { before: string; after: string },
): Promise<string> {
  const transformers = [linkTypes(known)];
  let source = code;
  if (context) {
    const first = context.before.split('\n').length;
    transformers.push(keepLines(first, first + code.split('\n').length - 1));
    source = context.before + code + context.after;
  }
  return codeToHtml(source, { lang: 'ts', theme: devsandboxesTheme, transformers });
}
