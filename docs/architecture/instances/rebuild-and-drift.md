# Rebuild and drift

How devsandbox notices that a container no longer matches the config, and
how `rebuild` and `service rebuild` bring it back in line. Read before
touching the hashes, the drift rule or either rebuild.

Code: `src/commands/mod.rs` (`container_drifted`, `drift_decision`),
`src/commands/rebuild.rs` (`rebuild`, `needs_rebuild`, `recreate`,
`resolve`, `resumable`), `src/commands/services.rs` (`rebuild`,
`ensure_service`), `src/commands/run/mod.rs` (`run_container`,
`warn_on_drift`, `materialize`), `src/snapshot.rs` (`listed_drifted`),
`src/config.rs` (`sandbox_hash`, `build_hash`, `sandbox_build_hash`).

## Labels

Every instance and service container is created with
`devsandbox.config_hash` and `devsandbox.build_hash` labels. How each hash is
computed (merged table minus `autostart`/`dispatcher`/`inbox`/`forwardPorts`;
dockerfile contents plus local-feature contents) is in
[config.md](../config.md#hashing). The build context and OCI feature contents
are not hashed; `rebuild --force` covers changes there and devsandbox-side
changes the hashes can't see (rc wiring, mount layout).

## The drift rule

`drift_decision(config_label, expected_config, build_label, expected_build)`
is the one rule every site shares:

- config drift: the label is present and differs;
- build drift: the label is present, the expected build hash is non-empty,
  and they differ.

A missing label (older container, image-based sandbox) or an empty expected
build hash is never drift, so old containers don't flap.
`container_drifted` reads the labels through the runtime; the snapshot
(`listed_drifted`) applies the same rule to labels already in the `ps`
listing, so the dashboard and `service ls` pay no per-container inspect.

Sites:

- `start` → `warn_on_drift`: warns and suggests `devsandbox rebuild <name>`,
  then starts anyway.
- `ensure_service` (an existing service container on `run`/`start`): warns;
  the container is not recreated.
- `rebuild` → `needs_rebuild`.
- the snapshot: the dashboard's `drift` flag on instance and service rows.
  In the dashboard, `s` on an exited, drifted instance runs a detached
  `rebuild` instead of a start; so does `s` on an instance whose container is
  missing.

`run` never reuses a container: an existing instance name is refused (see
[lifecycle](lifecycle.md)).

## rebuild

`devsandbox rebuild <name>` (alias `recreate`; `--all`, `--force`)
recreates an instance's container from the current config while keeping its
identity and working tree:

1. Refuse an instance still in flight. A `failed` one is always rebuilt
   (that's its retry), unless its `run` was interrupted before the worktree
   existed (`resumable`).
2. `resolve`: the instance's `project` must match this config root's
   `project_id` and its sandbox must still resolve, else bail (unlike
   `start`, which falls back to a bare container start).
3. `needs_rebuild`: the shared rule, plus one rebuild-only case: no
   `config_hash` label (container gone or removed by hand) means rebuild.
   It's the only worktree-preserving way back. No drift and no `--force`:
   no-op with a message.
4. `recreate`: `remove_force` the container, then `run::materialize` with
   the recorded folder, base folder, worktree, branch and `branch_created`,
   under a `Tracker` in phase `building`, output into the instance's build
   log.

Kept: instance name and `instance_id`, worktree and branch, recorded
`volumes` (per-instance volumes are only deleted by `rm`), shell history,
`${instance}`-anchored host paths, the recorded dispatcher and `extra_env`.
Refreshed from config: workspace, `remoteEnv`, `remoteUser`, the
`.code-workspace` file, mounts, services, lifecycle commands.

A failure is never rolled back: the instance stays `failed` with its
worktree for another `rebuild` or an `rm`. Ctrl-C marks it failed
("cancelled"). `--all` skips in-flight/failed instances and ones that don't
resolve here, with a note, instead of aborting the batch.

## service rebuild

`devsandbox service rebuild <name>` recreates a service's containers and
rewires running sandboxes in place, with no sandbox restart on any runtime:

1. Remove every container labelled with this `devsandbox.project` and
   `devsandbox.service`.
2. For each running instance of this root that references the service:
   `ensure_services` recreates the container (rebuilding a `build` service's
   image, fresh labels), then `wire_service_dns` re-points the live
   sandbox. On docker/podman that's a no-op: the network alias follows the
   new container. On Apple `container` it rewrites the sandbox's
   `/etc/hosts` in place.
3. A `global` service nobody running references is still recreated once on
   the shared network.

Isolated services of stopped instances are just removed; their next `start`
recreates and rewires them. A rewire failure is an error naming the
instance. Recreating rather than restarting is what applies a dockerfile
edit. See [services](services.md).

Limitations: [status.md](../../status.md#instances)
