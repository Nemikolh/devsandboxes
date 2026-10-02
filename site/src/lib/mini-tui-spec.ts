/**
 * The mini dashboard's `:` prompt grammar and line editing, ported from the
 * real one so the preview parses, errors and completes the same way:
 * `SPECS` / `parseArgs` mirror `src/tui/spec.rs`, `parseLine` mirrors
 * `parse_line` in `src/tui/prompt.rs`, `candidatesFor` mirrors
 * `candidates_for` in `src/tui/app/command_line.rs`, and `complete` mirrors
 * `Prompt::complete` (sorted candidates filtered by the stem, the first one
 * applied, repeated Tab cycles). The caret always sits at the end of the line
 * here (no ←/→ editing), which is where the real one leaves it after typing.
 *
 * Pure and DOM-free, like `mini-tui.ts`.
 */

export type ArgValue = 'instance' | 'instanceOrService' | 'service' | 'sandbox' | 'branch' | 'free';

export interface FlagSpec {
  name: string;
  /** Set when the flag takes the next token as its value. */
  value?: ArgValue;
}

export interface CommandSpec {
  name: string;
  aliases: readonly string[];
  usage: string;
  flags: readonly FlagSpec[];
  positionals: readonly ArgValue[];
  /** After the positionals, the rest of the line is verbatim argv (`exec`). */
  trailing: boolean;
}

/** Every prompt command, in first-token completion order (`SPECS` in `src/tui/spec.rs`). */
export const SPECS: readonly CommandSpec[] = [
  {
    name: 'run',
    aliases: [],
    usage: 'run <sandbox> [--name n] [--branch b] [--base ref]',
    flags: [
      { name: '--name', value: 'free' },
      { name: '--branch', value: 'branch' },
      { name: '--base', value: 'free' },
    ],
    positionals: ['sandbox'],
    trailing: false,
  },
  { name: 'exec', aliases: [], usage: 'exec <instance> <cmd…>', flags: [], positionals: ['instance'], trailing: true },
  { name: 'code', aliases: [], usage: 'code <instance>', flags: [], positionals: ['instance'], trailing: false },
  {
    name: 'rm',
    aliases: [],
    usage: 'rm [--force] <instance>',
    flags: [{ name: '--force' }],
    positionals: ['instance'],
    trailing: false,
  },
  {
    name: 'rename',
    aliases: [],
    usage: 'rename <instance> <new-name>',
    flags: [],
    positionals: ['instance', 'free'],
    trailing: false,
  },
  { name: 'stop', aliases: [], usage: 'stop <instance>', flags: [], positionals: ['instance'], trailing: false },
  { name: 'start', aliases: [], usage: 'start <instance>', flags: [], positionals: ['instance'], trailing: false },
  {
    name: 'rebuild',
    aliases: ['recreate'],
    usage: 'rebuild [--force] <instance>',
    flags: [{ name: '--force' }],
    positionals: ['instanceOrService'],
    trailing: false,
  },
  {
    name: 'port',
    aliases: [],
    usage: 'port <instance> [--service s] [--address a] <[host:]port>',
    flags: [
      { name: '--service', value: 'service' },
      { name: '--address', value: 'free' },
    ],
    positionals: ['instance', 'free'],
    trailing: false,
  },
];

export const findSpec = (cmd: string): CommandSpec | undefined =>
  SPECS.find((s) => s.name === cmd || s.aliases.includes(cmd));

export interface ParsedArgs {
  flags: string[];
  values: [string, string][];
  positionals: string[];
  trailing: string[];
}

/** Last value given for a value-taking flag (last one wins, like the real one). */
const valueOf = (args: ParsedArgs, name: string): string | undefined =>
  args.values.findLast(([n]) => n === name)?.[1];

/** `parse_args` in `src/tui/spec.rs`: the same walk and the same messages. */
export function parseArgs(spec: CommandSpec, tokens: readonly string[]): ParsedArgs | { error: string } {
  const out: ParsedArgs = { flags: [], values: [], positionals: [], trailing: [] };
  for (let i = 0; i < tokens.length; i++) {
    const tok = tokens[i];
    if (spec.trailing && out.positionals.length === spec.positionals.length) {
      out.trailing.push(...tokens.slice(i));
      break;
    }
    if (tok.startsWith('--')) {
      const flag = spec.flags.find((f) => f.name === tok);
      if (!flag) return { error: `unknown flag \`${tok}\`` };
      if (flag.value) {
        const val = tokens[i + 1];
        if (val === undefined) return { error: `\`${flag.name}\` needs a value` };
        out.values.push([flag.name, val]);
        i++;
      } else {
        out.flags.push(flag.name);
      }
    } else {
      if (out.positionals.length === spec.positionals.length) return { error: `unexpected argument \`${tok}\`` };
      out.positionals.push(tok);
    }
  }
  if (out.positionals.length < spec.positionals.length || (spec.trailing && out.trailing.length === 0)) {
    return { error: `usage: ${spec.usage}` };
  }
  return out;
}

