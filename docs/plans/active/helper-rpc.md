# Helper RPC: host → in-container daemon requests

Status: steps 1-3 and 8 shipped; steps 4-7 planned (fleet step 4, their
prerequisite, shipped)
Step 1 shipped: `channel::RPC` / `caps::RPC`, flowing `Open`
(`proto::channel::flows`, `Mux::on_host_open`), `src/devsbd/rpc.rs`,
`devsbd/src/rpc_server.rs` with `rpc.methods`.
Step 2 shipped: `Link`s on the `BridgeBoard`, `Mux::open_host` /
`Mux::close_stream`, `HelperClient` (`src/serve/helper.rs`: queue, log,
`offers`), the internal `helper.call`.
Step 3 shipped: streaming handlers (`rpc_server::Out`), the `run.*`
methods (`devsbd/src/rpc_runs.rs`; `run.start`/`rm`/`prune` root only),
`dispatch.rs`' `run_op` calling them (`rpc_request`, `rpc_answer`,
`Executor::helper_call`) with exec as the per-method fallback. The
dispatcher → host leg still polls (`docs/status.md`).
Step 8 shipped: `session.attach` / `session.ls` / `session.rm`
(`devsbd/src/rpc_sessions.rs`, root only; `sessions::join_end` / `End`),
the `no-session` / `detached` / `version` error codes, the host's
`HelperClient::call_with_input` / `Input`.

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

## Step details (1-3, 8)

From recon; line numbers drift, trust the symbol. Corrections to the
design above, found in the code:

- **`Open` streams have no flow control.** Only `Connect` gets
  `Entry::Flow` (window credit, `Eof`); an `Open` is `Entry::Legacy`,
  its `Data` written inline on the frame reader (`src/devsbd/mux.rs`
  `serve_with`). One slow consumer stalls the link, keepalive included.
- **The daemon isn't always root**: the host starts it `exec -u root`
  (`src/devsbd.rs`), the bridge's self-heal spawn too, but `devsbd boot`
  starts it as the container command's user (`devsbd/src/boot.rs`).
- **Run ops already run in `serve`**: `dispatch::handle` is called on
  serve's bridge threads (`dispatch_control`, `src/devsbd/bridge.rs`);
  `Subprocess::exec_in` runs `<runtime> exec` from serve. Only
  `Executor::devsandbox` (ensure/stop/rm) is a subprocess.
- **No shared handle to a container's mux**: managed `Bridges` belong to
  the worker thread (`spawn_worker`); the mux sits in the private
  `Bridge.forward` `OnceLock`.
- **The helper has no JSON encoder** and `devsbd/src/json.rs` only
  extracts top-level strings.

### Step 1: transport

- `src/devsbd/proto.rs`: `channel::RPC = 5`, `caps::RPC = 1 << 5`; also
  into the helper bridge's `OWN_CAPS` (`devsbd/src/bridge.rs`), else the
  host never sees the bit; the daemon's `Hello` caps in `serve_bridge`
  (`devsbd/src/daemon.rs`).
- **Flow-controlled `Open`**: an `Open` on a channel registered as
  flowing creates `Entry::Flow` (same window/`Eof` path as `Connect`:
  `start_flow`, `flow_pump`, `flow_writer`), on both sides. Legacy
  channels (`SSH_AGENT`, `HTTP_PROXY`, `NOTIFY`, `CONTROL`) unchanged.
  Bump `proto::VERSION` if the frame semantics change (pre-1.0, both ends
  same version). [fleet](fleet.md) step 1a reuses this for `API` and
  `TRANSFER`.
- Host-opened `Open` (high-bit ids): `serve_with` today accepts only
  container ids on `Open`; add an `on_host_open(channel, stream)` hook
  admitting high-bit ids under `connect_allowed` / `MAX_HOST_STREAMS`.
  The daemon passes a handler for `RPC` only.
