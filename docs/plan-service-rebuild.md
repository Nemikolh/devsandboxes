# Plan: dockerfile drift detection + `service rebuild` / `service ls`

## Problem

1. `config_hash` hashes the TOML table only (`src/config.rs:635`). A dockerfile
   _path_ is in the hash; its _contents_ are not. Editing a Dockerfile flags no
   drift anywhere: `rebuild` says "no config drift; nothing to do"
   (`src/commands/rebuild.rs:55`), services are never recreated, the TUI shows
   nothing.
2. Services have no rebuild verb. The only remedies are `gc` (works only when
   unreferenced) or hand-`docker rm`.
3. The TUI Services tab shows no drift (`ServiceRow` has no `drift` field,
   `src/snapshot.rs:86-105`) and offers no rebuild action.
4. No `service ls` CLI.

## Design decisions

- **Build-input hash, separate label.** New container label
  `devsandbox.build_hash` = `short_hash` of the dockerfile bytes at creation
  time; empty string for image-based sandboxes/services. `config_hash` keeps
  its meaning (the merged TOML). Drift = config hash differs OR build hash
  differs. Missing label (pre-upgrade containers) or unreadable dockerfile →
  no drift, matching the existing lenient rule in `snapshot::drifted`.
  _Limitation (documented): the build context and devcontainer features are
  not hashed — only the dockerfile file itself._

- **Apple path: rewire, don't restart.** `wire_service_dns`
  (`src/runtime/apple.rs:248`) rewrites `/etc/hosts` in place via
  `exec -u root … sh -c`, and works on a running container (`start.rs:53`
  already relies on this). After recreating a service container, re-exec it on
  every _running_ sandbox that references the service. Stopped sandboxes get
  rewired by their next `start`. No sandbox restart on any runtime; a rewire
  failure propagates as an error naming the instance so the user can restart
  it manually.

- **TUI freshness: no watcher, no lazy check.** The snapshot collection
  already runs every 2s off the UI thread (`src/tui/mod.rs:46`) and already
  does per-container docker inspects for drift. A local file hash per
  collection is noise next to that; fold it in and drift is live within 2s.

## Step 1 — build-input hash (`feat(config)`)

- `src/config.rs`: add `pub fn build_hash(dir: &Path, build: Option<&Build>) -> String`
  next to `short_hash` — `short_hash` of the dockerfile bytes
  (`dir.join(build.dockerfile)`), `""` when there is no build, no dockerfile
  key, or the file is unreadable. Unit tests: image-based → empty, file change
  → hash change, missing file → empty.

- Label at creation:
  - sandbox containers: `src/commands/run.rs:451-456` — add
    `devsandbox.build_hash=<hash>` label (hash from
    `sandbox.properties.build`).
  - service containers: both label arrays in `ensure_services`
    (`src/commands/services.rs:77-98`) — hash from `service.spec.build`.

- Compare at every drift site (all gain a `dir`-derived expected build hash;
  rule: recorded label `Some(h)` differing from a _non-empty_ expected → drift;
  `None` label or empty expected → rely on config hash alone):
  - `src/commands/rebuild.rs:68` `needs_rebuild` — dockerfile edits must
    trigger `rebuild`.
  - `src/commands/run.rs:417` `warn_on_drift` (+ its `start.rs` caller if any).
  - `src/commands/services.rs:126` `ensure_service` warning.
  - `src/snapshot.rs:558` `drifted` — the TUI Instances tab then flags
    dockerfile edits with zero UI work.
    Factor one shared helper (e.g. `pub fn container_drifted(container, expected_config, expected_build) -> Result<bool>`
    in `commands/mod.rs` or `snapshot.rs`) so the four sites can't diverge.

- Tests: hash helper in `config.rs`; drift-rule table test for the shared
  helper (label present/absent × expected empty/non-empty).

## Step 2 — `service rebuild <name>` (`feat(services)`)

- `src/main.rs`: new subcommand group
  `Service { #[command(subcommand)] cmd: ServiceCommand }` with
  `Rebuild { name: String }` (Ls arrives in step 3). `gc` stays top-level.