/** `validate_port_spec` in `src/commands/port.rs`: `[host, container]` or the error. */
export function validatePortSpec(spec: string): { host: number | null; port: number } | { error: string } {
  const parse = (s: string, what: string): number | string => {
    if (s === '') return `port spec \`${spec}\`: ${what} is empty`;
    // Rust's `u16::from_str`: digits only (a leading `+` too), in range.
    const n = /^\+?\d+$/.test(s) ? Number(s) : NaN;
    if (!Number.isInteger(n) || n > 65535) return `port spec \`${spec}\`: ${what} \`${s}\` is not a port number`;
    if (n === 0) return `port spec \`${spec}\`: ${what} must not be 0`;
    return n;
  };
  const parts = spec.split(':');
  if (parts.length === 1) {
    const port = parse(parts[0], 'port');
    return typeof port === 'string' ? { error: port } : { host: null, port };
  }
  if (parts.length === 2) {
    const host = parse(parts[0], 'host port');
    if (typeof host === 'string') return { error: host };
    const port = parse(parts[1], 'container port');
    return typeof port === 'string' ? { error: port } : { host, port };
  }
  return { error: `port spec \`${spec}\`: expected \`<port>\` or \`<host>:<port>\`` };
}

/** A parsed prompt line (`PromptAction` in `src/tui/prompt.rs`). */
export type Action =
  | { cmd: 'run'; sandbox: string; name?: string; branch?: string; base?: string }
  | { cmd: 'exec'; instance: string; argv: string[] }
  | { cmd: 'code' | 'rm' | 'stop' | 'start'; instance: string }
  | { cmd: 'rename'; instance: string; newName: string }
  | { cmd: 'rebuild'; instance: string; force: boolean }
  | { cmd: 'port'; instance: string; service?: string; address?: string; spec: string; host: number | null; port: number };

/** `parse_line` in `src/tui/prompt.rs`. */
export function parseLine(line: string): Action | { error: string } {
  const tokens = line.split(/\s+/).filter(Boolean);
  const [cmd, ...rest] = tokens;
  if (cmd === undefined) return { error: 'empty command' };
  const spec = findSpec(cmd);
  if (!spec) return { error: `unknown command \`${cmd}\` (${SPECS.map((s) => s.name).join(', ')})` };
  const args = parseArgs(spec, rest);
  if ('error' in args) return args;
  const [first, second] = args.positionals;
  switch (spec.name) {
    case 'run':
      return {
        cmd: 'run',
        sandbox: first,
        name: valueOf(args, '--name'),
        branch: valueOf(args, '--branch'),
        base: valueOf(args, '--base'),
      };
    case 'exec':
      return { cmd: 'exec', instance: first, argv: args.trailing };
    case 'code':
    case 'rm':
    case 'stop':
    case 'start':
      return { cmd: spec.name, instance: first };
    case 'rename':
      return { cmd: 'rename', instance: first, newName: second };
    case 'rebuild':
      return { cmd: 'rebuild', instance: first, force: args.flags.includes('--force') };
    default: {
      const ports = validatePortSpec(second);
      if ('error' in ports) return ports;
      return {
        cmd: 'port',
        instance: first,
        service: valueOf(args, '--service'),
        address: valueOf(args, '--address'),
        spec: second,
        ...ports,
      };
    }
  }
}

/** What completes where: the names the dashboard knows right now. */
export interface Names {
  /** Services tab: `rebuild` completes services instead of instances. */
  servicesTab: boolean;
  sandboxes: readonly string[];
  instances: readonly string[];
  services: readonly string[];
}

/** The branch `run` would use (`DEFAULT_WORKTREE_BRANCH`; the sandbox sets no `worktree-branch`). */
export const DEFAULT_WORKTREE_BRANCH = 'sandbox/${instance}';

/**
 * Positional tokens already on the line (`consumed_positionals`): not a known
 * flag, not a flag's value, not the token being completed at `skip`.
 */
