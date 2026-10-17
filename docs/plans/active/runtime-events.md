# Runtime events: drive the host poll from `docker events`

Status: in progress

## Problem

The daemon's one container poll (`src/serve/host.rs` `poll`) runs
`<runtime> ps --all --filter name=devsandbox-` every `POLL` (5 s) even when
nothing is watched and nothing changes. With `serve install` the daemon runs
from login with `--keep-alive`, so that's a CLI spawn every 5 s forever.
Docker and podman can push container state changes instead (`docker events`,
`podman events`): one long-lived child that is idle until something happens.

## Goal

While an event stream is up, no dashboard watches a root and every bridge is
settled, the poll sleeps `QUIET_POLL` (60 s, a safety net) instead of `POLL`;
a relevant container event wakes it at once, so the running set, bridges,
forwards, holders and `instances.changed` react *faster* than today. Without
an event stream (Apple `container`, podman with `events_logger = "none"`, the
runtime down, the child died) behavior is exactly today's.

Not in scope (flag, don't build): the accept loop's 100 ms `TICK`
(`src/serve/daemon.rs` `run`) and the 500 ms `WATCH` stamp checks stay;
watched cadence (`LISTING_INTERVAL`, `STATS_INTERVAL`) is unchanged.

## Design

- **Source.** `Backend::events_argv(&self) -> Option<Vec<&'static str>>`
  (default `None`; Apple keeps it). docker:
  `events --filter type=container --format {{json .}}`; podman:
  `events --filter type=container --format json`. No `event=` filters in
  argv (docker says `die`, podman `died`); the parser whitelists.

- **Parser** (pure, in `src/runtime/events.rs`): one line → `Option<Event
  { name, action }>`. Docker: `Action` + `Actor.Attributes.name`; podman:
  `Status` + `Name`. Relevant = name starts with `NAME_PREFIX` and action is
  one of `create`, `start`, `restart`, `die`, `died`, `stop`, `kill`,
  `destroy`, `remove`, `rename`, `pause`, `unpause`. Everything else
  (`exec_create`/`exec_start`/`exec_die` — the bridges' own `exec`s generate
  these! — `health_status: …`, `attach`, `top`, …) is ignored, or the poll
  would wake itself in a loop.

- **Watcher** (`src/serve/events.rs`, thread `serve-events`): loop { spawn
  the child (stdin null, stdout piped, stderr null: the daemon's stderr is
  `serve.log`); mark `live`, call `wake()` (re-list after subscribing, so no
  gap); read lines, `wake()` on each relevant one; on EOF/read error mark
  not live, `wake()`, log one `serve.log` line on the up→down transition and
  one on down→up; wait `EVENTS_RETRY` (10 s) on a stop condvar }. A spawn
  failure is "down" too. Drop: set stop, kill + wait the child (unblocks the
  reader), join. The spawn is injectable (a `Command` builder or argv) so
  tests run `sh -c 'printf …'`. No events argv → no thread, never live.

- **Poll.** `Host::start` starts the watcher with `wake` = the poll's
  `Waker::refresh` path (it already re-lists and reconciles everything; the
  `refresh` flag coalesces bursts). The sleep becomes the pure
  `tick(watched, events_live, bridges_settled)`: watched →
  `LISTING_INTERVAL`; else events live and settled → `QUIET_POLL`; else
  `POLL`. `bridges_settled` = `BridgeBoard` has no `Pending` or `Failed`
  entry: a dead bridge's retry (`retry_decision`, `RETRY` 10 s) and a
  pending handshake are only acted on by a poll pass, so they keep the
  5 s cadence.

- **What the safety net covers:** missed events, a config edit that makes a
  running instance a holder (`holds_host`), a host agent appearing without a
  `bridges.ensure`. Those now take up to 60 s while idle; that's the trade.

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

  Starting a sandbox never waits: `run` emits `create`/`start` and calls
  `bridges.ensure`. Holder count only feeds the idle exit, so a keep-alive
  (installed) daemon is unaffected by case 1.

## Step 1: event source

Add the runtime side and the watcher, unwired.

- `src/runtime/mod.rs`: `Backend::events_argv` next to `list` (around the
  `fn list` declaration, ~line 240), default `None`, doc comment saying why
  (push instead of poll; `None` = poll only).
- `src/runtime/dockerlike.rs`: implement per `self.name` (docker vs podman
  `--format`). `src/runtime/apple.rs`: nothing (default).
- `src/runtime/events.rs` (new, `mod events;` in `runtime/mod.rs`): the
  parser and `relevant` as above. Unit tests with real-shaped docker and
  podman lines: start/die/died/destroy/remove/rename accepted;
  `exec_start: sh -c …`, `exec_die`, `health_status: healthy`, a
  non-`devsandbox-` name, garbage, an empty line rejected.
- `src/serve/events.rs` (new, unix like the rest of `serve/`; add to
  `src/serve/mod.rs` module list and its module doc): `EventWatcher` with
  `start(argv: Option<Vec<String>>, bin, wake: Box<dyn Fn() + Send>, log:
  fn(&str)) -> EventWatcher` (shape is yours, keep the spawn injectable),
  `is_live() -> bool`, `Drop` as in Design. `EVENTS_RETRY` const; tests may
  need it shorter, so make the retry gap a field set by the constructor.
  Unit tests with `sh`: relevant lines wake, irrelevant don't; child exit →
  not live, woken, respawned after the gap; drop kills a blocking child
  (`sh -c 'sleep 1000'`) promptly; no argv → never live.
- One `#[test_utils::docker_test]` (see `src/test_support.rs`, sibling
  docker tests for the pattern, returns `Result`): start the watcher on the
  real `docker` argv, `docker run -d --name devsandbox-evtest-<rand>
  alpine sleep 30` (or whatever image sibling tests use), assert a wake
  within a few seconds, clean up with `with_cleanup`.
- Docs: `docs/architecture/runtimes.md` gets a short paragraph on
  `events_argv` (which runtimes have it, the whitelist and why exec events
  are dropped). AGENTS.md module map: one line for `serve/events.rs` and
  `runtime/events.rs`.
- Do not touch `host.rs` yet.

## Step 2: wire into the poll

- `src/serve/host.rs`: `Host` owns the `EventWatcher` (drop order: stop the
  watcher before or with the poll; no wake after the poll is gone matters,
  `Waker` already ignores it). Wake = the same thing `Waker::refresh` does
  (factor a fn over `Shared` if cleaner). `QUIET_POLL` const (60 s) next to
  `POLL`, doc comment says it's a safety net. `tick` gains `events_live` and
  `bridges_settled`; extend the existing
  `one_listing_feeds_the_running_list_and_the_tick_follows_watches` test
  (or a new one) over the truth table.
- `src/devsbd/bridge.rs`: `BridgeBoard::settled(&self) -> bool` (no entry
  `Pending` or `Failed`), unit-tested.
- Update the module doc of `host.rs` (cadence sentence) and `Waker::refresh`
  doc (now also "a container event").
- Docs, present tense: `docs/architecture/daemon/host-poll.md` ("The poll":
  cadence, the event stream, the safety net and what it covers, the
  fallback); `docs/architecture/helper/bridges.md:21` (cites `host::POLL`
  every 5 s); `docs/reference/api.md` `instances.changed` row (cadence);
  AGENTS.md `host.rs` line ("every 5 s, 2 s while a root is watched").
- `docs/status.md` *Known limitations → daemon*: with events live and
  nothing watched, holder changes from config edits and a host agent
  appearing without `bridges.ensure` are noticed within `QUIET_POLL`
  (`host::tick`); Apple `container` has no event stream (`events_argv`).

## Checks

`cargo test --workspace` after each step (docker is available here, so the
docker test runs). No clippy/rustfmt in this toolchain.