- `src/commands/services.rs`: `pub fn rebuild(dir, name) -> Result<()>`:
  1. `config.resolve_service(name)` — unknown service bails.
  2. Enumerate backing containers by labels, like `gc`
     (`services.rs:329-341`): project + `devsandbox.service=<name>`;
     `remove_force` each (report removed names).
  3. Derive referencing instances from state + resolved sandboxes (same walk
     as `gc`, `services.rs:301-326`). For each _running_ referencing
     instance: `ensure_services(config, dir, project, instance, &[name])`
     — recreates the container on the right network with fresh labels; the
     create path re-runs `docker build`, whose cache picks up dockerfile
     changes — then `backend().wire_service_dns(&instance_container, &endpoints)`
     (no-op on docker/podman, /etc/hosts rewrite on Apple).
  4. Global service with no running referencing instance: recreate once on
     the global network via the same `ensure_services` machinery with a
     synthetic instance? No — call `ensure_network` + `ensure_service`
     directly with the global container name/labels (mirror the
     `ServiceScope::Global` arm of `ensure_services`).
  5. Isolated service with no running instances: nothing to recreate
     (containers exist only per instance); print what was removed.

- Tests: pure parts only (no runtime in CI) — factor the "which containers /
  which instances reference service X" derivation into a pure function over
  `(config, state, ps-rows)` and unit-test it; model on `build_service_rows`.

## Step 3 — `service ls [--json]` (`feat(services)`)

- Factor the services block of `snapshot::collect` (`src/snapshot.rs:498-541`)
  into `pub fn service_rows(dir, config, state, ps) -> (Vec<ServiceRow>, Vec<String>)`
  (or similar), reused by `collect` and the new command so the two cannot
  drift.

- `service ls`: table with `NAME SCOPE SOURCE PORTS USED BY STATUS`,
  modeled on `src/commands/ls.rs` (plain/colored lockstep, `column_widths`,
  `format_row`); `--json` emits `Envelope::new(rows)` like `ls --json`
  (`src/commands/ls.rs:19-25`), sharing `ServiceRow` serialization.
  Runtime down → statuses render `Missing`, errors to stderr, table still
  prints (same spirit as the snapshot).

- Step 4 adds `drift` to `ServiceRow`; `service ls --json` then carries it
  for free.

## Step 4 — TUI: service drift + `:rebuild` on Services tab (`feat(tui)`)

- `ServiceRow` gains `drift: bool` (OR over backing containers). The impure
  label reads happen in `collect` beside the instance `drifted()` calls and
  feed `build_service_rows` as pre-joined input (keep the join pure +
  testable).
- *Learned in step 1 review:* `container_drifted` costs two `docker inspect`s
  per container. The snapshot's single `ps` listing already carries labels
  (`ContainerRow::label`, used by `gc`) — compute service drift by joining
  against the `ps` rows (`drift_decision` is already pure and public to the
  crate), adding zero docker calls to the 2s loop. Do not add per-service
  `backend().label` inspects.

- `src/tui/ui.rs`: yellow marker in the services table row (match the
  instances treatment) + a detail-panel line like `ui.rs:883-888`.

- `src/tui/app.rs`: `:rebuild` on the Services tab queues the
  `service rebuild <selected>` action through the same run-action loop
  instances use (`app.rs:867` region); completion command list at
  `app.rs:1090` gains the services-tab case. State-machine tests beside the
  existing ones (`s_on_exited_drifted_instance_queues_rebuild` et al.).

## Step 5 — docs (`docs(readme)`)

- README: `service rebuild`, `service ls`, build-hash drift semantics + the
  context/features limitation. AGENTS.md module map line for
  `services::rebuild/ls` if the services.rs description changed shape.

## Constraints for implementers

- `cargo test` green per step; no clippy/rustfmt available here.
- Never touch git — the orchestrator commits.
- Keep doc comments explaining _why_; keep the pure/impure split in
  `snapshot.rs` and `services.rs` (pure joins unit-tested, docker I/O at the
  edges).
- Conventional commits, all lowercase (orchestrator writes them).
