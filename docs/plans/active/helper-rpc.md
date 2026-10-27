# Helper RPC: host → in-container daemon requests

Status: planned
Design written, nothing built: no `RPC` channel or caps bit in
`src/devsbd/proto.rs`, no `rpc.rs`, no `HelperClient`.

Today the one persistent link (the serve daemon's bridge,
[bridges](../../architecture/helper/bridges.md)) carries the container's
requests to the host (`SSH_AGENT`, `NOTIFY`, `CONTROL`); host → container is
only `Connect` (port forwards), `Ping` and `Quit`
([protocol](../../architecture/helper/protocol.md)). Every other host
operation is a fresh `<runtime> exec`, including dispatcher runs: `exec_in`
in `src/commands/dispatch.rs` execs `devsbd run start|ls|logs|wait` in the
child ([dispatchers](../../architecture/automations/dispatchers.md),
[in-container](../../architecture/helper/in-container.md)).

## Problem

The host can't ask `devsbd daemon` to do anything, so there's no single
place to queue, serialize, block, log or retry host-initiated operations,
and container-owned work depends on the host process that started it.
Dispatcher runs specifically:

- **No owner in the container.** A run is files under
  `/var/lib/devsandbox/runs/<id>/` plus a detached `devsbd run supervise`
  (`devsbd/src/runs.rs`); the daemon knows nothing about it, nobody reports
  its end.
- **Everything polls through the host.** `run ls/logs/wait`, `logs
  --follow` and `wait` are loops of short `CONTROL` requests
  (`devsbd/src/ctl.rs` `wait_once`, `follow`), each a new exec in the
  child. `serve` dying mid-`wait` breaks the dispatcher's call though the
  run continues.
- **No resume.** A container restart leaves runs `lost`; nothing restarts
  them.
- **No queue or gate.** Ops on a child are independent execs; limits or
  ordering ("one agent at a time") can only be bolted on in `dispatch.rs`.

Target: the daemon works like VS Code's extension host, a long-lived agent
owning its container's processes and state, which the host talks to with
messages. Helper binary size is best effort, not a hard limit.

## Goals

1. **Host → daemon requests**: open a stream, name a method, get results,
   streamed output, events or an error, with cancellation and flow control.
2. **One choke point per side**: the host's helper client (in `serve`) and
   the daemon's request handler; queueing, blocking and audit logging go
   there.
3. **Daemon-owned runs**: started, tracked, reported and restarted by the
   daemon. Host death changes who listens, never what runs.
4. **Enough for [cli-proxy](cli-proxy.md)**: PTY, resize, signals, stdin,
   arbitrary streamed I/O and caller identity on the same wire shapes.

Non-goals: replacing bootstrap (install, daemon start, the bridge) and the
local interactive `exec -it` paths (the CLI's and the dashboard's own
`exec`, and anything without `serve`, keep them); non-unix hosts (`serve`
is unix-only, they keep exec). Attaching to an existing helper session
(`session.attach`, below) is in scope: [fleet](fleet.md) step 4's `PTY`
needs a daemon-side attach, since a fleet worker runs no CLI for the hub.

## Design

### Transport

- **New `channel::RPC` (5) and `caps::RPC` bit**, advertised by daemons
  that serve it. No `VERSION` bump: an old peer ignores the bit and the
  host keeps the exec path for it.
- **One request per stream**: `Open { stream, channel: RPC }` with
  host-allocated ids (high bit), like `Connect`. The daemon (which refuses
  every `Open` today) accepts host `Open`s on `RPC` only. Concurrency
  without head-of-line blocking; cancel = `Close`; long transfers reuse
  `Connect`'s per-stream `Window` credit and `Eof` (`src/devsbd/mux.rs`).
- **Symmetric by design**: the container may open `RPC` streams to the host
  with the same envelope, so `CONTROL` can migrate later. Not in the first
  steps.

### Envelope

Shared `src/devsbd/rpc.rs` (in devsbd via `#[path]`, std only, like
`control.rs`). Inside a stream's `Data`, records are length-prefixed
`u8 tag | u32 len | payload` (the stream is a byte pipe; `Data` frame
boundaries mean nothing).

