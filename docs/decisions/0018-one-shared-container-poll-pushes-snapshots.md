# 0018: One shared container poll; the daemon pushes snapshots

Status: accepted

## Context

Each dashboard used to poll the runtime itself (listing every 2 s, stats
every 7 s), and the daemon polled separately for bridges and forwards. N
dashboards cost N listings and N stats calls, and a `docker stats` call can
take over a second.

## Decision

- `serve::host` runs one loop: each pass lists every devsandbox container
  once and feeds the bridges, the forwards and the watched roots'
  snapshots from that one listing (every 5 s, 2 s while a root is watched).
- Dashboards call `instances.watch {dir}`; the daemon owns the cadence
  (`serve::snapshots`: stats at most every 7 s, cpu/mem carried over) and
  pushes `instances.snapshot` notifications, coalesced, latest wins per
  root.
- Notifications are coalesced flags written by a per-connection notifier;
  a slow client is cut off rather than buffered without bound.
- `instances.list` stays uncached for the CLI and npm; per-instance process
  lists (`instances.procs`) stay per request.

## Consequences

- Runtime load is independent of the number of dashboards.
- The listing runs every 5 s for the daemon's lifetime (bridges and
  forwards need it); watching a root only tightens it to 2 s and adds the
  snapshot and stats work.

Source: docs/before-machines.md, "Decisions" (Shared poll); docs/api.md, "Notifications".
