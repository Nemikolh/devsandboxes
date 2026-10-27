# Container runtimes

How devsandbox talks to docker, podman and Apple `container`. Read before
touching any runtime call or adding a runtime capability.

Code: `src/runtime/mod.rs` (`Backend`, `backend()`, `default_backend_name`,
`backend_named`, `NAME_PREFIX`, `fixed_path`), `src/runtime/dockerlike.rs`
(`Dockerlike::DOCKER`/`PODMAN`), `src/runtime/apple.rs` (`AppleContainer`),
`src/runtime/bounded.rs`, `src/runtime/procs.rs`, `src/runtime/events.rs`,
`src/runtime/identity.rs`, `src/commands/run/identity.rs`.

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
  [events](#events)), `identity_fast_path` and `identity_canary` (see
  [mode detection](#mode-detection)).
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

## Mode detection

The same devcontainer user fields mean opposite things on rootful docker,
rootless docker, rootless podman and VM-backed runtimes, so the runtime's
user-namespace mode (`runtime::identity::Mode`) is probed, with whether
binds need SELinux relabeling, into a `runtime::identity::Probe`.
`commands::run::identity::probe(image, reprobe)` decides it (`decide`, I/O
injected) in this order:

1. **Cache**: `State.identity_probes` in `state.toml`, keyed by
   `runtime::identity::context_key`, unless `reprobe`.
2. **Fast path**: `Backend::identity_fast_path`, the runtime's own metadata.
3. **Canary**: `Backend::identity_canary`, when the fast path is ambiguous
   or on `reprobe`.

A fresh result is written through `State::update`. Apple `container`
answers `VmBacked` before any of this: every container is its own VM, and
no command runs.

**Fast path.** docker reads `docker info --format '{{json .}}'`
(`runtime::identity::docker_fast_path`), with `DOCKER_HOST`:
- a `tcp://` or `ssh://` endpoint: ambiguous (binds name the remote host's
  paths);
- a non-Linux host, or a VM marker (`OperatingSystem` Docker Desktop /
  OrbStack / Rancher Desktop, a colima/lima/orbstack/desktop hostname
  `Name` or `ClientInfo.Context`, a VM's socket dir in `DOCKER_HOST`):
  `VmBacked`;
- `name=rootless` in `SecurityOptions`: `RootlessDocker`;
- `name=userns` (a `userns-remap` daemon): ambiguous;
- otherwise `Rootful`.

SELinux is `name=selinux` (docker labels containers only with
`selinux-enabled`). podman reads `podman info --format json`
(`runtime::identity::podman_fast_path`), with `CONTAINER_HOST`: a non-Linux
host, or `host.serviceIsRemote` without a local `unix://` endpoint (a podman
machine), is `VmBacked`; else `host.security.rootless` picks
`RootlessPodman` or `Rootful`. SELinux is `host.security.selinuxEnabled`.
podman's metadata is never ambiguous.

**Canary.** A throwaway container from the sandbox's own image (`run --rm
--pull=never --network=none --entrypoint /bin/sh`, so nothing is pulled),
with an empty mode-0777 host dir bound in (`:z` when the fast path saw
SELinux), creates one file as `--user 0:0` and one as `1000:1000`
(`dockerlike::canary_argv`). The host `stat`s both, and
`runtime::identity::classify` maps the owners (and the backend's engine)
to a mode; an unknown outcome is an error. The dir sits next to
`state.toml` (`identity-probe-<pid>`), under `$HOME`, which VM file shares
cover by default, and is removed after. An image without `/bin/sh`, or an
engine whose binds don't reach this host's filesystem, fails the canary
with an error. The canary never tests SELinux: that flag always comes from
the fast path.

**Cache key.** The runtime context as the runtime's CLI resolves it, never
the machine that asked (a machine probes for itself, with its own
`state.toml`): `docker:host=<DOCKER_HOST>`, else
`docker:context=<DOCKER_CONTEXT>`, else the docker CLI config's
`currentContext` (`$DOCKER_CONFIG/config.json`, else
`~/.docker/config.json`), else `docker:context=default`;
`podman:host=<CONTAINER_HOST>`, else
`podman:connection=<CONTAINER_CONNECTION>`, else `podman:default`. A
changed key is a fresh probe. A change behind the same key (a reinstalled
engine, podman's default connection switched) needs `--reprobe`.

**`--reprobe`** on `run` and `rebuild` (`RunParams::reprobe` /
`RebuildParams::reprobe` over the API, `--reprobe` in the verb argv) skips
the cache and runs the canary even when the metadata is clear. It reaches
`materialize` as `RunExtras::reprobe` and isn't recorded on the instance.
`rebuild --reprobe` alone doesn't count as drift.

What the mode then decides (who the container is created and exec'd as,
the user namespace, relabeled binds, the helper's remap) is in
[instances/identity.md](instances/identity.md). It reads the image's
facts.

## Image facts

What `run` reads from the built image (`state::ImageFacts`,
`run::facts::image_facts`): `Backend::image_env` (its `ENV`: a root plan's
tool env extends its `PATH`, `${containerEnv:…}` reads it),
`Backend::image_user` (its `USER`, the adopted account when the config
names none) and `Backend::image_passwd` (its `/etc/passwd`: the adopted
account's real ids and home). The passwd comes from a throwaway container,
like the canary (`dockerlike.rs` `passwd_argv`: `--pull=never`,
`--network=none`, `--user 0:0`, `sh` as the entrypoint, `cat
/etc/passwd`); exit 126/127 (no `sh`, no `cat`) reads as no passwd. Apple
`container` has none (a VM per run).

They're cached in `state.toml` (`State::image_facts`) by image id
(`Backend::image_id`, `image inspect {{.Id}}`): content-addressed, so never
stale. At most `IMAGE_FACTS_CAP` (32) entries; `State::remember_image_facts`
drops the oldest by `at`. Caching is all or nothing (`facts::decide`):
when any of the three reads fails, nothing is cached and the next `run`
reads them all again; the fact that failed reads as unset. Without an id
(Apple `container`) they're read on every `run`.

## Adding a runtime

A docker-CLI-compatible one is another `Dockerlike` constant plus its name in
`backend_named` and `default_backend_name`. Anything else implements
`Backend`; the query methods parse runtime output, with one exception:
`services::gc` reads `network ls -q` itself and matches network names (which
docker's `-q` doesn't print: it prints IDs).

Limitations: [status.md](../status.md#runtimes)
