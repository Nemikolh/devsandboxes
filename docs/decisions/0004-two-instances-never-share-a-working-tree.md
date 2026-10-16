# 0004: Two instances never share a working tree

Status: accepted

## Context

One sandbox config can spawn many instances (several agents on one repo,
dispatcher children). If two containers edit the same checkout they trample
each other's files, index and branch. Copying the repo per instance is slow
and loses the shared object store.

## Decision

- A `run` against a folder already in use by another instance
  (`run::base_in_use`) gets its own git worktree under `.worktrees/`, on a
  fresh `sandbox/<instance>` branch by default. Dispatcher children always
  get one.
- Extra `folders` entries that are repo roots get detached worktrees under
  the same rule when contested (`run::folders::needs_worktree`, modes
  `auto`/`always`).
- A new worktree branch starts from the remote default branch, not from the
  base checkout's HEAD (`worktree::worktree_start_point`): another agent
  may be mid-work there.
- devsandbox locks the worktrees it creates so in-container `git worktree
  prune` can't drop them.

## Consequences

- Instances are cheap and disposable: `rm` removes the worktree (and, on
  request, its branch) without touching anyone else's work.
- Worktrees bind-mount the base repo's `.git`, which is why host git is
  hardened (0003).
- Rejected: shared bind mount of one checkout; full clones per instance.

Source: docs/high-level-architecture.md, "Principles" ("Instances are cheap and disposable"); docs/folders-worktrees.md; docs/run-command.md §3; docs/worktree-locking.md.
