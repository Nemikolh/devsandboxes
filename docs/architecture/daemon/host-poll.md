# The host poll, snapshots and notifications

The daemon's one container poll and everything that reads it: bridges,
configured forwards, holders, `instances.changed`, and the watched roots'
`instances.snapshot` pushes. Read this before adding anything that asks
the runtime periodically: it should ride this poll, not start its own.

Code: `src/serve/host.rs` (`Host`, `poll`, `tick`, `POLL`, `QUIET_POLL`,
`Waker`, `Observers`, `holds_host`, the `serve-autostart` thread),
`src/serve/events.rs` (`EventWatcher`, `EVENTS_RETRY`),
`src/runtime/events.rs` (`parse`, `relevant`), `src/serve/snapshots.rs`
(`Snapshots`, `LISTING_INTERVAL`, `STATS_INTERVAL`, `stats_due`, `Join`,
`Remotes`, `merge`), `src/serve/daemon.rs` (`Watcher`, `Outbox`, `Pending`,
`subscribe`, `watch_root`, `unwatch_root`, `Shared::publish_*`),
`src/serve/api.rs` (`Topic`, `watch_dir`), `src/devsbd/bridge.rs`
(`Bridges`, `BridgeBoard`, `BridgeBoard::settled`, `BridgeBoard::on_failed`,
`report_agent`).

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
`bridges.ensure` never waits on them.

The poll thread also runs the runtime's event stream (`EventWatcher`, a
`serve-events` thread following `Backend::events_argv`: `docker events` /
`podman events`, container events only). Each relevant event (a
`devsandbox-` container created, started, stopped, died, removed, renamed,
paused; never the bridges' own `exec_*` events) wakes the poll at once, as
does the stream coming up (events before the subscribe are lost) or going
down. The sleep between passes is `tick`:

- `LISTING_INTERVAL` (2 s) while anything is watched;
- else `QUIET_POLL` (60 s) while the event stream is live and the bridges
  are settled (`BridgeBoard::settled`: none `Pending` or `Failed`);
- else `POLL` (5 s). A pending handshake and a dead bridge's retry
  (`retry_decision`, `RETRY` 10 s) are only acted on by a pass, so they
  keep this cadence. A bridge turning `Failed` during a quiet sleep (its
  handshake failed, its helper died) wakes the poll
  (`BridgeBoard::on_failed`), which then sleeps `POLL`. A bridge the
  worker hasn't spawned yet isn't on the board; it settles to `Ready` with
  no pass needed, or fails and wakes the poll. A bridge whose `exec`
  can't even start (`BridgeBoard::set` from `reconcile`) doesn't wake it:
  with no live entry there's no retry gap, so a wake would loop.

With no stream the poll is the plain `POLL` timer: Apple `container` has
no `events_argv`, and podman with `events_logger = "none"`, a runtime
that's down or a child that died all leave the stream not live; the
watcher respawns it every `EVENTS_RETRY` (10 s), logging the loss and the
recovery once each in `serve.log`. `QUIET_POLL` is the safety net for what
no event announces: a missed event, a host agent appearing without a
`bridges.ensure`. Config edits don't wait for it: `serve-watch` stamps
every known root's config files and wakes the poll on a change
([Notifications](#notifications)), and each pass re-reads them
(`holds_host`, the snapshots).

```
                       poll thread sleeping
                              │
     ┌────────────────────────┼─────────────────────────────┐
     │ container event        │ client call                 │ nothing
     │ (docker events)        │ (bridges.ensure, watch)     │
     ▼                        ▼                             ▼
 devsandbox run/start/   run/start/exec/TUI           timer expires:
 stop/rm, docker restart  report agent + wake          5 s  if no stream,
 at boot, crash (die)                                       or a bridge is
     │                        │                             Pending/Failed,
     └──────────► wake NOW ◄──┘                        2 s  if a TUI watches,
                     │                                 60 s otherwise
                     ▼                                      │
          list containers, reconcile bridges,  ◄────────────┘
          forwards, holders, instances.changed
```

A `Waker` cuts the sleep short too: `wake(container)` for
`bridges.ensure` (that container urgent), `refresh()` for a new watch or a
`state.toml` change while watched; a container event takes the `refresh`
path. Dropping `Host` stops the poll, which drops the watcher (its child
killed) and then the bridge worker.

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

The same thread stamps the config files of every root `state.toml` records
plus every watched root (`config_stamps`: each `config::candidates` path,
present or not, so an edit, a new file and a rename between names all
count). A stamp that moves under an already-known root wakes the poll
(`Waker::refresh`); a root that's merely new gets its pass from the
`state.toml` write or the watch that added it.

Watches live with the connection: a hang-up drops them, and
`unsubscribe` from `instances` clears them.

## Autostart

At start a `serve-autostart` thread runs the `autostart = true` pass once
for every config root recorded in `state.toml`; its notes go to
`serve.log`, and it's an `autostart` holder while it runs. The pass is
once per boot per root (`State.autostart_boot`), so `run`/`rebuild`/`start`
calling it too is harmless. Detail: the automations docs.

Limitations: [status.md](../../status.md#daemon)
