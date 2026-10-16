# Worktrees

How devsandbox keeps two instances from ever sharing a working tree: git
worktrees for repeat instances and `folders` entries, the files seeded into
them, and the locks that keep containers from pruning them. Read before
touching worktree creation, branches, `.worktreeinclude`/`worktree-link` or
`rm`'s cleanup.

Code: `src/commands/run/worktree.rs` (`create_worktree`,
`create_new_branch_worktree`, `create_detached_worktree`,
`worktree_start_point`, `fetch_origin`, `git_companion_mount`,
`lock_worktree`, `unlock_worktree`, `copy_worktree_includes`,
`link_shared_files`), `src/commands/run/folders.rs` (`mount_folders`,
`needs_worktree`, `folder_owned`, `folder_directly_mounted`,
`merge_folder_mounts`), `src/commands/run/mod.rs` (`base_in_use`,
`prepare_worktree`, `worktree_branch`, `link_store`, `gc_keeps_worktrees_env`),
`src/commands/rm.rs` (`branch_plan`, `branch_action`, `remove_or_prune`).
Config fields: [config-toml-spec](../../../skills/config-toml-spec/SKILL.md).

All host git runs through `host_git`, which refuses repos whose config could
run code: see [mounts and state](mounts-and-state.md#host-git).

## When an instance gets a worktree

`base_in_use`: the sandbox's base folder is already mounted directly by
another instance (its `folder`, or a `folders` entry without a worktree), any
sandbox, any config root. The first instance mounts the base checkout; the
next ones get `<config dir>/.worktrees/<instance_id>/`. A dispatcher's child
always gets a worktree: an unattended agent never works in the user's
checkout.

The worktree's `.git` file points at `<base>/.git/worktrees/<id>` by absolute
host path, so the base repo's whole `.git` is bind-mounted read-write at the
identical path (`git_companion_mount`); commits land in the shared object
store.

## Branches

`worktree_branch`: `run --branch`, else `worktree-branch`, else
`sandbox/${instance}`, `${…}` substituted. A dispatcher's `--branch` is
validated and used verbatim.

`create_worktree` picks the source:

- local `refs/heads/<branch>` exists → checked out as is (refused with the
  owning path/instance if it's checked out elsewhere);
- only `origin/<branch>` → a local branch tracking it (an explicit branch
  name triggers a one-branch fetch first);
- neither → new branch, `--no-track`, from the start point.

Start point (`worktree_start_point`): `run --base` / `worktree-base` →
`origin/HEAD` → `origin/main` → `origin/master` → `HEAD`; never the base
checkout's branch, which another agent may be using. `git fetch origin` runs
first unless `FETCH_HEAD` is under 2 minutes old; failure only warns. No
`origin` → the explicit ref if any, else `HEAD`, with no fetch. An unborn start or empty result is an error.

`Instance.branch_created` records whether `run` made the branch; `rm` only
considers deleting branches it made (see [lifecycle](lifecycle.md#rm)).

## folders worktrees

Each `folders` entry (extra VS Code roots) has a `worktree` mode:

- `never`: bind the host dir.
- `always`: detached worktree (must be a repo root).
- `auto` (default): worktree only if the folder is a repo root (`.git`
  directory) and either the instance is a dispatcher child, the folder is
  *owned* (some sandbox of this config has it as `folder`, or any instance
  in state has it as `base_folder`), or another instance mounts it directly.
  Otherwise this instance takes the live dir.

Folder worktrees are detached at the base's `HEAD` (no branch: nothing to
delete, no "already checked out" clash), at
`.worktrees/<instance_id>.folders/<basename>-<hash8>`, each with its own
companion `.git` mount. `Instance.folders` records every entry as mounted. A
rebuild reuses a prior record that fits the current mode (recreating a
worktree that vanished); records whose entry was dropped are kept so `rm`
still cleans them.

## .worktreeinclude and worktree-link

Both run in `materialize` after `initializeCommand` and before the container,
on `run` and every `rebuild`, for the primary repo only.

- **Copy** (`copy_worktree_includes`, worktree instances): gitignored,
  untracked files matching the base repo's `.worktreeinclude` plus the
  sandbox's `worktree-include` patterns (applied after, so they can negate)
  are copied from the base into the worktree. Git does the matching
  (`ls-files --others --ignored --exclude-from`, then `check-ignore`).
  Never overwrites. Best effort: problems warn.
- **Link** (`link_shared_files`, every instance): each `worktree-link` entry
  (repo-relative, gitignored; `*`/`?` per segment, no `**`, no `..`/`.git`)
  lives once in `<config dir>/shared-files/<basename>-<hash8>/` (one store
  per base repo, shared by every sandbox on it). The first sighting moves the
  base's real file into the store; the base and each worktree get an absolute
  symlink; the store is mounted at its host path so links resolve in the
  container (a directory mount, so it works on Apple `container`). Links run
  before copies, so a path in both ends up linked. Only an invalid entry fails
  `run`; everything else warns.

`rm` and a rolled-back `run` leave the store alone.

## Locking

A container sees the base's whole `.git` but none of the other checkouts, so
from inside every other worktree looks missing and `git worktree prune` (or
`git gc`) would delete their records.

- `lock_worktree`: `git worktree lock --reason …` on every devsandbox
  worktree, at creation and again on every `run`/`start`/`rebuild` (no-op
  once locked; best effort). A missing admin dir is reported, not repaired.
- `gc_keeps_worktrees_env`: containers get `gc.worktreePruneExpire=never`
  via `GIT_CONFIG_COUNT` env, covering worktrees devsandbox didn't make
  against `gc`.
- `unlock_worktree` runs before removing or re-adding one.

## Removal

`rm` removes `folders` worktrees, then the primary worktree
(`remove_or_prune`: already-gone ones are pruned so a retried `rm` gets
past them), then the branch it decided on. Without `--force` a dirty worktree
is refused before anything is torn down. Copied and linked files are
gitignored, so they never make a worktree dirty.

Limitations: [status.md](../../status.md#instances)
