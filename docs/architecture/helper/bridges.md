# Bridges: the host side of the helper link

A bridge is one `exec -i -u root <container> /run/devsandbox/bin/devsbd bridge`
child whose stdio carries the frame protocol. The `devsandbox serve` daemon
keeps one per running helper instance and serves what the container asks
for over it: ssh-agent streams, notifications, control requests; and it
calls the container's daemon over it (`RPC`). Read this when touching who
spawns bridges, their lifecycle, what a container request turns into on the
host, or how the host calls into a container.

Code: `src/devsbd/bridge.rs` (`Bridge`, `Bridges`, `BridgeWorker`,
`BridgeBoard`, `Link`, `spawn_observed`, `spawn_for`, `attach`, `open_stream`,
`handle_notify`, `handle_control`, `dispatch_control`, `apply_message`,
`AgentCandidates`), `src/serve/helper.rs` (`HelperClient`, `Call`, `serve`),
`src/serve/host.rs` (`poll`, `Waker`), `src/serve/api.rs`
(`bridges_ensure`), `src/serve/client.rs` (`ensure_bridge`,
`ensure_bridge_running`), `src/commands/exec.rs` (`AgentRelay`),
`src/devsbd/desktop.rs`, `src/commands/dispatch.rs`.

## Who owns bridges

