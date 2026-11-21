# 0057: Host git never follows what a sandbox can write

Status: accepted (2026-11-20). Extends
[0003](0003-host-git-never-runs-repo-controlled-code.md).

## Context

0003 stops host git from *running* repo-controlled code. A container can
still *point* host git elsewhere through the files it can write in a
bind-mounted checkout: a `gitdir:` or `commondir` naming another host
repo, a symlinked `refs/`, `objects/`, `index` or reflog, a rewritten
`remote.origin.url` fetched with the user's credentials into refs the
sandbox reads, a `.gitattributes` triggering the user's global filter
drivers (git-lfs with its credential helpers). Host-side copies out of
the checkout (worktree-include files) and per-instance dirs that outlive
the instance leak the same way.

## Decision

- **Git dirs must be what git made** (`run::git::check_repo`): no `.git`
  symlink, a `gitdir:` file only for git's own linked worktree (its
  back-pointer names this checkout), `commondir` only in such an admin
  dir, no symlink where git reads or writes.
- **Origin is fetched by value from a URL trusted on first use**
  (`run::origin::fetch_trusted_origin`, `State::trusted_origins`); a
  different URL in the config is not fetched until the user runs
  `devsandbox trust-origin`.
- **Host git makes every user filter driver inert** and sets
  `GIT_LFS_SKIP_SMUDGE=1` (`run::git::neutral_filter_args`).
- **Host copies out of a checkout never follow a symlinked dir**
  (`run::worktree::copy_include_paths`), and **per-instance dirs
  devsandbox created go with the instance** (`Instance::bind_dirs`,
  deleted by `rm`).

## Consequences

- A repo layout git didn't make (`--separate-git-dir`, symlinked
  `objects/`) is refused, as unknown config keys are under 0003.
- LFS files in host-made checkouts are pointers; the sandbox runs
  `git lfs pull`.
- A remote the user changes needs one `trust-origin`.
- Checks race a container rewriting files between check and use; only
  removing the shared, container-writable git dir closes that (clones,
  `docs/plans/active/git-gateway.md`).
- Rejected: host git with global and system config off (loses the
  credential helpers and `insteadOf` legitimate fetches need), refusing
  every fetch while a sandbox is running.
