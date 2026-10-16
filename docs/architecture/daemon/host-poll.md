# The host poll, snapshots and notifications

The daemon's one container poll and everything that reads it: bridges,
configured forwards, holders, `instances.changed`, and the watched roots'
`instances.snapshot` pushes. Read this before adding anything that asks
the runtime periodically: it should ride this poll, not start its own.

Code: `src/serve/host.rs` (`Host`, `poll`, `POLL`, `Waker`, `Observers`,
`holds_host`, the `serve-autostart` thread), `src/serve/snapshots.rs`
(`Snapshots`, `LISTING_INTERVAL`, `STATS_INTERVAL`, `stats_due`, `Join`,
`Remotes`, `merge`), `src/serve/daemon.rs` (`Watcher`, `Outbox`, `Pending`,
`subscribe`, `watch_root`, `unwatch_root`, `Shared::publish_*`),
`src/serve/api.rs` (`Topic`, `watch_dir`), `src/devsbd/bridge.rs`
(`Bridges`, `BridgeBoard`, `report_agent`).

## The poll

A `serve-poll` thread. Each pass lists every devsandbox container once
(`rt.list(true, NAME_PREFIX)`), then from the running ones:

1. bumps `Host::changes` when the set moved (→ `instances.changed`);
2. counts the `live_instances` holders (`holds_host`: `dispatcher` or
   `inbox = true`);
3. hands the list to the forward registry (configured `forwardPorts`
   follow it, [forwards.md](forwards.md));
4. hands it, with urgent containers, to the bridge worker
   (`Bridges::spawn_worker`, its own thread owning every bridge);
5. while any root is watched, collects snapshots (below).

The bridges get the list before any stats or snapshot work, so a
`bridges.ensure` never waits on them. Cadence: `LISTING_INTERVAL` (2 s)
while anything is watched, else `POLL` (5 s). A `Waker` cuts the sleep
short: `wake(container)` for `bridges.ensure`, `refresh()` for a new watch
or a `state.toml` change while watched.

With the runtime unreachable the last list stands (bridges and holder
count kept, not torn down on a blip); the outage and the recovery are one
`serve.log` line each. Watched roots still get a snapshot, saying the
runtime is down.

## Bridges

One bridge per running instance with a helper: it relays the host
ssh-agent, drains the container's outbox into `inbox.json` (the notify
sink), shows desktop popups, and serves in-container control ops
(dispatcher child ops, `events`, `thread ls`). Only the daemon bridges;
neither the dashboard nor CLI commands do (a forward has its own,
[forwards.md](forwards.md)). Bridge internals: [helper/bridges.md](../helper/bridges.md).

`bridges.ensure {instance, agent, wait?}` puts the caller's
`SSH_AUTH_SOCK` first among the agent candidates (`report_agent`, up to
`MAX_AGENTS` = 8 newest, then the daemon's inherited one), wakes the poll
with that container urgent, and answers at once, or with `wait` (capped at
`MAX_ENSURE_WAIT`, 15 s) once the bridge's handshake is decided
(`BridgeBoard`). `run`'s and `start`'s lifecycle chains wait up to 12 s.

A message the sink stores that earns a status line goes to `inbox`
subscribers as `inbox.shown` (latest line only).

## Snapshots

A client that shows a config root continuously sends `instances.watch
{dir, machines?}` (after `subscribe` to `instances`). While at least one
connection watches anything, each pass:

- asks for stats (`<runtime> stats --no-stream`, the expensive call) when
  `stats_due`: at most every `STATS_INTERVAL` (7 s), at once for a root
  that never had them. A newly watched root is pushed first without stats
  (Disk → Listing → Full);
- joins every watched root against that one listing (`Join`:
  `snapshot::collect_from` + `thread_children`), carrying each running
  container's last cpu/mem between stats passes so every push is complete;
- pushes the result as `instances.snapshot` to that root's watchers.

So N dashboards on M roots cost one listing per pass and one stats call
per 7 s. The cache (`Snapshots`) keeps the last push per watched root and
drops roots nobody watches. A new watch gets the cached snapshot at once,
or a disk-only one (no runtime call) and a `refresh` of the poll. With
`machines: true` every push is merged with the machines' rows
(`snapshots::merge`, [machines.md](machines.md)). `instances.list` stays
per request and uncached, for the CLI and scripts.

## Notifications

`subscribe {topics}` / `unsubscribe`: `inbox`, `instances`, `forwards`
(`Topic`). A connection's first `subscribe` starts its `serve-notify-<n>`
thread. One daemon-wide `serve-watch` thread (`Watcher`) notices changes
(this process's store writes at once, other processes' within `WATCH`,
500 ms, by file stamp; `state.toml` by its stamp; the poll's `changes`) and
only *flags* them on each subscribed connection's `Outbox`; it does no
I/O, so a slow client can't stall the others. Flags coalesce in `Pending`:
one `inbox.changed {generation}` however many writes, latest `inbox.shown`,
one `instances.changed`, latest `instances.snapshot` per root,
`forwards.changed`, latest `forwards.status`. Every write to a connection
goes through its one writer lock; a client that doesn't drain its socket
for `WRITE_TIMEOUT` (5 s) is disconnected. Exiting, the daemon sends
`closing {reason}` ([role.md](role.md#drain)).

Watches live with the connection: a hang-up drops them, and
`unsubscribe` from `instances` clears them.

## Autostart

At start a `serve-autostart` thread runs the `autostart = true` pass once
for every config root recorded in `state.toml`; its notes go to
`serve.log`, and it's an `autostart` holder while it runs. The pass is
once per boot per root (`State.autostart_boot`), so `run`/`rebuild`/`start`
calling it too is harmless. Detail: the automations docs.

Limitations: [status.md](../../status.md#daemon)
