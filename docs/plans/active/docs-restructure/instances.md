# Fragment: instances + config + runtimes

Note: the spawn cap (3) was held by other leads for the whole run, so the
lead wrote all eight docs directly instead of through children. Every claim
was checked against the code named in each doc's `Code:` line.

## Docs written

- `docs/architecture/instances/lifecycle.md`: names/ids, `resolve_instance`, `state.toml` + `State::update`, `run` → `materialize` steps, phases/failure/rollback, `start`/`stop`/`rm`/`rename`/`done`.
- `docs/architecture/instances/worktrees.md`: when an instance gets a worktree, branch source + start point, `folders` worktree modes, `.worktreeinclude` copy / `worktree-link` store, locking, removal.
- `docs/architecture/instances/rebuild-and-drift.md`: labels, `drift_decision` and its call sites, `rebuild` (kept vs refreshed), `service rebuild` rewiring.
- `docs/architecture/instances/services.md`: isolated/global naming, labels, `ensure_services`, reaching services per runtime, lifecycle with the instance, `gc`, `service ls`.
- `docs/architecture/instances/images-and-features.md`: base image/build, feature image + generated Dockerfile, OCI/local feature sources + cache, entrypoint chain, container command, feature mounts/privileged, build log.
- `docs/architecture/instances/mounts-and-state.md`: config as mount boundary, every mount `run` makes, ssh-agent fallback mount, host paths table, VS Code files/launch, `## Host git`.
- `docs/architecture/config.md`: config file discovery/legacy rename/YAML, `project_id`, three tables + validation, `extends`/`deep_merge`, `${…}` variables and where they expand, hashing, `daemon.config.toml`.
- `docs/architecture/runtimes.md`: `Backend` groups + capabilities, selection, `Dockerlike` vs `AppleContainer`, names/labels, fixed-path scripts, `bounded`, `procs`, adding a runtime.

Links assume `status.md` gets `### instances`, `### config`, `### runtimes`
headings under Known limitations (anchors `#instances`, `#config`, `#runtimes`).

## Known limitations (verified true today)

### instances
- `devsandbox gc` treats only running instances as live, so it removes a stopped instance's isolated services and its network (and the global network when no running instance uses a global service); docker then refuses `devsandbox start` on that container (network gone, `start` runs before `ensure_services`); `rebuild` recovers. Reproduced the docker half by hand (stopped container, `network rm`, `start` fails). → architecture/instances/services.md#gc; evidence: `services::gc` (`running` = `container_running` or in flight), `start::start_logged` (runs `start` before `ensure_services`).
- A drifted service is only warned about on `run`/`start`; the warning suggests `gc` + re-run rather than `devsandbox service rebuild`. → architecture/instances/rebuild-and-drift.md; evidence: `services::ensure_service`.
- Image tags and build contexts aren't namespaced by config root: `devsandbox-img-<sandbox>`, `devsandbox-img-<sandbox>-feat`, `devsandbox-img-svc-<name>`, `<cache>/devsandbox/features/build-<sandbox>/`. Two roots with the same sandbox/service name share (and can clobber) them. → architecture/instances/images-and-features.md; evidence: `run::image::base_image_for`, `build_features_image`, `services::service_image`, `features::build_context_dir`.
- `caches` mount at fixed `/root/…` targets, whatever `remoteUser` is. → architecture/instances/mounts-and-state.md; evidence: `config::known_cache`.
- Drift doesn't cover the build context or OCI feature contents; changes there need `rebuild --force`. → architecture/instances/rebuild-and-drift.md; evidence: `config::build_hash`, `sandbox_build_hash`.
- Feature cache entries never refresh (mutable tags like `:1` stay at their first pull until the cache dir is cleared). → architecture/instances/images-and-features.md; evidence: `features::fetch_into`.
- Feature ordering uses `installsAfter` only (no `dependsOn`, `overrideFeatureInstallOrder`); feature `capAdd`/`init` ignored; `_REMOTE_USER` falls back to `root` with no image-user introspection. → images-and-features.md; evidence: `features::install_order`, `FeatureMetadata`, `build_features_image`.
- Feature entrypoint chain races `run`: lifecycle commands start while feature entrypoints (e.g. dockerd) may still be starting. → images-and-features.md; evidence: `materialize` (no wait after `run_container`).
- `worktree-include` / `worktree-link` apply to the primary repo only, not `folders` worktrees. → worktrees.md; evidence: `materialize` (copy/link only on `folder`/`source`).
- A `folders` entry that is a repo subdirectory (no `.git` dir) is always bind-mounted directly under `auto`. → worktrees.md; evidence: `folders::is_repo_root`, `needs_worktree`.
- Worktrees devsandbox didn't create stay unlocked (only `gc.worktreePruneExpire=never` env covers them; an explicit `git worktree prune` in a container still drops them); the lock file itself is writable from the container. → worktrees.md#locking; evidence: `lock_worktree`, `gc_keeps_worktrees_env`.
- Extra `folders` worktrees aren't surfaced in `run --json`/`status --json`/the dashboard. → worktrees.md; evidence: `RunRecord` has no folders field, `snapshot.rs` doesn't read `Instance.folders`.

