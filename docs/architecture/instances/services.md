# Services

Containers a sandbox depends on (databases, caches) declared under
`[services.*]` and listed in a sandbox's `services`. Read before touching
service creation, networks, `gc` or `service ls`. Field reference:
[config-toml-spec](../../../skills/config-toml-spec/SKILL.md).

Code: `src/commands/services.rs` (`project_id`, `network_name`,
`instance_network`, `service_container`, `isolated_service_container`,
`ensure_services`, `ensure_service`, `service_image`, `build_args`,
`service_run_args`, `prefer_same_host_ports`, `check_port_conflicts`,
`service_refs`, `gc`, `rebuild`, `ls`), `src/commands/stop.rs`
(`service_containers`), `src/commands/rm.rs` (`remove_container`),
`src/snapshot.rs` (`service_rows`).

## Scope and naming

All names carry `project_id` (8 hex chars of the config root's hash), so two
roots never collide. Isolated names use the instance's persistent
`instance_id`, so a renamed instance still owns its services.

| scope | container | network |
|---|---|---|
| `isolated` (default) | `devsandbox-svc-<project>-<instance_id>-<name>`, one per instance | `devsandbox-net-<project>-<instance_id>` |
| `global` | `devsandbox-svc-<project>-<name>`, one per config root | `devsandbox-net-<project>` |

Labels: `devsandbox.service`, `devsandbox.project`, `devsandbox.scope`,
`devsandbox.instance` (isolated only), `devsandbox.config_hash`,
`devsandbox.build_hash`. Every lookup (stop, rm, gc, rebuild, ls) goes by
labels, never by stored refcounts.

Services have no `extends`; a sandbox inherits `services` entries from
templates because arrays concatenate under `extends` (see
[config](../config.md#extends-and-deep_merge)).

## ensure_services

Called by `materialize` (before the instance container is created), by
`start`, and by `service rebuild`. For each listed service:

1. `resolve_service` (exactly one of `image` / `build`).
2. Create the scope's network if missing.
3. `ensure_service`: exists → warn on drift, `start` if stopped; absent →
   create. Creating builds a `build` service into
   `devsandbox-img-svc-<project>-<name>` (`service_image_tag`; `build_args`,
   shared with sandbox builds),
   prefers the same host port for bare `-p` specs when it's free
   (`prefer_same_host_ports`), fails early if another running container
   publishes a requested host port (`check_port_conflicts`), then `run -d`
   on its network with `env`, `ports`, `command`.

It returns the networks the instance joins and the `ServiceEndpoint`s
(`alias` = service name) it must resolve.

## Reaching a service

A sandbox reaches a service as `<name>:<port>`.

- docker/podman: the service runs with `--network-alias <name>`
  (`service_alias_args`); the sandbox joins its first network at `run` and
  the rest via `network connect`.
- Apple `container`: all networks at `run`; `wire_service_dns` writes each
  service's IPv4 into the sandbox's `/etc/hosts`, after `run`, on every
  `start` and on `service rebuild`.

See [runtimes](../runtimes.md).

## Lifecycle with the instance

- `stop`: stops the instance container, then its isolated service
  containers (found by `devsandbox.project` + `devsandbox.instance`).
  Global services keep running.
- `start`: starts the container, then `ensure_services` (recreating any
  missing service) and `wire_service_dns`. When the config no longer
  resolves, it bare-starts the container and its labelled isolated services.
- `rm` (`remove_container`): removes the container, its isolated services
  and its instance network. Global services are left to `gc`.

## gc

`devsandbox gc [--force]`, per config root. References are derived from state
plus the runtime (`service_refs`): instances whose sandbox exists in this
config and whose container is running, or that are mid-`run` (in flight).

- Removes this project's service containers not referenced: a global one no
  live instance lists, an isolated one whose instance isn't live or no longer
  lists it.
- Network sweep: lists `network ls -q` and matches entries by name prefix,
  dropping `devsandbox-net-<project>` when no live instance uses a global
  service and each `devsandbox-net-<project>-<id>` whose instance isn't live
  (`network rm` failures ignored). docker prints network IDs for `-q`, so on
  docker no network matches and gc removes none.
- Also reaps orphaned shell-history dirs under
  `shared-volumes/history/` (confirmed per entry unless `--force`) and
  dangling `<data-dir>/devsandbox/agent/<id>.sock` links.

"Live" means running: a stopped instance's isolated service containers are
collected too; its next `start` recreates them from scratch via `ensure_services`.

## service ls

`devsandbox service ls [--json]`: `NAME SCOPE SOURCE PORTS USED BY STATUS`
for every service in the config, built from `snapshot::service_rows`, the
same rows the dashboard's Services view uses. A runtime that's down is data:
the error goes to stderr and statuses read `missing`.

## service rebuild

See [rebuild and drift](rebuild-and-drift.md#service-rebuild).

Limitations: [status.md](../../status.md#instances)
