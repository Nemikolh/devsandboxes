'use strict';

const { spawn } = require('node:child_process');
const path = require('node:path');

/** JSON envelope version this shim understands (`commands::status::SCHEMA`). */
const SCHEMA = 1;

/** `<process.platform>-<process.arch>` keys with a published binary package. */
const PLATFORMS = ['darwin-arm64', 'linux-arm64', 'linux-x64', 'win32-x64'];

class DevsandboxError extends Error {
  constructor(message, { args, exitCode = null, signal = null, stdout = '', stderr = '' } = {}) {
    super(message);
    this.name = 'DevsandboxError';
    this.args = args;
    this.exitCode = exitCode;
    this.signal = signal;
    this.stdout = stdout;
    this.stderr = stderr;
  }
}

function binaryPath() {
  if (process.env.DEVSANDBOX_BINARY) return process.env.DEVSANDBOX_BINARY;
  const key = `${process.platform}-${process.arch}`;
  const pkg = `@devsandboxes/${key}`;
  if (!PLATFORMS.includes(key)) {
    throw new Error(
      `devsandbox: no prebuilt binary for ${key} (available: ${PLATFORMS.join(', ')}). ` +
        'Build it with `cargo install devsandbox` and set DEVSANDBOX_BINARY to its path.',
    );
  }
  let dir;
  try {
    dir = path.dirname(require.resolve(`${pkg}/package.json`));
  } catch {
    throw new Error(
      `devsandbox: ${pkg} is not installed. It is an optional dependency; ` +
        'reinstall without --omit=optional / --no-optional.',
    );
  }
  return path.join(dir, 'bin', process.platform === 'win32' ? 'devsandbox.exe' : 'devsandbox');
}

function cli(args, opts = {}) {
  const argv = opts.dir ? ['-C', opts.dir, ...args] : [...args];
  const stderrMode = opts.stderr === 'inherit' ? 'inherit' : 'pipe';
  return new Promise((resolve, reject) => {
    let child;
    try {
      child = spawn(binaryPath(), argv, {
        cwd: opts.cwd,
        env: opts.env,
        signal: opts.signal,
        // stdin is closed unless we feed it, so the CLI never blocks on an
        // interactive prompt (they are all TTY-gated).
        stdio: [opts.input === undefined ? 'ignore' : 'pipe', 'pipe', stderrMode],
      });
    } catch (e) {
      return reject(e);
    }
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8').on('data', (d) => (stdout += d));
    if (child.stderr) child.stderr.setEncoding('utf8').on('data', (d) => (stderr += d));
    if (opts.input !== undefined) child.stdin.end(opts.input);
    child.on('error', reject);
    child.on('close', (exitCode, signal) => {
      const result = { exitCode, signal, stdout, stderr };
      if (exitCode === 0 || opts.reject === false) return resolve(result);
      const detail = stderr.trim().split('\n').pop() || `exit ${exitCode ?? signal}`;
      reject(new DevsandboxError(`devsandbox ${argv.join(' ')}: ${detail}`, { args: argv, ...result }));
    });
  });
}

/** Run a `--json` verb and unwrap its `{ schema, data }` envelope. */
async function json(args, opts) {
  const res = await cli([...args, '--json'], opts);
  let doc;
  try {
    doc = JSON.parse(res.stdout);
  } catch (e) {
    throw new DevsandboxError(`devsandbox ${args.join(' ')}: invalid JSON output: ${e.message}`, { args, ...res });
  }
  if (doc.schema !== SCHEMA) {
    throw new DevsandboxError(
      `devsandbox ${args.join(' ')}: unsupported JSON schema ${doc.schema} (this package understands ${SCHEMA})`,
      { args, ...res },
    );
  }
  return doc.data;
}

/** `name` or `{ all: true }` → CLI positional / `--all`. */
function target(t) {
  if (typeof t === 'string') return [t];
  if (t && t.all) return ['--all'];
  throw new TypeError('expected an instance name or { all: true }');
}

const unit = () => undefined;

const ls = (opts) => json(['ls'], opts);
const ps = (opts = {}) => json(['ps', ...(opts.all ? ['--all'] : [])], opts);
const stats = (opts) => json(['stats'], opts);
const status = (opts) => json(['status'], opts);
// The runtime's own inspect document, not enveloped.
const inspect = (name, opts) => cli(['inspect', name, '--json'], opts).then((r) => JSON.parse(r.stdout));

// `run --json` keeps stdout to the one document (build/hook output goes to stderr).
function run(sandbox, opts = {}) {
  const args = ['run', sandbox];
  for (const flag of ['name', 'branch', 'base']) {
    if (opts[flag] !== undefined) args.push(`--${flag}`, opts[flag]);
  }
  return json(args, opts);
}
const start = (t, opts) => cli(['start', ...target(t)], opts).then(unit);
const stop = (t, opts) => cli(['stop', ...target(t)], opts).then(unit);
const rebuild = (t, opts = {}) => cli(['rebuild', ...target(t), ...(opts.force ? ['--force'] : [])], opts).then(unit);
const rename = (name, newName, opts) => cli(['rename', name, newName], opts).then(unit);
const rm = (name, opts) => cli(['rm', name], opts).then(unit);
const gc = (opts = {}) => cli(['gc', ...(opts.force ? ['--force'] : [])], opts).then(unit);

function logs(name, opts = {}) {
  return cli(['logs', name, '-n', String(opts.lines ?? 50)], opts).then((r) => {
    const out = r.stdout.replace(/\n$/, '');
    return out === '(no log output)' ? '' : out;
  });
}
// Resolves with the command's exit code instead of rejecting on non-zero.
const exec = (name, command, opts = {}) =>
  cli(['exec', ...(opts.input !== undefined ? ['-i'] : []), name, ...command], { ...opts, reject: false });

// argv for spawning `exec` yourself (node-pty, child_process) without a shell.
// Unset flags are left to the CLI: no `cmd` gets a login shell with `-i`, plus
// `-t` when its stdin is a TTY.
function execArgv(name, cmd = [], opts = {}) {
  const args = ['exec'];
  if (opts.interactive) args.push('-i');
  if (opts.tty) args.push('-t');
  // `--` so a command word like `-t` reaches the container, not devsandbox's flags.
  args.push(name);
  if (cmd.length) args.push('--', ...cmd);
  return { file: binaryPath(), args };
}

const service = {
  ls: (opts) => json(['service', 'ls'], opts),
  rebuild: (name, opts) => cli(['service', 'rebuild', name], opts).then(unit),
};

// Plain `{ ident }` object so Node's CJS lexer exposes ESM named imports.
module.exports = {
  SCHEMA,
  DevsandboxError,
  binaryPath,
  cli,
  ls,
  ps,
  stats,
  status,
  inspect,
  run,
  start,
  stop,
  rebuild,
  rename,
  rm,
  gc,
  logs,
  exec,
  execArgv,
  service,
};
