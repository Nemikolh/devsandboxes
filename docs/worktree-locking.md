# Worktree locking

Code: `lock_worktree` / `unlock_worktree` in `src/commands/run/worktree.rs`,
`gc_keeps_worktrees_env` in `src/commands/run/mod.rs`.

## Problem

A worktree instance's container gets the base repo's whole `.git` (the git
companion mount), read-write, at its host path, but only its own checkout.
Every other worktree of that repo (other instances', or the user's own) is
recorded under `.git/worktrees/<name>/` with a host path that doesn't exist in
the container. From inside, `git worktree prune` sees all of them as stale and
deletes their admin dirs; each of those checkouts then fails with
`fatal: not a git repository` (working-tree files survive, HEAD/index don't).
`git gc` does the same for records older than `gc.worktreePruneExpire`.

## What devsandbox does

- **Locks its worktrees** (`git worktree lock --reason …`). Prune and gc never
  touch a locked record. Done on creation (primary and `folders` worktrees),
  in `materialize` before the container is created (`run`, `rebuild`), and on
  every `start`: instances made before locking get locked lazily. Already
  locked: no git call. A worktree whose record is already gone gets a warning,
  not a failure.
- **Unlocks before removing**: `rm` (`remove_or_prune`) and the recreation of
  a missing `folders` worktree. `remove` refuses a locked worktree short of a
  double `--force`, `prune` skips it, `add` won't reuse its path.
- **`gc.worktreePruneExpire=never` in the container** via
  `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>`, appended after
  any count `containerEnv`/`run --env` set. Covers worktrees devsandbox didn't
  make, against gc only. Applied at container creation, so existing containers
  get it on rebuild. A `remoteEnv` `GIT_CONFIG_COUNT` overrides it for exec'd
  processes.

## Limits

- Worktrees not made by devsandbox stay unlocked: an explicit `git worktree
  prune` in a container still removes their records. Faking their checkouts
  in the container (stub `.git` files at the recorded paths, kept in sync by
  `devsbd`) would cover them; not done.
- The lock is a guard against accidents: anything in the container can delete
  the `locked` file.
