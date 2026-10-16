# Instance lifecycle

How an instance comes to exist (`run` → `materialize`), what its record holds,
and what `start`, `stop`, `rename` and `rm` do to it. Read before touching any
instance verb or `state.toml`.

Code: `src/commands/run/mod.rs` (`run`, `claim_name`, `prepare_worktree`,
`materialize`, `run_container`, `container_command`, `default_instance_name`,
`unique_instance_id`, `orphan_check`, `RunExtras`, `RunRecord`),
`src/commands/run/lifecycle.rs`, `src/commands/run/progress.rs` (`Tracker`,
`on_failure`, `rollback`), `src/commands/run/cancel.rs`,
`src/commands/{start,stop,rm,rename,done}.rs`, `src/commands/mod.rs`
(`resolve_instance`, `not_ready`, `in_flight`), `src/state.rs` (`State`,
`Instance`, `Progress`, `Phase`).

## Names and ids

- **Instance name** (the `state.toml` key, what every `<name>` argument
  takes): `--name`, else the sandbox name if free, else the first free
  `<sandbox>-<n>` (n ≥ 2).
- **`instance_id`**: the name at creation, kept forever (`rename` moves only
  the key). If a rename left the id taken, the new one gets `<name>-<i>`
  (`unique_instance_id`). The container (`devsandbox-<instance_id>`),
  `${instance}`, the worktree dir, isolated services and networks, shell
  history and the build log all key off the id, so a rename moves nothing.
- `resolve_instance`: an exact instance name or id wins; otherwise every
  instance whose sandbox name or base-folder basename matches. Several
  matches prompt on a TTY and fail off one. `rename` takes the exact key only.

## state.toml

`<data-dir>/devsandbox/state.toml` (`$XDG_DATA_HOME`, else
`~/.local/share`), one file for every config root. The runtime is the truth
for liveness; the file maps names to what devsandbox made. `State::load`
reads unlocked (writes are atomic); every change goes through
`State::update`, a locked (`state.lock`) read-modify-write that applies only
the caller's change, so a long `run` never drops what another command
recorded meanwhile.

`Instance` holds: `sandbox`, `instance_id`, `project` (config-root id),
`config_dir`, `container`, `folder` (mounted tree) / `base_folder` /
`worktree` / `branch` / `branch_created`, `folders` (extra roots as
mounted), `workspace`, `workspace_file`, `remote_env`, `remote_user`,
`extra_env` (`run --env`, kept across rebuild), `volumes` (per-instance
volumes `rm` deletes), `shell_history`, `ssh_auth_sock`, `devsbd_arch`,
`dispatcher` (owning dispatcher's id), `forwarded_ports`, `done`,
`progress`, `created_unix`. `State` also holds `autostart_boot` and
`global_forwarded_ports` per project id.

## run

`run` always creates a new instance; an existing name is refused (`start`
restarts, `rm` frees it). Steps:

1. Load config (no sandboxes → offer an example config), pick the sandbox
   (prompt on a TTY), `resolve_sandbox`, warn on `ignored()` properties.
2. Name and id. A container with that name but no state entry is refused,
   except a dispatcher's orphaned child with no worktree left
   (`orphan_check`), which is removed.
3. Worktree decision: a worktree when the base folder is already mounted
   directly by another instance, or always for a dispatcher's child (see
   [worktrees](worktrees.md)).
4. **Record first**: a provisional `Instance` with `progress` (phase
   `preparing`) is inserted under the lock (`claim_name` re-checks the name
   and id). From here a failure leaves a record `rm` can find.
5. `prepare_worktree`, then `materialize`.

## materialize

Shared by `run` and `rebuild`; replaces the record. Fields a previous record
had (dispatcher, `config_dir`, `extra_env`, `forwarded_ports`, `done`,
`folders` worktrees, `volumes`) carry over.

1. `initializeCommand` on the host (in the config dir).
2. `worktree-link` store and links, `.worktreeinclude` copies, `folders`
   worktrees, worktree locks ([worktrees](worktrees.md)).