- **The serve daemon**, always (unix-only, like `serve`). Its `serve-poll`
  thread lists devsandbox containers on each container event and on a timer
  (`host::tick`: `host::POLL`, 5 s, while a bridge is pending or failed or
  there's no event stream; [host poll](../daemon/host-poll.md#the-poll)) and
  hands the running ones to the bridge worker thread (`Bridges::spawn_worker`), which
  owns every bridge and coalesces bursts into one `reconcile`. The CLI and the
  dashboard never keep their own agent/notify bridges.
- **The forward engine**: `spawn_for` makes a short-lived bridge per forward
  route, without notify/control services. `attach` runs the same handshake
  and mux over a child the caller spawned (and may have read a header
  from), agent-less; `spawn_observed` is the `exec` spawn in front of it.
  See
  [../daemon/forwards.md](../daemon/forwards.md).

## Reconcile

`Bridges::reconcile(running, urgent)` keeps one bridge per running container
whose instance `wanted` one, and drops the rest (dropping a `Bridge` kills its
`exec`; the in-container bridge sees EOF and exits):

- wanted = (relay mode **and** the host has a live agent) **or** (the
  instance has a helper, `devsbd_arch` set). The daemon always has a sink, so
  in practice every helper instance gets a bridge; the agent decides only
  whether it serves `SSH_AGENT`. A bridge whose agent wiring no longer
  matches (host agent appeared or vanished) is replaced at once.
- Before every (re)spawn it runs `devsbd::ensure_recorded`, so a CLI upgrade
  reaches running containers ([overview.md](overview.md)).
- Retry (`retry_decision`): a dead bridge is respawned after `RETRY` (10 s), or
  `MISMATCH_RETRY` (300 s) after a protocol mismatch; an `urgent` container
  is respawned at once.
- Runtime unreachable: the last list stands; bridges aren't torn down on a
  blip.

### `bridges.ensure`

A client that needs the relay right now sends `bridges.ensure {instance,
agent?, wait?}` ([../../reference/api.md](../../reference/api.md)). The
daemon records `agent` (the client's own `$SSH_AUTH_SOCK`) as a candidate,
wakes the poll with that container urgent (a ticket on the `BridgeBoard`),
and with `wait` answers once that ticket's reconcile ran and the handshake
decided (`ready` / `error`), capped a bit over the 10 s handshake timeout.

CLI callers (`AgentRelay`, unix only):

- `exec`: `AgentRelay::start` asks on a background thread without waiting,
  only in relay mode with a host agent.
- `run`'s lifecycle chain and `start`'s `postStartCommand`:
  `AgentRelay::ready` waits up to 12 s (they may `git clone` over ssh at
  once); not ready is one `note:`, the command runs anyway.
- Dispatcher child ops inside the daemon: `AgentRelay::wait_in_daemon`
  (`ensure_bridge_running`, never lazy-starts a daemon).

The client holds its connection while the command runs, which makes it a
daemon holder. `ensure_bridge` lazy-starts the daemon unless `--in-process`.

## Lifecycle of one bridge

`spawn_observed`:

1. Spawn the `exec`, stdio piped, stderr captured (quiet: the TUI may own
   the terminal).
2. Handshake thread: `proto::handshake`; a watchdog kills the child after
   `HANDSHAKE_TIMEOUT` (10 s). Failure messages: version mismatch phrased by
   direction ("helper … is outdated: restart the instance" / "this
   devsandbox is older"), the bridge exiting `MISMATCH_EXIT`, or the bridge's
   stderr (e.g. `daemon not running`).
3. On success: send `Caps` (`own_caps`: `SSH_AGENT` with a live agent,
   `NOTIFY | CONTROL` only with a sink), publish the mux for forwards and,
   for a daemon bridge, its `Link` on the board (*Host → container
   requests*), report `Ready` on the board, start the keepalive (dead peer
   → kill the child), then `Mux::serve` until the stream ends; the board
   then shows `Failed("the bridge ended …")`, drops the link, and the next
   reconcile retries.

Wire details: [protocol.md](protocol.md).

## What a bridge serves

`open_stream` maps each daemon `Open` by channel; anything else is refused.

### ssh-agent

Each `SSH_AGENT` stream connects to `host_agent()`, chosen per stream: the
newest live socket among client-reported candidates (`AgentCandidates`, up
to 8), else the daemon's own `$SSH_AUTH_SOCK`. Agent rotation needs no
restart. See [ssh-agent.md](ssh-agent.md).

### Notifications (`NOTIFY`)

`handle_notify` reads one record to `Eof` (30 s timeout, size-capped),
decodes it (`notify::decode`; undecodable → reply `bad record`, which the
daemon treats like any non-`ok`: keep the file, retry later), and hands a `Notification` (instance key + `instance_id` + message) to
the sink. Only after the sink succeeds does it reply `ok`, so delivery is
durable: a host that dies between the two leaves the record in the outbox.

The sink (`apply_message`) applies the record to the shared Inbox store
(`inbox::decide` → `inbox::ops::sink`); thread records are first checked with
`dispatch::declares_inbox` (the sandbox must declare `inbox = true`). A store
write that changed nothing shows nothing. Otherwise:

- a desktop popup through `desktop::notify_desktop` (`notify-send` on Linux,
  `osascript` on macOS, silent if missing), rate-limited per instance
  (`RateLimit`: burst 3, one token per 10 s, same `(instance, key)` coalesced
  for 60 s);
- the status line `<instance>: <msg>` to `OnShown`, which the daemon
  publishes as `inbox.shown`.

### Control requests (`CONTROL`)

`handle_control` reads one encoded `control::Request` (30 s, ≤ 64 KiB),
then:

- `events-follow` → `dispatch::follow`, which holds the stream and writes
  JSON lines until it ends;
- anything else → `dispatch_control` → `dispatch::handle(key, req)`, one
  encoded `Response` back. Undecodable → `usage`.

`dispatch::handle` authorizes against the config as it is now (dispatcher
declared, `spawn` list, ownership, `max-instances`, …) and runs ops as
`devsandbox` subprocesses; see `src/commands/dispatch.rs`. Ops that write
`state.toml` (`ensure`, `stop`, `rm`, `done`) from a declared dispatcher are
serialized host-wide; reads, run ops and requests that will be denied skip
the lock. Every daemon bridge advertises `CONTROL`; non-dispatchers get
`denied`.

Bounds: at most 8 notify/control handler threads per bridge (`MAX_HANDLERS`).
In-flight control requests (waits and followers included) count as daemon
holders (`control_in_flight`), so the daemon doesn't idle out under them.

## Host → container requests

The serve daemon calls into a container's `devsbd daemon` over the same
bridge, one `RPC` stream per call ([protocol.md](protocol.md#rpc)).

**The registry.** Each daemon bridge's handshake thread publishes a `Link`
(its mux, the daemon's caps, an `attach` id unique to that bridge) on the
`BridgeBoard` right before it reports `Ready`; `BridgeBoard::link` hands it
out. It goes when the bridge turns `Failed` (handshake failed, or it ended),
when a new bridge for the container spawns, and when a reconcile drops the
container. Links and statuses share the board's lock and spawn ids, so a
replaced bridge's late link never shadows its successor. The forward
engine's own bridges publish nothing.

**`HelperClient`** (`src/serve/helper.rs`, one per daemon over the board)
is the one way in:

- `call(container, method, params, meta, caller, abandoned)`: no link →
  `NoBridge`; a daemon without `caps::RPC` → `Unsupported` (the caller
  falls back to `exec`). Then the call waits its turn: at most `MAX_CALLS`
  (16) per container in flight, the rest queued in arrival order, each
  checking `abandoned` while it waits. Each host-opened stream counts
  against the bridge's 256 host streams (`MAX_HOST_STREAMS`, shared with
  port forwards' `Connect`s), so RPC keeps well clear of them. Then
  `Link::open_rpc` (`Mux::open_host` on `RPC`), the `call` record with
  `meta.caller` stamped over whatever `meta` had (who asked; for
  `helper.call` the connection's client, `cli`, `tui` or `api:<name>`), and
  `Eof`: calls send no input.
- `Call::next` reads the reply records up to the last (`result`, `error`,
  `exit`); a stream that ends without one is an error with the reason
  (cancelled, the peer's `Close` reason, the bridge gone).
- Cancel (`Call::canceller`, or dropping an unfinished `Call`) is
  `Mux::close_stream("cancelled")`: `Close` to the daemon, which drops the
  stream; the reader sees the end at once.
- One `serve.log` line per call, started or not: `helper <container>:
  <method> from <caller>: <outcome> (<ms> ms)`, the outcome `ok`, `error
  <code>: …`, `exit <n>`, `signal <n>`, `cancelled`, `ended: …` or why it
  didn't start.
- `offers(container, method)`: whether the daemon lists `method` in
  `rpc.methods` (a bare name matches any version), asked once per bridge
  attach and cached; `false` for a helper without `RPC`.

The daemon also publishes its client process-wide (`helper::install`,
`helper::shared`) for callers running inside it without a handle: the
dispatcher's run ops, on the bridges' control handler threads, call the
child's `run.*` methods through it
([dispatchers](../automations/dispatchers.md#ops)).

The API reaches it through the internal `helper.call`
([../../reference/api.md](../../reference/api.md#helpercall)): held on its
connection like a verb, records relayed as notifications, and a hang-up
cancels the call.

## Boot hook (host role)

The host writes `/run/devsandbox/boot` (`devsbd::sync_boot`, `boot_spec`) on
`run`/`start`/`rebuild` when the runtime restarts the container itself
(`autostart = "runtime"` on a backend with restart policies) and a helper is
installed; otherwise it removes it. No bridge is involved: `devsbd boot` runs
it locally. See [in-container.md](in-container.md).

Limitations: [status.md](../../status.md#helper)