- `src/devsbd/rpc.rs` (shared via `#[path]`, `devsbd/src/main.rs` and
  `src/devsbd.rs`, `#[allow(dead_code)]` for one-sided items): record
  codec per the envelope table, plus a small flat JSON object
  encoder/decoder (strings, ints, bools, string arrays, string maps:
  enough for `run.start`'s argv/env), std only, fixture tests compiled in
  both crates (model: `bootfile.rs` `FIXTURE`).
- Helper handler `devsbd/src/rpc_server.rs`: one thread per stream,
  `rpc.methods` (model: `FEATURES`, `devsbd/src/main.rs`), unknown method
  → `error`.
- The daemon advertises `caps::RPC` always; methods that need to act as
  another user (`run.start`, `session.*`) are listed by `rpc.methods` only
  when the daemon's euid is 0. The host falls back per method.
- Tests: mux (`open_with_high_bit_is_refused` gets a per-channel
  counterpart, `connect_id_policy`, `flow_pair_with`,
  `credit_overrun_closes_only_that_stream` for `Open`), daemon caps,
  `own_caps` in `src/devsbd/bridge.rs`, the codec fixture.
- Docs: `docs/architecture/helper/protocol.md` (channels, caps, stream
  kinds), `docs/architecture/helper/in-container.md`.

### Step 2: `HelperClient` in `serve`

- A registry container → (`Arc<Mux>`, peer caps) shared from the bridge
  worker: on `BridgeBoard` (already shared with the API's `Waker`,
  `src/serve/host.rs`), published where `attach_with` sets
  `hs_forward`, removed on detach.
- `Mux::open_host(channel)` (model: `Mux::connect`: high-bit id,
  `Entry::Flow`, frame, `start_flow`), wrapped like `Bridge::connect`
  (caps check → `Unsupported`).
- `HelperClient` (new `src/serve/helper.rs`): `call(container, method,
  params) -> Stream`, cancel = `Close`, per-container concurrency limit
  under `MAX_HOST_STREAMS`, one log line per call (method, caller,
  outcome), `meta.caller` stamped.
- `helper.call`: connection-level like verbs (`src/serve/daemon.rs`
  `handle` → `verb`), internal: not in `npm/devsandboxes/index.d.ts`
  (like `bridges.ensure`), documented in `docs/reference/api.md`.
- Tests: `src/devsbd/bridge.rs` (`service_pair`), `src/serve/api.rs`
  fakes.
- Docs: `docs/architecture/helper/bridges.md` (host → container),
  `docs/reference/api.md`.

### Step 3: runs over RPC

- Helper methods `run.start/ls/rm/prune/logs/wait` on `runs.rs`
  (`start_with`, `ls`, `read_chunk`, `wait`, `rm`, `prune`); `run.logs`
  with `follow` and `run.wait` are one held stream each (events, then
  `result`). `run.start` params carry what `exec_argv_with` passes today
  (user, cwd, env incl. `SSH_AUTH_SOCK`); the root daemon drops to that
  user (`CommandExt::uid/gid`, `HOME`/`USER`, like `boot.rs`).
- `src/commands/dispatch.rs`: `Executor::helper_call` beside `exec_in`;
  `run_op` uses it when the child's helper lists the method, else
  `helper_argv` + `exec_in` as today (incl. `run rm|prune`). The `Fake`
  executor keeps the decision tests.
- The dispatcher → host leg (`devsbd/src/ctl.rs` `wait_once`, `follow`)
  stays per-poll CONTROL for now; noted in `docs/status.md`.
- Tests: `dispatch.rs` (`run_ops_exec_the_childs_helper`,
  `run_op_failures`, `run_op_fields`), `runs.rs`, `rpc_server.rs`.
- Docs: `docs/architecture/automations/dispatchers.md`, `in-container.md`,
  `bridges.md`.

### Step 8: sessions over RPC

- `session.attach {id, create?: {argv, cwd, env, user}}`: the daemon is a
  session client (`devsbd/src/sessions.rs` `join` → `connect` →
  `relay`, which takes `impl Read` / `&mut dyn Write` / a size fn: feed
  RPC `stdin` and `resize` in, `stdout` out; skip `terminal`'s raw-mode
  wrapper). End records: `exit` (code, `128+n` for signals as today),
  `error` `no-session` | `detached` | `version`.
- `create` from a root daemon must start the supervisor as `user` (else
  the session and its `sock` are root's). Root only (step 1's rule).
- `session.ls` / `session.rm` on `sessions::ls` / `rm`.
- Tests: `sessions.rs` (`echo_round_trip_and_exit_codes`,
  `a_new_attach_takes_over` with an RPC client), `rpc_server.rs`.
- Docs: `in-container.md` (*Sessions*: who attaches),
  `protocol.md`.

## Order across plans

1. [resumable-exec](resumable-exec.md) step 4, "now" part.
2. Step 1 here first (fleet's link needs its flow-controlled `Open`),
   then steps 2-3 here and [fleet](fleet.md) steps 1-3.
3. Step 8 here (`session.attach`).
4. Fleet step 4 (`PTY` over the RPC stream; shipped: the fleet
   worker's daemon is `session.attach`'s first caller, `fleet::pty`).
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