3. `ensure_services` ([services](services.md)).
4. Mounts, caches, shell history, `shell-rc`, feature options, privileged and
   restart decisions ([mounts and state](mounts-and-state.md),
   [images and features](images-and-features.md)).
5. Phase `building`: `image_for`. Phase `creating`: `run_container`
   (`run -d`, labels, `--mount`s, env, first network; command
   `sh -c '[ -x /run/devsandbox/bin/devsbd ] && /run/devsandbox/bin/devsbd boot & exec sleep infinity'`,
   `container_command`), then
   `connect_networks`, `wire_service_dns`, install the `devsbd` helper,
   write the `.code-workspace` file.
6. Save the record (still in flight), write the VS Code name config, sync the
   helper's boot file.
7. Lifecycle (phase = step name): `shell-rc` wiring if any, then
   `onCreateCommand`, `updateContentCommand`, `postCreateCommand`,
   `postStartCommand`, `postAttachCommand`, each via `exec` with the
   workspace as workdir, `remoteUser`, `exec_env` (`remoteEnv` + `extra_env`)
   and the ssh-agent socket. Then `progress` is cleared.

## Phases and failure

`Progress { phase, pid, boot, error }`; phases `preparing`, `building`,
`creating`, a lifecycle step, `failed`. A reader derives "interrupted" when
the pid is dead or the boot id changed, so nobody rewrites records.

- Any failure keeps the instance as `failed` with its build log
  (`devsandbox rebuild` retries, `rm` removes).
- Only a cancelled `run` (SIGINT/SIGTERM, `run::cancel`) rolls back:
  `rollback` removes what the record says was made, and the branch only if
  this run created it and it has no new commits. A cancelled `rebuild` is
  recorded `failed`. A second signal kills as usual.
- `not_ready` / `in_flight` gate other verbs on an in-flight or failed
  instance; `exec` only waits for the container to exist.

## start and stop

- `start <name>|--all`: refreshes the ssh-agent link and worktree locks. If
  the config still resolves for this root: drift warning
  ([rebuild and drift](rebuild-and-drift.md)), `start` the container, apply
  the restart policy, ensure the helper, `ensure_services` +
  `wire_service_dns`, sync the boot file, then exec `postStartCommand` itself
  unless the in-container boot hook already runs it (`autostart = "runtime"`
  on a runtime with restart policies; a container predating the hook warns
  to `rebuild --force`). Otherwise it bare-starts the
  container and its labelled isolated services. A missing container is an
  error (its message points at `devsandbox run`).
- `stop <name>|--all`: stops the container, then its isolated services.
  Global services keep running.

## rm

`rm <name> [--delete-branch|--keep-branch] [--force]`:

1. Refuse an in-flight instance (even forced). Without `--force`, refuse
   up front if the base repo fails `check_repo` or a worktree is dirty, so
   nothing is half torn down.
2. Decide the branch (`branch_plan`: the worktree's current branch, never
   the remote's default; plus the recorded one if it is an ancestor). With no
   flag: `git fetch --prune`, delete when in sync with or gone from the
   remote, else ask (off a TTY: keep; `--fail-on-ask` exits 3 for the
   daemon's confirm round).
3. Remove the container, isolated services and instance network
   (`remove_container`); per-instance volumes; the ssh-agent link; `folders`
   worktrees; the worktree; the branch if decided.
4. Drop the record by `instance_id`, delete the build log, archive the
   instance's Inbox threads.

Kept: the shell-history dir (a later instance with the same id inherits it;
`gc` reaps orphans) and the shared `worktree-link` store.

## rename and done

`rename` moves the state key only (exact key, no fuzzy resolution).
`done` marks an instance finished (`done` timestamp, kept by `rebuild` and
`start`); only display and dispatcher policy read it.

Verbs may run inside the host daemon as `--in-process` subprocesses; see
[daemon](../daemon/).

Limitations: [status.md](../../status.md#instances)