| Tag | Dir | Payload |
|---|---|---|
| `call` | → | first record: JSON `{ "method", "params", "meta" }` |
| `stdin` | → | raw bytes |
| `stdin-eof` | → | — |
| `resize` | → | `u16 cols, u16 rows` |
| `signal` | → | `u8 signo` |
| `stdout` / `stderr` | ← | raw bytes |
| `event` | ← | JSON, subscriptions and progress |
| `result` | ← | JSON, last record on success |
| `error` | ← | JSON `{ "code", "message" }`, last record on failure |
| `exit` | ← | `i32 code` or `signal n`, last record of a process |

- `meta`: `caller` (the instance a forwarded call came from), `deadline`,
  `idempotency` key (a retried `run.start` doesn't run twice).
- JSON stays small and flat: the helper's `json.rs` only extracts
  top-level strings and grows a flat-object decoder. Nested `params` →
  tagged records instead.
- **Discovery**: `rpc.methods` returns names with versions (`run.start@1`);
  the host picks per container, as `devsbd features` does for the CLI.

### Methods (first set)

| Method | Shape | Replaces |
|---|---|---|
| `rpc.methods` | result | — |
| `run.start` | result `{ id }`; params: argv, cwd, env, user, restart policy | `exec_in` + `run start` |
| `run.ls` / `run.rm` / `run.prune` | result | `exec_in` + `run …` |
| `run.logs` | `stdout` from `offset`; with `follow`, until the run ends | polling |
| `run.wait` | `event`s on state changes, then `result` | polling |
| `run.attach` | live run I/O (`stdin`/`stdout`/`resize`/`signal`) | — (interactive runs) |
| `session.attach` | params: session id, `create` (+ argv, cwd, env, user) or attach only; live session I/O (`stdin`/`stdout`/`resize`), then `exit`, or `error` `no-session` / `detached` | `exec -it … devsbd session new\|attach` for daemon-side callers ([resumable-exec](resumable-exec.md)) |
| `session.ls` / `session.rm` | result | `devsbd session ls\|rm` |
| `proc.spawn` | untracked process, I/O + `exit`, optional PTY | lifecycle / `vscode-goto` exec (optional) |
| `fs.write` | path, mode, owner, atomic; body in `stdin` | boot file, workspace file (optional) |
| `worktrees.sync` | result: created/removed stand-ins | — (see below) |

Handlers are pure where possible (request → actions), like `dispatch.rs`
and `runs.rs`, so they unit-test without a container.

### Queueing and blocking

- **Host: `HelperClient` in `serve`.** Every host → container call goes
  through it, from `dispatch.rs` or the CLI (a `helper.call` API method).
  It checks the method against what the container advertised, applies
  policy (e.g. no `proc.spawn` for dispatcher-originated calls), keeps a
  per-container queue with a concurrency limit, logs every call, stamps
  `meta.caller`.
- **Daemon: a scheduler.** `run.start` with a `queue` key ("agents") is
  serialized/limited per key, the queue persisted in the runs dir. Queued
  runs are `queued` (new state); `run.rm` of one just drops it.
- The daemon is the final authority on what runs in its container; host
  policy is about who may ask.

### Daemon-owned runs

- **Ownership**: the daemon spawns supervisors and keeps a registry (runs
  dir stays the truth). On start, including a takeover by a newer build,
  it adopts runs whose supervisor is alive (the existing pid + cmdline
  check) and marks the rest `lost`. Supervisors stay separate processes,
  so replacing the daemon doesn't kill runs.
- **Reporting**: on state change, an `event` on an open host subscription,
  else a queued outbox record like `notify`. The host turns it into an
  Inbox/events entry for the owning dispatcher, which stops polling.
- **Restart policy** on `run.start`: `restart = "no" | "on-failure" |
  "always"`, `max-restarts`, backoff. Applied on exit, and at boot to runs
  a restart left `lost` (`devsbd boot` already starts the daemon on every
  container start). Resume = relaunch the argv; attempts recorded
  (`attempt <n>` in `meta`, a log marker), the command sees
  `DEVSBD_RUN_ID` and `DEVSBD_RUN_ATTEMPT`; an optional `resume-argv` for
  attempts after the first (agent CLIs' `--resume`).
- **Host-independent**: `run.start` returns once the run is persisted and
  its supervisor spawned. `serve` dying drops only open `logs`/`wait`
  streams; clients reopen at the last offset and `devsbd run wait`
  reconnects instead of failing.

### Worktree stand-ins (`worktrees.sync`)

Worktrees devsandbox didn't make stay unlocked, so `git worktree prune` in
a container still deletes their records
([worktrees](../../architecture/instances/worktrees.md)). The daemon finds
repo `.git` dirs from `/proc/self/mountinfo` and keeps stub `.git` files at
the other worktrees' recorded paths. `serve` calls it on bridge attach and
when `run`/`rm` add or remove a worktree of a repo the container mounts. No
watcher, no host file.

### Without `serve`

`RPC` exists only while `serve` holds the bridge; `serve::client::
ensure_running` starts it for `run`/`start`/`rebuild` and the dashboard.
Bootstrap and interactive paths keep exec; moved ops keep their exec path
as the fallback for helpers without `caps::RPC` until it's retired.

## Steps

Each ships alone; 1-3 already fix "runs depend on a host exec".

1. **Transport**: `channel::RPC`, `caps::RPC`; daemon accepts host `Open`
   on `RPC` with stream caps like `Connect`; `rpc.rs` codec + envelope
   with fixture tests compiled in both crates (like `bootfile.rs`);
   `rpc.methods`.
2. **`HelperClient` in `serve`**: call, stream, cancel, per-container
   queue/log; `helper.call` API method; `Mux::open_host(channel)` next to
   `Mux::connect`.
3. **Runs over RPC**: `run.start/ls/rm/prune/logs/wait` backed by
   `runs.rs`; `dispatch.rs` uses them when the child advertises them, else
   `exec_in`. Follow and wait become one held stream.
4. **Daemon-owned runs**: registry and adoption, run-state events (stream,
   else outbox), `queued` state with per-key scheduling.
5. **Restart policy**: `restart`/`max-restarts`/backoff, relaunch of
   `lost` runs at boot, `DEVSBD_RUN_ATTEMPT`, `resume-argv`; surfaced in
   `devsbd exec` flags and the control op.
6. **`worktrees.sync`**.
7. **Container → host `RPC`**, only if [cli-proxy](cli-proxy.md) picks the
   through-the-host transport or `CONTROL` migrates.
8. **Sessions over RPC**: `session.attach` / `session.ls` / `session.rm`,
   the daemon connecting to the session's `sock` as a client (the
   supervisor's one-client takeover applies to it like to any other).
   Done before [fleet](fleet.md) step 4, after steps 1-3 here.

## Order across plans

1. [resumable-exec](resumable-exec.md) step 4, "now" part.
2. [fleet](fleet.md) steps 1-3, alongside steps 1-3 here.
3. Step 8 here (`session.attach`).
4. Fleet step 4 (`PTY` over the RPC stream).
5. Steps 4-5 here (daemon-owned runs, restart policy), before fleet step
   6 so dispatcher children on workers don't poll across machines.
6. Fleet steps 5-9.

## Open questions

- **PTY for `proc.spawn` / `run.attach`**: the helper already opens ptys
  through `extern "C"` libc calls without a dependency
  (`devsbd/src/sessions.rs`); `run.attach` in TTY mode and `proc.spawn`
  with a PTY reuse that. `session.attach` needs no pty of its own (the
  supervisor holds it).
- **Event delivery with no host attached**: reuse the notify outbox
  (persisted, acked) or a separate run-events queue?
- **Scheduler policy source**: per-call `queue` key only, or sandbox
  config (`[sandbox.*].runs.max-concurrent`) passed at `run`?
- **Lifecycle commands via `proc.spawn`?** Central log and queue, but makes
  `run` depend on `serve`. Probably not.
