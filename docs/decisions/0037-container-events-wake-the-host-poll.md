# 0037: Container events wake the host poll; the timer is a safety net

Status: accepted. Supersedes the cadence in 0018 (the shared poll stays).

## Context

With `serve install` the daemon runs from login with `--keep-alive`, and
its one container poll (0018) ran `<runtime> ps` every 5 s for its whole
life, mostly to learn that nothing changed. Docker and podman push
container changes over `events`.

## Decision

- The poll keeps one `<runtime> events --filter type=container` child
  (`serve::events::EventWatcher`, argv from `Backend::events_argv`). A
  whitelisted event on a `devsandbox-` container wakes a full pass at once;
  exec events never do (the bridges' own `exec`s would loop).
- Sleep (`serve::host::tick`): 2 s while a root is watched; else 60 s
  (`QUIET_POLL`) while the stream is live and every bridge is `Ready`;
  else 5 s (`POLL`). A bridge turning `Failed` wakes the poll.
- No stream (Apple `container`, podman without an events log, runtime
  down) is exactly the old 5 s poll; the stream is retried every 10 s.

## Consequences

- An idle daemon costs one blocked child instead of a CLI spawn every 5 s;
  container changes reach bridges, forwards and `instances.changed` at once
  instead of within 5 s.
- What no event announces (a config edit making a running instance a
  holder, a host agent appearing without `bridges.ensure`) waits up to
  60 s while idle.

Source: the runtime-events plan.
