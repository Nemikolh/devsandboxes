import { describe, expect, it } from 'vitest';
import {
  candidatesFor,
  complete,
  DEFAULT_WORKTREE_BRANCH,
  type Names,
  newPrompt,
  parseLine,
  type Prompt,
  SPECS,
  validatePortSpec,
} from './mini-tui-spec';

const names: Names = { servicesTab: false, sandboxes: ['web'], instances: ['web', 'web-2', 'web-3'], services: ['postgres'] };
const toks = (line: string) => line.split(/\s+/).filter(Boolean);
const tab = (p: Prompt, n: Names = names) => complete(p, (idx, tokens) => candidatesFor(idx, tokens, n));

describe('grammar (src/tui/spec.rs)', () => {
  it('lists the commands in the real order', () => {
    expect(SPECS.map((s) => s.name)).toEqual(['run', 'exec', 'code', 'rm', 'rename', 'stop', 'start', 'rebuild', 'port']);
  });

  it('parses like the real parser', () => {
    expect(parseLine('run web')).toEqual({ cmd: 'run', sandbox: 'web' });
    expect(parseLine('run --name api web')).toMatchObject({ cmd: 'run', sandbox: 'web', name: 'api' });
    expect(parseLine('run web --name a --name b')).toMatchObject({ name: 'b' });
    expect(parseLine('exec box git log --oneline')).toEqual({ cmd: 'exec', instance: 'box', argv: ['git', 'log', '--oneline'] });
    expect(parseLine('rebuild box --force')).toEqual({ cmd: 'rebuild', instance: 'box', force: true });
    expect(parseLine('recreate box')).toEqual({ cmd: 'rebuild', instance: 'box', force: false });
    expect(parseLine('port api --service postgres 5432')).toMatchObject({ instance: 'api', service: 'postgres', host: null, port: 5432 });
    expect(parseLine('port api 8080:3000')).toMatchObject({ spec: '8080:3000', host: 8080, port: 3000 });
  });

  it('errors with the real messages', () => {
    expect(parseLine('')).toEqual({ error: 'empty command' });
    expect(parseLine('frobnicate x')).toEqual({
      error: 'unknown command `frobnicate` (run, exec, code, rm, rename, stop, start, rebuild, port)',
    });
    expect(parseLine('run')).toEqual({ error: 'usage: run <sandbox> [--name n] [--branch b] [--base ref]' });
    expect(parseLine('run web --name')).toEqual({ error: '`--name` needs a value' });
    expect(parseLine('run web --bogus')).toEqual({ error: 'unknown flag `--bogus`' });
    expect(parseLine('run a b')).toEqual({ error: 'unexpected argument `b`' });
    expect(parseLine('exec box')).toEqual({ error: 'usage: exec <instance> <cmd…>' });
    expect(parseLine('rename old')).toEqual({ error: 'usage: rename <instance> <new-name>' });
    expect(parseLine('rebuild --force')).toEqual({ error: 'usage: rebuild [--force] <instance>' });
    expect(parseLine('port api')).toEqual({ error: 'usage: port <instance> [--service s] [--address a] <[host:]port>' });
  });

  it('validates port specs like validate_port_spec', () => {
    expect(validatePortSpec('3000')).toEqual({ host: null, port: 3000 });
    expect(validatePortSpec('0')).toEqual({ error: 'port spec `0`: port must not be 0' });
    expect(validatePortSpec('abc')).toEqual({ error: 'port spec `abc`: port `abc` is not a port number' });
    expect(validatePortSpec('70000')).toEqual({ error: 'port spec `70000`: port `70000` is not a port number' });
    expect(validatePortSpec('8080:0')).toEqual({ error: 'port spec `8080:0`: container port must not be 0' });
    expect(validatePortSpec(':80')).toEqual({ error: 'port spec `:80`: host port is empty' });
    expect(validatePortSpec('1:2:3')).toEqual({ error: 'port spec `1:2:3`: expected `<port>` or `<host>:<port>`' });
  });
});

describe('completion (candidates_for + Prompt::complete)', () => {
  it('offers commands first, then what the spec expects', () => {
    expect(candidatesFor(1, toks('run'), names)).toEqual(['web']);
    expect(candidatesFor(2, toks('run web'), names)).toEqual(['--name', '--branch', '--base']);
    expect(candidatesFor(4, toks('run web --name x'), names)).toEqual(['--branch', '--base']);
    expect(candidatesFor(3, toks('run web --branch'), names)).toEqual([DEFAULT_WORKTREE_BRANCH]);
    expect(candidatesFor(3, toks('run web --name'), names)).toEqual([]);
    expect(candidatesFor(1, toks('port'), names)).toEqual(names.instances);
    expect(candidatesFor(3, toks('port web --service'), names)).toEqual(['postgres']);
    expect(candidatesFor(2, toks('exec web'), names)).toEqual([]);
    expect(candidatesFor(1, toks('rebuild'), { ...names, servicesTab: true })).toEqual(['postgres']);
    expect(candidatesFor(2, toks('rebuild --force'), names)).toEqual(names.instances);
    expect(candidatesFor(1, toks('nope'), names)).toEqual([]);
  });

  it('applies the first sorted match and cycles on repeated Tab', () => {
    const r = tab(newPrompt('r'));
    expect(r.input).toBe('rebuild');
    expect(r.completion?.candidates).toEqual(['rebuild', 'rename', 'rm', 'run']);
    expect(tab(r).input).toBe('rename');
    expect(tab(tab(tab(tab(r)))).input).toBe('rebuild');
    expect(tab(newPrompt('ru')).input).toBe('run');
  });

  it('completes the last token, an empty one after a space', () => {
    expect(tab(newPrompt('stop ')).input).toBe('stop web');
    const w = tab(newPrompt('stop web-'));
    expect(w.input).toBe('stop web-2');
    expect(tab(w).input).toBe('stop web-3');
    expect(tab(tab(w)).input).toBe('stop web-2');
  });

  it('leaves the line alone without a match; one candidate does not cycle', () => {
    const p = newPrompt('zz');
    expect(tab(p)).toBe(p);
    const run = tab(newPrompt('ru'));
    expect(tab(run)).toBe(run);
  });
});
