'use strict';

const { spawn } = require('node:child_process');
const { EventEmitter } = require('node:events');
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
const done = (name, opts) => cli(['done', name], opts).then(unit);
const undone = (name, opts) => cli(['undone', name], opts).then(unit);
// `deleteBranch` unset passes neither flag: the CLI then keeps the branch (no TTY here).
const rm = (name, opts = {}) => {
  const branch = opts.deleteBranch === undefined ? [] : [opts.deleteBranch ? '--delete-branch' : '--keep-branch'];
  return cli(['rm', name, ...branch, ...(opts.force ? ['--force'] : [])], opts).then(unit);
};
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
  // `--` before `name` so neither a name nor a command word like `-t` is
  // parsed as one of devsandbox's flags.
  args.push('--', name, ...cmd);
  return { file: binaryPath(), args };
}

const service = {
  ls: (opts) => json(['service', 'ls'], opts),
  rebuild: (name, opts) => cli(['service', 'rebuild', name], opts).then(unit),
};

// ---- Daemon API (docs/api.md) over `devsandbox api --stdio`.

/** The API protocol this client speaks (`serve::proto::PROTOCOL`). */
const PROTOCOL = 1;
/** `devsandbox api --stdio`'s exit when the daemon hung up first (`EX_TEMPFAIL`). */
const EXIT_DAEMON_GONE = 75;
/** Bytes of the relay's stderr kept for error messages. */
const STDERR_TAIL = 4096;

class DevsandboxApiError extends Error {
  constructor(message, { code, method }) {
    super(message);
    this.name = 'DevsandboxApiError';
    this.code = code;
    this.method = method;
  }
}

class Api extends EventEmitter {
  constructor(child) {
    super();
    this.daemon = null;
    this.closed = false;
    this._child = child;
    this._nextId = 1;
    this._pending = new Map();
    this._stderr = '';
    this._buf = '';
    this._exit = new Promise((resolve) => (this._exited = resolve));

    child.stdout.setEncoding('utf8').on('data', (d) => this._data(d));
    if (child.stderr) {
      child.stderr.setEncoding('utf8').on('data', (d) => (this._stderr = (this._stderr + d).slice(-STDERR_TAIL)));
    }
    // A dead relay's EPIPE surfaces as `close`; don't let it throw.
    child.stdin.on('error', () => {});
    child.on('error', (e) => this._closed(null, null, e));
    child.on('close', (code, signal) => this._closed(code, signal));

    this.inbox = {
      list: (view) => this.call('inbox.threads.list', view === undefined ? {} : { view }),
      get: (t) => this.call('inbox.thread.get', t),
      markRead: (t) => this.call('inbox.thread.markRead', t).then(unit),
      act: (t, action) => this.call('inbox.thread.act', { ...t, action }).then(unit),
      reply: (t, text) => this.call('inbox.thread.reply', { ...t, text }).then(unit),
      done: (t) => this.call('inbox.thread.done', t).then(unit),
      reopen: (t) => this.call('inbox.thread.reopen', t).then(unit),
      dismiss: (target) => this.call('inbox.notify.dismiss', target).then(unit),
      markNotifyRead: (target) => this.call('inbox.notify.markRead', target).then(unit),
    };
    // The daemon's cwd is `/`: config roots go absolute from here.
    this.instances = {
      list: (dir) => this.call('instances.list', { dir: path.resolve(dir) }),
    };
    this.forwards = {
      list: (dir) => this.call('forwards.list', dir === undefined ? {} : { dir: path.resolve(dir) }),
      add: (params) => this.call('forwards.add', { ...params, dir: path.resolve(params.dir) }),
      rm: (id) => this.call('forwards.rm', { id }),
    };
  }

  call(method, params) {
    return this._send(this._nextId++, method, params);
  }

  subscribe(topics) {
    return this.call('subscribe', { topics: [...topics] }).then(unit);
  }

