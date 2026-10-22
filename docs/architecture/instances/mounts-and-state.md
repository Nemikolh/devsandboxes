# Mounts and host state

What gets mounted into an instance container, where devsandbox keeps
host-side data, the VS Code integration files, and the host git wrapper that
guards every git call on a sandbox's repo. Read before adding a mount,
touching host paths, or running git on the host.

Code: `src/commands/run/mounts.rs` (`resolve_mounts`, `last_wins_by_target`,
`sort_parents_first`, `resolve_folders`, `resolve_caches`,
`provision_shell_history`, `resolve_shell_rc`, `shell_rc_wiring`,
`write_workspace_file`, `feature_mount_decision`, `merge_volumes`),
`src/commands/run/git.rs` (`host_git`, `check_repo`),
`src/commands/run/editor.rs` (`write_vscode_name_config`),
`src/commands/vscode.rs` (`launch`), `src/commands/run/ssh_agent.rs`,
`src/commands/run/mod.rs` (`materialize`, `run_container`), `src/config.rs`
(`Mount`, `MountContext`, `known_cache`).

## Config is the mount boundary

Every host path a container gets comes from the config layer or from
devsandbox itself: the workspace, `folders`, `mounts`, `caches`,
`persist-shell-history`, `shell-rc`, feature mounts, and devsandbox's own
companions. No CLI flag or control request adds a mount; a dispatcher's
child is a sandbox of the same config (`dispatcher.spawn`) with only extra
env. An agent driving the CLI gets run/start/exec/stop, not arbitrary host
paths.

## What run mounts

In `materialize` / `run_container`:

- **Workspace**: the instance's `folder` (base checkout or worktree) at
  `workspaceFolder` (default `/workspaces/<basename>`), also the workdir.
- **Git companion** (worktree instances, and each `folders` worktree's base):
  `<base>/.git` read-write at the identical path
  ([worktrees](worktrees.md)).
- **`worktree-link` store**: `<config dir>/shared-files/<repo>-<hash8>/` at
  its host path.
- **`mounts`**: devcontainer shorthand or object, `${…}` substituted
  ([config](../config.md#variables)), collapsed to the last entry per target
  (`last_wins_by_target`, so a sandbox overrides an inherited mount). Missing
  bind sources are created: a basename with a dot after any leading dots is a
  file, else a directory. A volume named with `${instance}` /
  `${devcontainerId}` is per-instance: recorded in `Instance.volumes`
  (accumulated across rebuilds) and deleted by `rm`.
- **`folders`**: each entry (or its detached worktree) at its container path.
- **`caches`**: `<config dir>/shared-volumes/<dir>` bind-mounted at
  `/var/cache/devsandbox/<dir>` plus the tool's env var(s) pointing there
  (`known_cache`: pnpm → `pnpm-store`, cargo, npm, yarn, go → `go-mod`,
  pip), shared by every instance of the config root. No home directory is
  involved, so the same target serves any `remoteUser`/`containerUser`; the
  host-created source is as writable in the container as the workspace.
- **`persist-shell-history`**: `shared-volumes/history/<instance_id>/` at
  `/commandhistory`, `HISTFILE` set in env and pinned in `~/.zshrc` /
  `~/.bashrc` (VS Code's shell integration resets the env value). A
  directory, since Apple `container` can't bind files. Kept on `rm`; `gc`
  reaps orphans.
- **`shell-rc`**: each file's parent dir read-only at `/devsandbox/rc/<i>`,
  sourced from the rc files by a guarded line added once.
- **Feature mounts** last: a target already claimed wins
  ([images and features](images-and-features.md)).

`sort_parents_first` orders all `--mount`s by target depth so nested mounts
stay visible on every runtime, not just docker.

Who owns what lands in these binds is the identity plan's
([identity](identity.md)): it relabels binds under SELinux, and
`devsbd identity apply` hands empty root-owned named-volume mount points
(feature volumes included) to the adopted account and adds it to the
docker socket's group when a bind brings the socket in.

## ssh-agent

Relay first: with an embedded `devsbd` helper (unix), nothing is mounted;
the helper serves the agent socket over `exec` ([helper](../helper/)).
Builds without the helper fall back to bind-mounting the host agent through
a re-pointable symlink `<data-dir>/devsandbox/agent/<instance_id>.sock`
(refreshed on `start`), only on runtimes with file binds. `rm` deletes the
link; `gc` sweeps leftovers.

## Host paths

| path | what |
|---|---|
| `<data-dir>/devsandbox/state.toml` (+ `state.lock`) | every instance, all config roots ([lifecycle](lifecycle.md#statetoml)); `$XDG_DATA_HOME`, else `~/.local/share` |
| `<data-dir>/devsandbox/logs/<instance_id>/build.log` | per-instance build log |
| `<data-dir>/devsandbox/agent/` | ssh-agent links (mount fallback) |
| `<cache>/devsandbox/features/` | feature cache and build contexts |
| `<config dir>/.worktrees/<instance_id>/`, `<instance_id>.folders/` | worktrees |
| `<config dir>/shared-volumes/` | caches, shell history, `${sharedVolumes}` |
| `<config dir>/shared-files/` | `worktree-link` stores |

Runtime objects are namespaced by `project_id` (services, networks) or the
globally unique `instance_id` (containers); see [services](services.md).

## VS Code

- `write_workspace_file`: `/workspaces/<instance>.code-workspace` inside the
  container, written on `run`/`rebuild` (best effort). The window title is
  the file name, so the window is named after the instance. Roots: the
  workspace (named by its basename, else the instance) then `folders`;
  `terminal.integrated.cwd` pinned to the primary root.
- `write_vscode_name_config`: the Remote-Containers `nameConfigs/<container>.json`
  for each installed product (`Code`, `Cursor`, `VSCodium`) with
  `customizations.vscode.extensions` and `remoteUser` (the instance's
  recorded exec user, `Instance::remote_user`: [identity](identity.md));
  written by `run` when either is set and by every `vscode` launch.
- `vscode::launch`: `code --file-uri` (or `--folder-uri` without a workspace
  file) on `vscode-remote://attached-container+<hex(name)>/…`; on Apple
  `container`, `apple-container+<hex({id, image})>`, which needs VS Code's
  `dev.containers.experimentalAppleContainerSupport`. nameConfigs extension
  install is Docker-only, so on Apple `container` the extensions go into the
  workspace file as recommendations instead.

## Host git

Containers write the repo's `.git` (worktree instances bind-mount the base's
whole `.git`), so a planted hook or config key would run as the host user on
the next host git call, which dispatchers can trigger from inside. Every host
git call on a sandbox's repo goes through `run::git::host_git`:

- `-c core.hooksPath=/dev/null -c core.fsmonitor=false`, stdin closed,
  `GIT_TERMINAL_PROMPT=0`, `GIT_DIR`/`GIT_COMMON_DIR`/`GIT_WORK_TREE`
  removed.
- `check_repo` first: the repo's local config files are parsed as files
  (never `git config`, which follows includes) and every key checked against
  an allowlist of keys that can't run commands; `ext::` URLs (remote, push,
  `insteadOf`, submodule) refuse too, as does any
  `objects/info/alternates`. A refusal names the file and key for the user
  to review; nothing is auto-fixed.

`rm` runs `check_repo` before tearing anything down so it never stops
half-way.

Limitations: [status.md](../../status.md#instances)