### config
- `runArgs`, `appPort`, `portsAttributes`, `workspaceMount`, `capAdd`, `securityOpt`, `overrideCommand`, `shutdownAction`, `hostRequirements`, `waitFor`, `secrets`, `userEnvProbe`, `updateRemoteUserUID`, `overrideFeatureInstallOrder`, `name`, `customizations.vscode.settings` parse but are ignored (warned at `run`). → architecture/config.md; evidence: `SandboxProperties::ignored`.
- No way to remove an inherited array entry under `extends`; a diamond duplicates entries. → config.md#extends-and-deep_merge; evidence: `config::deep_merge`.

### runtimes
- Apple `container`: no `--cache-from` (dropped with warning), no `--privileged` (skipped), no restart policy (`autostart = "runtime"` acts as `true`), no file binds (feature file mounts skipped, ssh-agent mount fallback off), CPU shows `-`. → architecture/runtimes.md; evidence: `AppleContainer::supports_*`, `apple::parse_stats`.
- On podman, `autostart = "runtime"` restarts at boot only with `podman-restart.service`, which devsandbox never installs. → runtimes.md; evidence: `autostart::restart_decision`.

## Status entries

- **Parked (instances)**: `worktree-include`/`worktree-link` for `folders` worktrees; surfacing extra worktrees in JSON/dashboard (from docs/folders-worktrees.md "Out of scope (parked)", still unimplemented).
- **Parked (instances)**: `.worktreeinclude.local`, `**`/`[...]` in `worktree-link`, read-only store mounts, an unlink/restore command, serving shared files via `devsbd` tmpfs (docs/worktreeinclude.md "Out of scope", none implemented).
- **Parked (config)**: `runArgs` pass-through / env-file / secrets (docs/config-toml-findings.md #7 "OPEN"; `runArgs` still in `ignored()`).
- **Parked (instances)**: locking worktrees devsandbox didn't make by faking their checkouts in the container (docs/worktree-locking.md "Limits", not done).

## Stale claims in old docs

- high-level-architecture.md (Drift): "devcontainer features aren't hashed" → local-folder features' contents are in `build_hash`; only OCI feature contents aren't. Evidence: `config::sandbox_build_hash`, `features::local_features_fingerprint`.
- high-level-architecture.md (Drift): "`autostart` and `dispatcher` are stripped before hashing" → also `inbox` and `forwardPorts`, and default-restating `folders` entries are normalized. Evidence: `config::sandbox_hash`, `normalize_folders`.
- high-level-architecture.md (devsandboxes.toml): "`forwardPorts`, `runArgs` parse but warn" → `forwardPorts` is implemented (daemon forwards); `runArgs` still warns. Evidence: `SandboxProperties::forward_ports`, `ignored()`.
- high-level-architecture.md (devsandboxes.toml): file named `devsandboxes.toml` only → `.yaml`/`.yml` accepted too; legacy `config.toml` read and renamed. Evidence: `config::CONFIG_FILES`, `migrate_legacy`.
- devsandboxes.md: "No daemon" goal, compose (`dockerComposeFile`) support, `cache-folder`, `devsandbox init` → a host daemon exists, compose was removed, `cache-folder` became `caches`. Evidence: `src/serve/`, `SandboxProperties` (no compose fields), `known_cache`.
- run-command.md §1/§2: "`devsandbox run <sandbox>` with an existing stopped container does `docker start` on it (reuse)" and "nameConfig rewritten on every run" → `run` refuses an existing name (`start` restarts); nameConfig is written by `run` only when extensions or `remoteUser` are set, and by every `vscode` launch. Evidence: `run::run`, `claim_name`, `materialize`, `vscode::launch`.
- run-command.md §2: "every sandbox instance container joins the project network" / "`devsandbox.base_folder` … `sleep infinity`" → it joins the global net only when it uses a global service, plus a per-instance net for isolated ones; the command is `sh -c '[ -x devsbd ] && devsbd boot & exec sleep infinity'`. Evidence: `services::ensure_services`, `run::container_command`.
- run-command.md §4: compose override files, `rm --services`; §5 "features out of scope" → compose gone, no `rm --services` (gc only), features implemented. Evidence: `main.rs` `Command::Gc`, `run::image`.
- rebuild.md: "`ensure_services` already recreates drifted service containers" → it only warns; recreation is `service rebuild`. Evidence: `services::ensure_service`.
- rebuild.md "Out of scope: `--force` rebuild without drift" → `rebuild --force` exists. Evidence: `rebuild::rebuild` (`force`).
- rebuild.md code landmarks (`src/commands/run.rs:…`, `src/tui/app.rs`, `src/tui/data.rs`) → files split/moved (`run/mod.rs`, `tui/app/actions.rs`, `snapshot.rs`).
- service-rebuild.md title and text say `service rebuild`; AGENTS.md module map says `services::{gc,ls,rebuild}` → CLI is `devsandbox gc`, `devsandbox service rebuild|ls` (module fn names are `services::*`). Not stale, just easy to misread.
- vscode-workspaces-and-persistent-state.md: "Extra folders are plain shared mounts: no worktree treatment, shared between concurrent instances" → `folders` entries get detached worktrees under `auto`/`always`. Evidence: `folders::needs_worktree`.
- vscode-workspaces-and-persistent-state.md: workspace file "rewritten on every reuse" and "`${sharedVolumes}`/`${instance}` substituted in cache sources"; "Apple `container` untested" for nested binds → there is no reuse (written on `run`/`rebuild`); cache sources are fixed paths, not substituted; mounts are explicitly sorted parents-first for every runtime. Evidence: `materialize`, `resolve_caches`, `sort_parents_first`.
- config-toml-findings.md #1: "`${configDir}` = directory holding `config.toml`" → `devsandboxes.toml`. #6 v1 limitations "OCI refs only — no local paths" → local-folder features supported. Evidence: `config::CONFIG_FILE`, `features::FeatureSource::Local`.
- config-toml-findings.md #2: shell history "orphaned dirs reaped by `gc`" → true; also ssh-agent links. (Accurate, noted for completeness.)
- devcontainer-features.md "flagged as ignored at `src/config.rs:67,103`" and similar line refs → stale line numbers; `features` is typed now.
- Code comments (adjacent, not docs; report only): `rebuild.rs` `recreate` says "`--env`s aren't recorded, so they are dropped" but `Instance.extra_env` is recorded and kept (`run::effective_extra_env`); `commands::container_drifted` doc lists "`run` reuse" as a call site, but `run` never reuses (it's `start`'s `warn_on_drift`); `SandboxProperties` lists `mounts` under "valid devcontainer properties, not implemented yet" though it's implemented; `state::State` doc says "No daemon".

## Decision candidates

- **Host git never runs repo-controlled code**; why: containers write `.git` and dispatchers can trigger host git; source: high-level-architecture.md Principles; still true: every non-test `git` spawn is `run::git::git_command` via `host_git`/`check_repo`.
- **Mount sources come only from config**; why: an agent driving the CLI must not mount the host; source: high-level-architecture.md Principles, devsandboxes.md Goals; still true: no CLI/control flag adds a mount (`RunExtras` is env + dispatcher only).
- **Two instances never share a working tree** (repeat instances and contested `folders` get worktrees, dispatcher children always); source: high-level-architecture.md Principles, folders-worktrees.md Rule; still true: `run::base_in_use`, `folders::needs_worktree`.
- **`run` always creates; `start` restarts** (diverges from `devcontainer up` reuse); why: deterministic names, no surprise reuse of a drifted container; source: run-command.md (superseded) / code comment in `run::run`; still true: `claim_name` refuses existing names.
- **Persistent `instance_id` separate from the display name**; why: renames must never move containers, mounts, worktrees, history; source: `state::Instance` docs, vscode-workspaces doc; still true: `unique_instance_id`, `rename_exact`.
- **One lenient drift rule shared by every site** (missing label = no drift); why: old containers mustn't flap; source: rebuild.md, service-rebuild.md; still true: `commands::drift_decision` used by `container_drifted` and `snapshot::listed_drifted`.
- **Hash file contents, not paths, for builds; strip host-side keys from `config_hash`**; source: service-rebuild.md step 1, high-level-architecture.md Drift; still true: `build_hash`, `sandbox_hash`.
- **Service rebuild recreates and rewires in place, never restarts sandboxes**; source: service-rebuild.md Design decisions; still true: `services::rebuild` + `Backend::wire_service_dns`.
- **Service refcounts are derived from state + runtime labels, never stored**; source: run-command.md §4; still true: `services::service_refs`, label filters in `gc`/`stop`/`rm`.
- **Record first; failures stay `failed`, only a cancelled `run` rolls back**; source: instance-phase.md (daemon lead's source); still true: `run::run` provisional insert, `progress::on_failure`.
- **New worktree branches start from the remote default, not the base checkout's HEAD**; why: another agent may be mid-work there; source: run-command.md §3 / `worktree_start_point` docs; still true: `worktree_start_point`.
- **`extends` deep-merges with arrays concatenating**; why: templates' mounts/extensions/services reach sandboxes without restating; source: config-toml-findings.md #4, high-level-architecture.md; still true: `config::deep_merge`.
- **Features fetched natively (curl + tar), no devcontainer CLI or registry crates; layout mirrors the official CLI**; source: devcontainer-features.md Design; still true: `features::download_and_extract`, `run::image`.
- **Feature entrypoints chained at build time, image ENTRYPOINT always kept**; source: entrypoint.md; still true: `entrypoint_dockerfile`, `entrypoint_wrapper`.
- **Capability methods on `Backend`, never runtime-name checks** (exceptions by name: `"container"` in `write_workspace_file`/`vscode::launch` for VS Code specifics, `"podman"` in `autostart::restart_decision` for a warning); source: runtimes.md; still true: `Backend::supports_*`.

## Uncovered

- devsandboxes.md goals/milestones/open questions, run-command.md §6 implementation steps, rebuild.md/service-rebuild.md/devcontainer-features.md/folders-worktrees.md step lists: plan history, no current content beyond what the docs now say.
- Compose support (run-command.md §4, config-toml-findings.md #3): removed from the code.
- rebuild.md step 3 and service-rebuild.md step 4 (dashboard `s`/`:rebuild` behavior): dashboard area; only the `s`-rebuilds-drifted rule is mentioned in rebuild-and-drift.md.
- entrypoint.md's comparison table vs the devcontainers CLI and the Apple image-inspect shape caveat ("checked against sources, not on a real Mac"): history/unverifiable here; the behavior itself is in images-and-features.md.
- ssh-agent relay details, `devsbd` install/boot, instance-phase.md's build-log pty details: helper/daemon areas; linked loosely.
- high-level-architecture.md Machines/Automations/JSON/dashboard sections: other areas.
- config-toml-findings.md original "notes below" sections: superseded design notes.