function consumedPositionals(spec: CommandSpec, tokens: readonly string[], skip: number): string[] {
  const out: string[] = [];
  for (let i = 1; i < tokens.length; i++) {
    if (spec.trailing && out.length === spec.positionals.length) break;
    const flag = spec.flags.find((f) => f.name === tokens[i]);
    if (flag) {
      if (flag.value) i++;
      continue;
    }
    if (i !== skip) out.push(tokens[i]);
  }
  return out;
}

/** Raw candidates for the token at `idx` of `tokens` (`candidates_for`); `complete` filters them. */
export function candidatesFor(idx: number, tokens: readonly string[], names: Names): string[] {
  if (idx === 0) return SPECS.map((s) => s.name);
  const spec = tokens[0] === undefined ? undefined : findSpec(tokens[0]);
  if (!spec) return [];
  const values = (v: ArgValue): string[] => {
    switch (v) {
      case 'instance':
        return [...names.instances];
      case 'instanceOrService':
        return [...(names.servicesTab ? names.services : names.instances)];
      case 'service':
        return [...names.services];
      case 'sandbox':
        return [...names.sandboxes];
      case 'branch':
        return [DEFAULT_WORKTREE_BRANCH];
      case 'free':
        return [];
    }
  };
  const prevFlag = spec.flags.find((f) => f.name === tokens[idx - 1]);
  if (prevFlag?.value) return values(prevFlag.value);
  const unusedFlags = () =>
    spec.flags.filter((f) => !tokens.some((t, i) => i !== idx && t === f.name)).map((f) => f.name);
  if ((tokens[idx] ?? '').startsWith('-')) return unusedFlags();
  const next = spec.positionals[consumedPositionals(spec, tokens, idx).length];
  if (next) return values(next);
  if (spec.trailing) return [];
  return unusedFlags();
}

/** An in-flight Tab cycle over one token (`Completion`). */
export interface Completion {
  candidates: readonly string[];
  cycle: number;
  /** Char span `[start, end)` of the token being completed. */
  start: number;
  end: number;
}

export interface Prompt {
  input: string;
  completion: Completion | null;
  /** Inline parse error, shown red under the line. */
  error: string | null;
}

export const newPrompt = (input = ''): Prompt => ({ input, completion: null, error: null });

/** Longest line the preview keeps (the real one has no cap; the widget is narrow). */
export const PROMPT_MAX = 100;

/** Any edit drops the cycle and a stale error (`Prompt::edited`). */
export const typeChar = (p: Prompt, c: string): Prompt =>
  p.input.length >= PROMPT_MAX ? p : { input: p.input + c, completion: null, error: null };

export const backspace = (p: Prompt): Prompt =>
  p.input ? { input: p.input.slice(0, -1), completion: null, error: null } : p;

/** The token the end-of-line caret is in, or a new empty one after trailing space (`token_at_cursor`). */
function tokenAtEnd(input: string): { idx: number; start: number; end: number } {
  const cursor = input.length;
  let idx = 0;
  let i = 0;
  while (i < input.length) {
    while (i < input.length && /\s/.test(input[i])) i++;
    const start = i;
    while (i < input.length && !/\s/.test(input[i])) i++;
    if (cursor >= start && cursor <= i) return { idx, start, end: i };
    idx++;
  }
  return { idx, start: cursor, end: cursor };
}

/** Tab (`Prompt::complete`): apply the first sorted match, or cycle an existing completion. */
export function complete(p: Prompt, candidates: (idx: number, tokens: readonly string[]) => string[]): Prompt {
  const c = p.completion;
  if (c) {
    if (c.candidates.length < 2) return p;
    const cycle = (c.cycle + 1) % c.candidates.length;
    const pick = c.candidates[cycle];
    const input = p.input.slice(0, c.start) + pick + p.input.slice(c.end);
    return { input, error: null, completion: { ...c, cycle, end: c.start + pick.length } };
  }
  const { idx, start, end } = tokenAtEnd(p.input);
  const stem = p.input.slice(start, end);
  const tokens = p.input.split(/\s+/).filter(Boolean);
  const cands = [...new Set(candidates(idx, tokens).filter((x) => x.startsWith(stem)))].sort();
  if (cands.length === 0) return p;
  const pick = cands[0];
  return {
    input: p.input.slice(0, start) + pick + p.input.slice(end),
    error: null,
    completion: { candidates: cands, cycle: 0, start, end: start + pick.length },
  };
}