  unsubscribe(topics) {
    return this.call('unsubscribe', { topics: [...topics] }).then(unit);
  }

  /** Hang up: EOF on the relay's stdin; it exits once the daemon answered what's in flight. */
  close() {
    if (!this.closed) this._child.stdin.end();
    return this._exit;
  }

  _send(id, method, params) {
    if (this.closed) return Promise.reject(this._closedError(method));
    return new Promise((resolve, reject) => {
      this._pending.set(id, { method, resolve, reject });
      const req = params === undefined ? { id, method } : { id, method, params };
      this._child.stdin.write(JSON.stringify(req) + '\n');
    });
  }

  _data(chunk) {
    this._buf += chunk;
    let nl;
    while ((nl = this._buf.indexOf('\n')) !== -1) {
      const line = this._buf.slice(0, nl);
      this._buf = this._buf.slice(nl + 1);
      if (line.trim()) this._line(line);
    }
  }

  _line(line) {
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      return; // Not ours to fix: the relay passes the daemon's bytes through.
    }
    if (typeof msg.method === 'string' && msg.id === undefined) {
      const params = msg.params ?? {};
      this.emit('notification', { method: msg.method, params });
      this.emit(msg.method, params);
      return;
    }
    const call = this._pending.get(msg.id);
    if (!call) return;
    this._pending.delete(msg.id);
    if (msg.error) {
      const { code = 'internal', message = 'unknown error' } = msg.error;
      call.reject(new DevsandboxApiError(`devsandbox ${call.method}: ${message}`, { code, method: call.method }));
    } else {
      call.resolve(msg.result);
    }
  }

  _closedError(method) {
    const why =
      this._exitCode === EXIT_DAEMON_GONE
        ? 'the daemon closed the connection'
        : this._spawnError
          ? this._spawnError.message
          : this._exitCode === 0
            ? 'the connection is closed'
            : `devsandbox api --stdio exited (${this._exitCode ?? this._exitSignal})`;
    const detail = this._stderr.trim().split('\n').pop();
    return new DevsandboxApiError(`devsandbox ${method}: ${why}${detail ? `: ${detail}` : ''}`, { code: 'closed', method });
  }

  _closed(code, signal, spawnError) {
    if (this.closed) return;
    this.closed = true;
    this._exitCode = code;
    this._exitSignal = signal;
    this._spawnError = spawnError;
    for (const [, call] of this._pending) call.reject(this._closedError(call.method));
    this._pending.clear();
    this._exited();
    this.emit('close', { code, signal });
  }
}

/** Spawn the relay, say hello, check the protocol. */
async function connect(opts = {}) {
  if (process.platform === 'win32') {
    throw new DevsandboxApiError('devsandbox: the API needs the host daemon, which is unix only', {
      code: 'unsupported',
      method: 'hello',
    });
  }
  const name = opts.name || 'node';
  const child = spawn(binaryPath(), ['api', '--stdio', '--client', `npm:${name}`], {
    env: opts.env,
    stdio: ['pipe', 'pipe', opts.stderr === 'inherit' ? 'inherit' : 'pipe'],
  });
  const api = new Api(child);
  // Version `0.0.0`: the relay, a real devsandbox binary, already said hello
  // with its own version (and handed off an older daemon); ours must never
  // trigger a handoff, whatever binary DEVSANDBOX_BINARY points at.
  const hello = await api._send(0, 'hello', { version: '0.0.0', build: 0, client: `npm:${name}` });
  if (!hello || hello.protocol !== PROTOCOL) {
    await api.close();
    throw new DevsandboxApiError(
      `devsandbox: the daemon speaks API protocol ${hello && hello.protocol}, this package ${PROTOCOL}`,
      { code: 'protocol', method: 'hello' },
    );
  }
  api.daemon = hello;
  return api;
}

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
  done,
  undone,
  rm,
  gc,
  logs,
  exec,
  execArgv,
  service,
  PROTOCOL,
  DevsandboxApiError,
  connect,
};
