# Port forwarding: on-demand host ports for instances and services

## Goal

VS Code's _Ports_ tab, for devsandbox: on demand, make a port inside an
instance or a service reachable on the host (`localhost:3000` → the node dev
server in instance `api`, `localhost:5432` → `api`'s postgres). No container
restart, no `-p` at create time, works on docker / podman / Apple `container`.

Two entry points, same engine:

- **CLI** `devsandbox port …`: runs in the foreground, forwards until Ctrl-C.
- **TUI** _Ports_ tab (third tab, next to Instances and Services): add/remove
  forwards live; they end when the dashboard quits.

Static, config-declared forwards (`forwardPorts`) are out of scope (see
_Future: static `forwardPorts`_).

## Why tunnel through devsbd (not `-p`, not a socat sidecar)

- `-p` can't be added to a running container on any runtime; recreating
  restarts the service and loses unpersisted state.
- A sidecar publishing a port can only reach `0.0.0.0` binds. A dev server on
  `127.0.0.1:3000` is only reachable from *inside* the container's network
  namespace — which is where devsbd's daemon already runs.
- devsbd already gives us an `exec -i` stdio frame channel to every instance
  (docs/sandbox-helper.md), with keepalive, self-heal, and a protocol built for
  additive frames (`caps`, unknown kinds skipped, host-allocated stream-id
  space reserved).

```
curl ──tcp──▶ devsandbox (host listener 127.0.0.1:3000)
                 │ frames over `exec -i <c> devsbd bridge`
                 ▼
              devsbd daemon (in <c>) ──tcp──▶ 127.0.0.1:3000   (instance forward)
                                      └─tcp──▶ postgres:5432   (service, instance route)
```

## Design decisions

### Routes

- **Instance forward:** tunnel into the instance's container, daemon dials
  `127.0.0.1:<port>`.
- **Service forward — instance route (default):** tunnel into a *running
  instance that references the service*, daemon dials `<service>:<port>` over
  the shared network (network alias on docker/podman, `/etc/hosts` on Apple —
  musl's resolver reads it). Nothing is injected into third-party images.
  - isolated service: the owning instance (the one named on the command line);
  - global service: that instance if given and it references the service,
    otherwise any running instance of this config root that references it
    (first by name, deterministic).
- **Service forward — injection fallback:** no usable instance (none running,
  or its helper is unavailable) → `devsbd::ensure` into the service container
  on demand and dial `127.0.0.1:<port>` there. Needs `/bin/sh`, root `exec`, a
  writable `/run` (distroless images fail with the usual one-line reason).
- Routes are re-resolved on every (re)connect, so a `service rebuild` or an
  instance restart heals without restarting the forward.
- An instance without a recorded helper (`devsbd_arch = None`, e.g. created
  before embedding) gets `devsbd::ensure_recorded` on demand before failing.

### Lifetime and ownership

- Forwards live in the process that created them: the CLI command (until
  Ctrl-C — SIGINT hits the whole foreground process group, so the `exec`
  children die with it; no signal crate needed) or the TUI (until quit).
- No persistence in `state.toml` in v1; the Ports tab lists only this TUI's
  forwards.
- Each forward owns its own bridge (`exec -i … devsbd bridge`). Sharing one
  bridge per container across forwards is a later optimisation.

### Binding

- Default bind address `127.0.0.1`; `--address <ip>` opts in to others
  (`0.0.0.0`).
- `<port>` alone: host port = container port; if taken, fall back to an
  OS-assigned port and print it. `<host>:<port>` explicit: fail if taken.

### Protocol additions (no `VERSION` bump)

All additive, per the rules in docs/sandbox-helper.md (_Frame protocol_):

| kind | frame | direction | meaning |
|---|---|---|---|
| 7 | `Connect { stream, host, port }` | host → daemon | open a TCP stream to `host:port` from inside the container. `stream` has `HOST_ID_BIT` set. Payload `u16 port \| u8 host_len \| host`. |
| 8 | `Window { stream, credit: u32 }` | both | receiver grants `credit` more bytes on `stream`. |
| 9 | `Eof { stream }` | both | sender's local side hit EOF (half-close); receiver `shutdown(Write)`s its local socket. |
| 10 | `Caps(u32)` | host → daemon | host capabilities, sent once on stream 0 right after the handshake. |

- `Close` may carry a UTF-8 reason payload (empty = none). Today's decoder
  already ignores `Close` payloads, so old peers are unaffected; the host shows
  it (`connection refused`, `no such host postgres`).
- **Caps bits:** `CAP_TCP_FORWARD = 1 << 0` (daemon handles `Connect` with flow
  control + half-close), `CAP_SSH_AGENT = 1 << 1` (host serves ssh-agent
  streams).
- **Negotiation through the bridge.** The bridge does two separate handshakes
  and then copies bytes, so caps don't flow end to end by themselves:
  - The bridge advertises to the host `own_caps & daemon_caps`: a forward is
    attempted only when the *daemon* that will serve it supports it. Otherwise
    the host reports `helper in <c> is outdated: restart the instance`.
  - The daemon learns the host's caps from `Caps`, which passes through the
    bridge verbatim. An old daemon skips the unknown kind.
- **Agent routing fix.** The daemon routes ssh-agent streams to the newest
  bridge whose host advertised `CAP_SSH_AGENT`. A bridge that never sent `Caps`
  (an older host) counts as agent-capable, which is today's behavior. Without
  this, a `devsandbox port` started from a shell with no agent would become the
  newest bridge and break ssh inside the container while it runs.

### Flow control (the big piece)

Today `Mux::serve` writes each `Data` straight into the local socket on the
frame-reader thread. One slow local reader stalls every stream on the bridge,
and the keepalive then misreads the stall as a dead peer (the _head-of-line_
caveat in docs/sandbox-helper.md). Bulk TCP (bundles, HMR websockets, DB
dumps) makes this the common case, not an edge case.

- **Credit per stream per direction.** Each side starts with
  `INITIAL_WINDOW = 256 KiB` of credit for the peer. A sender reads from its
  local socket only while it holds credit (`min(CHUNK, credit)`), and waits on
  a condvar at zero. The receiver returns credit with `Window` once the bytes
  are *written to its local socket*, batched: one `Window` when ≥ half the
  window has been consumed, not one per chunk.
- **The reader never blocks.** On a flow-controlled stream, `Data` goes into
  that stream's queue, and a per-stream writer thread drains it into the local
  socket. The queue is bounded by the window by construction. A peer that
  overruns its credit is a protocol violation: `Close` *that stream* (with a
  reason), not the connection.
- **Half-close** on flow-controlled streams. Local EOF → send `Eof`, keep
  reading the other direction. The stream ends when both directions have seen
  EOF, or on `Close`, or on a local error. The mux must remember per direction
  whether EOF has been seen.
- **Scope:** only streams opened by `Connect` are flow-controlled. ssh-agent
  streams keep today's semantics exactly (direct write, EOF → `Close`): they
  are small request/response traffic, and changing them would couple this plan
  to the agent relay's regression surface.
- **Stream type.** `Mux` holds `UnixStream` today. It becomes a small
  `Conn` enum (`Unix(UnixStream)`, `Tcp(TcpStream)`) with `read` / `write` /
  `shutdown` / `try_clone`. No trait objects, no new deps; devsbd stays std-only.
- **Caps on streams:** `MAX_STREAMS` (64) stays for daemon-allocated ids. Host
  `Connect`s get their own cap of 256 live streams (browsers open many
  connections), enforced on the daemon side.
- **Non-blocking connect.** The daemon dials on a fresh thread, never on the
  serve thread: resolve with `to_socket_addrs`, then `connect_timeout` 5s per
  address. On failure it sends `Close` with the error text.

### Listening process (optional, `lsof`)

Show which process owns the forwarded port (`node (pid 412)`), à la VS Code's
_Running Process_ column.

- Host runs, quietly (`output_quiet`), in the container where the port lives:
  `exec <c> lsof -nP -iTCP:<port> -sTCP:LISTEN -Fpc`. `-F` output is
  machine-parseable (`p<pid>` / `c<command>` lines). *Where the port lives*:
  instance forward and injection route → the dialed container; via-instance
  service route → the **service** container (the instance can't see its
  processes). Plain `exec`, so no devsbd needed there.
- Optional: `lsof` missing (exit 127 / not found), no match, or non-zero exit →
  no process shown, never an error. Cached per forward; refreshed when the
  forward (re)connects and at most every 10s while shown, off the UI thread.
- Multiple listeners (e.g. v4 + v6 of the same process, or `SO_REUSEPORT`)
  → distinct `(pid, command)` pairs, joined with `, `.
- _Orchestrator suggestion, not requested:_ fall back to `ss -ltnpH
  sport = :<port>` when `lsof` is absent (busybox/alpine images often lack
  lsof but ship `ss` via iproute2). Left out unless you want it.

### Target naming (CLI)

```
devsandbox port <instance> [--service <svc>] [--address <ip>] <[host:]port>...
devsandbox port --service <global-svc> [--address <ip>] <[host:]port>...
```

- `<instance>` resolves via `commands::resolve_instance` (instance | id |
  sandbox | folder basename).
- `--service` without an instance is only valid for a `global` service.
- Several port specs forward in parallel from one command.
- Output, one line per forward:
  `127.0.0.1:3000 -> api:3000` and
  `127.0.0.1:5432 -> postgres:5432 (via instance api)`, suffixed with
  `[node (pid 412)]` once the listening process is known.
  Per-connection errors go to stderr as `note:` lines; the command keeps
  running.

## Steps

Steps 1–3 are the flow-control core. **They go to one dedicated implementer,
resumed for each step**, so it keeps the context of the design. Each step is
still reviewed and committed on its own. Steps 4+ get fresh implementers.

After any change under `devsbd/` or to `src/devsbd/{proto,mux}.rs`, rebuild the
helper with `scripts/build-devsbd.sh` before `cargo test --workspace`.
`embedded_host_helper_reports_version` and the docker-gated tests run the
embedded blob, so a stale blob fails them.

### Step 1 — protocol frames [x]

`src/devsbd/proto.rs` only. Pure, no behavior change anywhere else.

- Add `Frame::Connect { stream, host, port }`, `Window { stream, credit }`,
  `Eof { stream }`, `Caps(u32)`, and an optional reason on `Close`
  (`Close { stream, reason: String }`, empty = none; update every constructor
  in `mux.rs`, `daemon.rs`, and tests). Kinds 7–10 as in the table.
- Add `pub mod caps { TCP_FORWARD, SSH_AGENT }`, and `pub const
  INITIAL_WINDOW: u32 = 256 * 1024`.
- `proto::handshake` gets a `caps: u32` argument and returns the peer's caps
  alongside its hash. Callers pass `0` for now (step 4 wires real values).
- Malformed `Connect` (short, bad utf-8 host, empty host, port 0) →
  `InvalidData`, like `Open`.
- Tests: round-trip for every new kind; byte layout of `Connect`/`Window`;
  a `Close` with a reason decodes, and an *old-style* empty-payload `Close`
  still decodes; the frozen-bytes test is unchanged.

### Step 2 — `Conn` abstraction in the mux [x]

`src/devsbd/mux.rs` (+ call sites in `src/devsbd/bridge.rs`,
`devsbd/src/daemon.rs`). Pure refactor, no behavior change.

- `pub enum Conn { Unix(UnixStream), Tcp(TcpStream) }` with `read`, `write`,
  `shutdown(Shutdown)`, `try_clone`; `From<UnixStream>`, `From<TcpStream>`.
- `streams: HashMap<u32, Arc<Conn>>`; `attach` / `serve`'s `on_open` take and
  return `Conn`.
- Every existing mux test passes unchanged, apart from constructor adaptations.

### Step 3 — flow control + half-close + host-initiated streams [x]

`src/devsbd/mux.rs`. This is the heart of the plan; see _Flow control_ above.

- Per-stream state: `flow: Option<Flow>` (credit available to send + condvar,
  bytes consumed not yet granted back, write queue + writer thread,
  eof-sent / eof-received flags). `None` = legacy stream (ssh-agent),
  unchanged code path.
- `Mux::connect(self: &Arc<Self>, conn: Conn, host, port) -> io::Result<u32>`:
  allocates the next host id (`HOST_ID_BIT | n`, wrapping within the high
  half, never reusing a live id), registers a flow-controlled stream, and
  sends `Connect`.
- `serve` gains an `on_connect(stream, host, port, reply)` callback, the
  daemon's hook. It must return immediately; the callback dials on its own
  thread and then calls `reply(Ok(Conn))` to attach the flow-controlled stream
  or `reply(Err(reason))` to send `Close { reason }`. Id policy for `Connect`:
  high bit set, not live, under 256 live host streams; otherwise `Close`.
  Credit starts at `INITIAL_WINDOW` in both directions when the stream is
  registered. Data that arrives before the dial finishes is queued, which is
  bounded by credit.
- `Window` adds credit and wakes the pump. `Eof` → `shutdown(Write)` on the
  local conn once the write queue has drained. Credit overrun → `Close` with
  `"flow control violation"`.
- The host-side `on_close` for a stream carries the reason, so step 5 can
  surface it. The mechanism is your choice (callback or channel); keep it
  generic.
- Tests (in-process mux pairs over OS pipes, like the existing `pair()`):
  - 64 MiB through one stream arrives byte-identical;
  - a stalled local reader on stream A doesn't delay stream B, and B's
    round-trip stays fast while A is fully backed up (proves no head-of-line);
  - the keepalive doesn't fire while A is stalled (inject short durations);
  - credit overrun closes only that stream;
  - half-close: the client writes a request, `shutdown(Write)`, and still reads
    the full response;
  - `Connect` id policy (low bit refused, duplicate refused, cap);
  - a legacy ssh-agent stream still behaves exactly as before (the existing
    tests).

### Step 4 — daemon + bridge: serve `Connect`, negotiate caps, route the agent [x]

`devsbd/src/daemon.rs`, `devsbd/src/bridge.rs`, `src/devsbd/bridge.rs`.

- The daemon advertises `TCP_FORWARD` in its `Hello`. Its `on_connect` dials as
  specified (thread, resolve, `connect_timeout` 5s per address, `Close` with
  the reason on failure).
- The bridge advertises `own & daemon` caps to the host (it learns the
  daemon's caps from its first handshake).
- The host sends `Caps` right after its handshake: `SSH_AGENT` iff the bridge
  was spawned with an agent provider *and* `host_agent()` is `Some` at that
  moment. The daemon records caps per bridge (default: agent-capable until a
  `Caps` arrives), and `Bridges::route` skips non-agent bridges (`HOLD` still
  applies to "no agent-capable bridge").
- `spawn_with` exposes the negotiated peer caps and a way to open forward
  streams (`Bridge::connect(conn, host, port)`, or hand out the `Arc<Mux>`).
  Keep the agent-less variant usable: `spawn_with` taking
  `Option<agent provider>`.
- Tests: unit test for the routing rule (pure fn over per-bridge caps);
  extend the docker-gated test in `src/devsbd/bridge.rs` (or add a sibling) to
  check that an agent-less bridge newer than an agent bridge doesn't steal
  `ssh-add -l`, and that a `Connect` to a `busybox nc -l -s 127.0.0.1` in the
  container round-trips.

### Step 5 — host forwarder engine [x]

New `src/devsbd/forward.rs` (unix-only, like `bridge.rs`).

- `Forward::start(spec) -> io::Result<Forward>`. `spec` holds: the bind
  address, the host port (`Fixed(p)` or `Prefer(p)`, where `Prefer` falls back
  to an OS-assigned port), and a
  `resolve: Box<dyn Fn() -> Result<Route, String> + Send>` where
  `Route { container, recorded_arch, host, port, label }`.
- Binds the listener up front (the error is immediate and precise), then an
  accept thread. Each accepted `TcpStream` → `bridge.connect(...)`. The bridge
  is (re)spawned lazily via `resolve` when missing or dead. The retry gap
  reuses `bridge::retry_decision`'s spirit (no re-exec per connection while
  broken), and connections during an outage are closed at once.
- Observable state for the UI/CLI, pure and snapshot-able:
  `ForwardStatus { local_addr, route_label, state: Connecting | Active |
  Error(String), open_conns }`, and a stream of per-connection error notes.
- `Drop` closes the listener, kills the bridge, and joins the accept thread.
- Tests: pure retry/state transitions; a docker-gated end-to-end test with an
  alpine container running `busybox httpd -f -p 127.0.0.1:8080`: the forward
  serves `GET /`, and a 20 MiB file downloads intact.

### Step 6 — route resolution (instance / service, both routes) [ ]

New `src/commands/port.rs` (resolution only, no CLI yet).

- Pure core:
  `resolve_route(config, state, running, instance: Option<&str>, service:
  Option<&str>, port) -> Result<Planned>`, where `Planned` is
  `Instance { key, container }`, `ViaInstance { key, container, alias }`, or
  `Inject { service_container }`, following _Routes_. Reuse
  `services::service_refs` (make it `pub(crate)`) and the container-name
  helpers `service_container` / `isolated_service_container`.
- Impure edge: turn `Planned` into a `Route` closure for `Forward`, calling
  `devsbd::ensure_recorded` (instances) or `devsbd::ensure` (the injection
  target; no state to record — keep the arch in memory for the forward's
  lifetime).
- Errors name the fix: `service `postgres` is isolated: name the instance`,
  `no running container for service `redis``, and
  `devsbd couldn't run in … (image has no /bin/sh?)`.
- Tests: table tests over the pure resolver (isolated/global × instance
  given or not × running or not × helper present or not).

### Step 7 — CLI `devsandbox port` [ ]

`src/main.rs`, `src/commands/port.rs`, `src/commands/mod.rs`.

- Clap: `Port { name: Option<String>, #[arg(long)] service, #[arg(long,
  default_value = "127.0.0.1")] address, #[arg(required = true)] ports:
  Vec<String> }`. Parse port specs as `p` / `h:p` (pure, tested).
- Start every forward, print one line each (see _Target naming_), then block
  until SIGINT. Surface forward state changes and per-connection notes on
  stderr, deduplicated so a retry loop doesn't spam.
- Not on Windows yet: bail `port forwarding is unix-only for now`.

### Step 8 — listening process lookup [ ]

`src/commands/port.rs` (or a small `src/devsbd/forward.rs` hook; implementer's
call, justify it), see _Listening process_.

- `Route` / `Planned` carry `process_container` (dialed container, or the
  service container for the via-instance route).
- Pure `parse_lsof_f(&str) -> Vec<(pid, command)>` (dedup, order kept) and
  `format_procs` → `node (pid 412)`; table tests including empty output,
  multiple pids, a `c` line without `p`.
- Impure `listening_procs(container, port) -> Option<String>` via
  `backend().output_quiet`; any failure → `None`.
- `ForwardStatus` gains `process: Option<String>`; refreshed on (re)connect
  and at most every 10s, on the forward's own thread (never the caller's).
- CLI line gets the `[…]` suffix; it re-prints the line when the process
  changes (e.g. dev server restarted under a new pid).

### Step 9 — TUI Ports tab: state, prompt, rendering [ ]

`src/tui/app.rs`, `src/tui/spec.rs`, `src/tui/prompt.rs`, `src/tui/ui.rs`.
I/O-free.

- `Tab::Ports` as the third tab: `Tab::ALL`, `index`, `next_tab`/`prev_tab`
  (no longer a toggle), `selected: [usize; 3]`, key `3`.
- `App.ports: Vec<PortRow>` (from `ForwardStatus`, set by the loop).
  `pending_port: Option<PortRequest>` and `pending_unport: Option<id>`, like
  `pending_stop`.
- Prompt: `SPECS` entry
  `port <instance> [--service s] [--address a] <[host:]port>`, with a new
  `ArgValue::Service` completing service names. `PromptAction::Port`.
- Keys: `p` on Instances/Services opens the prompt prefilled with the
  selection (instance, or instance + `--service`); on Ports, `d` stops the
  selected forward.
- Rendering: a table with `LOCAL`, `TARGET`, `VIA`, `PROCESS`, `STATE`,
  `CONNS` (`PROCESS` = `-` when unknown), plus an empty state that explains
  `p`.
- Help modal + `docs/tui.md` keybindings.
- Tests: tab cycling over three tabs, selection clamp, prompt parse and
  completion for `port`, key → pending request mapping.

### Step 10 — TUI Ports tab: event-loop wiring [ ]

`src/tui/mod.rs`.

- A `ForwardWorker` thread, modelled on `BridgeWorker`: a command channel
  (`Add(request)`, `Remove(id)`) and a status channel back (a
  `Vec<ForwardStatus>` on change). It owns every `Forward`; its `Drop` closes
  the channel and joins, so every forward and bridge dies before the terminal
  is restored. Route resolution (config + state load, `ensure`) happens on the
  worker, never on the UI thread.
- Errors land in `app.status`. Nothing reaches stderr (the TUI owns the
  screen).

### Step 11 — docs sweep [ ]

- `docs/sandbox-helper.md`: the frame table, stream-id spaces (host ids are now
  used), the head-of-line caveat (resolved for forwarded streams), and agent
  routing by caps.
- README: a short `devsandbox port` section.
- Mark steps done here.

## Known limitations (v1)

- Unix hosts only (the mux is unix-only today; Windows needs the named-pipe
  work from docs/sandbox-helper.md).
- One bridge per forward (extra `exec`s when forwarding many ports from one
  container).
- CLI forwards are invisible to the TUI and vice versa.
- The injection route needs `/bin/sh` + root `exec` + writable `/run` in the
  service image.
- UDP not supported.

## Future: static `forwardPorts`

`forwardPorts` / `portsAttributes` / `appPort` on a sandbox are parsed and
flagged as not implemented (`src/config.rs:85`). They could ride the same
engine. The open question is **who owns the forward** when nothing is in the
foreground:

- Several instances of one sandbox all declare `3000`; only one can hold host
  `3000`. Options: first-come, per-instance port offset, or `Prefer(p)` with
  fallback, shown in `ps` and the TUI.
- Forwards need a long-lived host process. Options: the TUI while open
  (VS Code's model), a `devsandbox port --all` foreground command, or a
  background host daemon (new territory: lifecycle, logs, stale-state cleanup).
- Where the resulting mapping is recorded (`state.toml` per instance) so the
  CLI, TUI and `status --json` agree.
- `portsAttributes.onAutoForward` / `label` map onto the Ports tab's
  presentation.

Related follow-ups, not planned:

- Auto-detect listening ports by having the daemon read `/proc/net/tcp{,6}`, as
  VS Code does, so the Ports tab offers them.
- Share one bridge per container across forwards.
- Persist CLI forwards in state so the TUI can list them.
