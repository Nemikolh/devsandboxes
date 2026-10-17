# Container runtimes

How devsandbox talks to docker, podman and Apple `container`. Read before
touching any runtime call or adding a runtime capability.

Code: `src/runtime/mod.rs` (`Backend`, `backend()`, `default_backend_name`,
`backend_named`, `NAME_PREFIX`, `fixed_path`), `src/runtime/dockerlike.rs`
(`Dockerlike::DOCKER`/`PODMAN`), `src/runtime/apple.rs` (`AppleContainer`),
`src/runtime/bounded.rs`, `src/runtime/procs.rs`, `src/runtime/events.rs`.

## Backend

Every runtime invocation goes through the `Backend` trait; the rest of the
code never spells a binary name (outside docker-gated tests). One
process-wide backend, resolved once by `backend()`.

- **Shared process runners** (default methods): `run_inherit`, `output`
  (stderr inherited), `output_quiet` (stderr captured, first line folded into
  the error: safe while the TUI owns the screen), `output_merged` (logs),
  `run_checked`, `run_with_stdin`.
- **Queries** (per runtime): `is_running`, `label`, `network_exists`,
  `inspect_json`, `list` (rows as `ContainerRow`: name, image, status,
  normalized `state`, labels, host ports), `stats`, `server_version`,
  `logs_tail`, `proc_list`, `image_entrypoint`, `events_argv` (see
  [events](#events)).
- **Mutations that differ in shape**: `remove_force`, `network_run_args` +
  `connect_networks`, `service_alias_args` + `wire_service_dns`,
  `set_restart_policy`, `remove_volume`, `signal_proc` (`exec kill -<n>`, same
  everywhere).
- **Capabilities**: `supports_cache_from`, `supports_file_binds`,
  `supports_privileged`, `supports_restart_policy`. Callers branch on these,
  never on the runtime name.

Builds, `run`, `exec`, `start`/`stop` use the shared docker-shaped
subcommands through the runners (e.g. `services::build_args` builds the
`build` argv for both sandboxes and services).

## Selection

`default_backend_name`:

1. `DEVSANDBOX_RUNTIME=docker|podman|container` (`apple` is an alias for
   `container`). An unknown value warns and falls through.
2. On macOS: `docker` when the `docker` client answers `docker --version`
   (client-only, no daemon contact: OrbStack, Docker Desktop), else Apple
   `container`.
3. Elsewhere: `docker`.

OrbStack and Docker Desktop are just the `docker` backend.

## docker and podman (`Dockerlike`)

One struct, two constants differing only in binary name. Go-template
`inspect -f`, `ps --filter`, `--network-alias`, `network connect`, `top`.
The `ps --format {{json .}}` parser accepts both docker's strings and
podman's arrays/maps.

- Networks: the first one at `run` (`--network`), the rest via
  `network connect`.
- Services are reached by `--network-alias`; `wire_service_dns` is a no-op.
- `proc_list` prefers `exec ps -eo pid,ppid,args` (container-namespace pids,
  signalable); images without `ps` fall back to `top`, whose host pids are
  shown but flagged not signalable (`ProcList::container_pids`).
- Restart policies supported. On podman, `autostart = "runtime"` warns that
  boot restarts need `podman-restart.service`, which devsandbox never
  installs (`autostart::restart_decision`).

## Apple `container` (`AppleContainer`)

Shares the `run`/`exec`/`build`/`start`/`stop` surface; everything else
differs:

- `inspect`/`ls` are JSON-only (Swift property names); listing filters by
  name prefix client-side. `delete --force` removes; `logs -n` tails.
- All networks are passed at `run` (`--network` repeated); no
  `network connect`, so `connect_networks` is a no-op.
- No `--network-alias`: `wire_service_dns` inspects each service
  container's IPv4 and rewrites the sandbox's `/etc/hosts` in place (as root,
  `fixed_path` script). Called after `run`, on `start`, and when
  `services rebuild` rewires running sandboxes. See
  [services](instances/services.md).
- `proc_list` always uses `exec ps` (no `top`).
- `stats` counters are raw: CPU renders as `-`, memory is shown.
- `ContainerRow.status` is the bare state (`running`/`stopped`), not docker's
  `Up 3 minutes`.
- `image_entrypoint` picks the host-arch variant from `image inspect`.
- Capabilities all false:
  - no `--cache-from`: `build.cacheFrom` is dropped with a warning;
  - no file binds (directories only): a feature's file/socket bind mount is
    skipped with a warning (`run::mounts::feature_mount_decision`) and the
    ssh-agent bind-mount fallback is off (`run::ssh_agent::ssh_agent_forward`);
    `shell-rc` files go in through their parent directory; config-declared
    `mounts` are passed to the runtime unchecked;
  - no `--privileged`: `privileged` (sandbox or feature-requested) is skipped
    with one warning per requester (`run::image::privileged_decision`);
  - no restart policy: `autostart = "runtime"` behaves as `true`
    (devsandbox-driven autostart).

VS Code attach on Apple `container`: see
[mounts and state](instances/mounts-and-state.md).

## Names and labels

Everything devsandbox creates starts with `NAME_PREFIX` (`devsandbox-`), so
it shows in plain `docker ps` and listings filter on it. Drift labels
(`config_hash`, `build_hash`): see
[rebuild and drift](instances/rebuild-and-drift.md).

## Scripts run in containers

A container's env and `PATH` are untrusted and these execs often run as
root, so scripts run with `SH` (`/bin/sh` by absolute path) and
`fixed_path` (`PATH` pinned to `FIXED_PATH`).

## bounded

`runtime::bounded::run`: a child with stdout capped (past the cap it is
killed, `truncated` set), stderr capped, and a wall-clock timeout (killed and
reaped, whole process group on unix). For host calls whose output or duration
a container controls: dispatcher run ops, `devsbd run ls`, the dispatch
`devsandbox` subprocess. Silent, so safe under the TUI.

## procs

`runtime::procs` (unix-only) is what the daemon's `instances.procs` answers
from: `parse_top` (the `ps -eo pid,ppid,args` listing), `parse_runs` and
`run_ls` (the helper's `devsbd run ls`, bounded to 64 KiB / 5 s; any failure
means "no runs"). The dashboard's process forest lives in `tui::procs`.

## events

`Backend::events_argv` is the argv of a long-lived child printing one JSON
line per container event, so the daemon can be told of changes instead of
only listing on a timer (`serve::events::EventWatcher`). docker runs
`events --filter type=container --format {{json .}}`, podman the same with
`--format json`; Apple `container` has no event stream (`None`: poll only).
`runtime::events::parse` reads both shapes (docker's `Action` +
`Actor.Attributes.name`, podman's `Status` + `Name`) and
`runtime::events::relevant` keeps a whitelist: `devsandbox-` containers being
created, started, restarted, stopped, killed, dying, removed, renamed,
paused or unpaused (docker's `die`/`destroy` and podman's `died`/`remove`
both). Everything else is dropped, exec events above all: the daemon's
bridges `exec` into containers, so a wake on `exec_start: …` would make each
listing trigger the next.

## Adding a runtime

A docker-CLI-compatible one is another `Dockerlike` constant plus its name in
`backend_named` and `default_backend_name`. Anything else implements
`Backend`; the query methods parse runtime output, with one exception:
`services::gc` reads `network ls -q` itself and matches network names (which
docker's `-q` doesn't print: it prints IDs).

Limitations: [status.md](../status.md#runtimes)
