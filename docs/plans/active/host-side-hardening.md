# Host-side hardening

Status: in progress.

Fixes for bugs the credential plans' review found in today's code (the
reviews are not in the repo; each bug is restated here with its
evidence). All share one cause: the host trusts files a container can
write. [ADR 0003](../../decisions/0003-host-git-never-runs-repo-controlled-code.md)
stops host git from *running* repo-controlled code; these stop it from being
*pointed* elsewhere, and stop host file copies and per-instance dirs from
leaking across instances.

Out of scope: the shared, container-writable base `.git` itself (fixed by
replacing worktrees with clones, [git-gateway](../explore/git-gateway.md)
steps 3-4); a race with a container rewriting files between a check and
host git's use of them (same fix; recorded as a limitation in step 1).
Not a bug today: shared-namespace flags (`--pid container:…`,
`--volumes-from`) are refused only under a worker policy, but `runArgs` is
parsed and ignored (`config::SandboxProperties::run_args` has no reader),
so no local config can produce them.

## Step 1: host git refuses redirected git dirs

Bugs: a planted `gitdir:` file in a checkout points host git (and
`git_companion_mount`) at any host repo; `commondir` and symlinked
`refs/`, `objects/`, `index`, … in a container-writable git dir redirect
host git's reads and writes; fleet code transfer (`serve::fleet::code`,
all through `host_git`) inherits both.

In `src/commands/run/git.rs` (`git_dirs`, `check_dirs`, `check_repo`,
`host_git_bare`):

- `<repo>/.git` must not be a symlink (`symlink_metadata`, not
  `metadata`).
- A `gitdir:` file is accepted only when it points at
  `<common>/worktrees/<name>` whose `gitdir` back-pointer file names this
  checkout's `.git` (canonicalized both ways), the way git links a
  worktree. Anything else is refused, naming the file.
- A `commondir` file is accepted only in such a `worktrees/<name>` admin
  dir, resolving to the dir two levels up; in a main git dir it is
  refused.
- No symlink as a direct entry of the git dir or the common dir, nor
  anywhere under `refs/`, nor in `objects/`, `objects/info/`,
  `objects/pack/` (loose objects aren't walked: cost, and a redirected
  loose object only fails to inflate).
- Refusal messages follow the existing ones ("a sandbox may have written
  it — review and remove it").
- Tests (temp repos, like `git_dirs_for_checkout_and_linked_worktree`):
  a real linked worktree passes; a planted `gitdir:` to another repo, a
  forged back-pointer, a `commondir` in a main git dir, a symlinked
  `refs/heads`, `objects`, `index` and `.git` each refuse.
- Docs: the module doc of `git.rs`, `docs/architecture/instances/worktrees.md`
  (what host git refuses), `docs/status.md` limitation: a container
  running concurrently can still swap files between the check and git's
  use (until clones).

## Step 2: host fetches only a trusted origin URL

Bug (critical): a sandbox rewrites `remote.origin.url` in the
bind-mounted checkout; the next repeat `run` (a dispatcher can trigger
one) or `rm` makes host git fetch that URL (another private repo, a host
path, `file://`) with the user's helpers and keys into `refs/remotes/origin/*`,
which the sandbox reads.

- `State` gains `trusted_origins`: canonical common dir → origin URL
  (`src/state.rs`). Trust on first use: the first host fetch of a repo
  pins the URL its config holds.
- One function does every host fetch of origin: `fetch_origin`,
  `fetch_origin_branch` (`src/commands/run/worktree.rs`) and `fetch_prune`
  (`src/commands/rm.rs`). It reads `remote.origin.url` from the parsed
  config (`git.rs`'s parser, never `git config`), compares it with the pin,
  and fetches the *pinned URL* explicitly with devsandbox's own refspec
  (`+refs/heads/*:refs/remotes/origin/*`, `--prune` where today's call
  prunes), so a rewrite between check and fetch changes nothing.
- A mismatch skips the fetch with a warning naming both URLs (the
  callers already treat a failed fetch as "use the last-fetched refs")
  and how to trust the new one: `devsandbox trust-origin <folder>`, a new
  local subcommand (`src/commands/trust_origin.rs`) that re-pins to the
  URL the config holds now, after showing it. Local-only, like `rename`.
- Tests: pin on first fetch; equal URL fetches the pinned URL from a
  local bare repo; a rewritten URL skips and leaves refs untouched;
  `trust-origin` re-pins.
- Docs: `worktrees.md` (start point fetch), `lifecycle.md` (`rm`'s fetch),
  `docs/reference` if commands are listed there, the AGENTS.md module map
  line for `commands/*.rs`.

## Step 3: host checkouts never run filter drivers

Bug: a sandbox-written `.gitattributes` (`filter=lfs`) plus `.lfsconfig`
`lfs.url` makes host `git worktree add` run git-lfs with the user's
credential helpers (out-of-fence objects, requests to any URL); any
filter driver in the user's global or system config can be triggered the
same way. The repo's own `filter.*` keys are already refused (`refusal`).

- `safe_env` sets `GIT_LFS_SKIP_SMUDGE=1` on every host git.
- Every host git also gets, for each filter driver name found in the
  user's global and system config (`git config --global|--system
  --name-only --get-regexp '^filter\.'`, read once per process), overrides
  that make it inert: `-c filter.<name>.smudge=cat -c
  filter.<name>.clean=cat -c filter.<name>.process= -c
  filter.<name>.required=false`. Verify against real git that an empty
  `process` disables the long-running filter; if not, use what does.
- Tests: a global filter (via `GIT_CONFIG_GLOBAL` pointing at a temp
  file) whose smudge writes a marker is not run by a host `worktree add` /
  `reset --hard`; LFS pointer files stay pointers.
- Docs: `worktrees.md`; `docs/status.md` limitation: LFS files in a
  host-made checkout are pointers until `git lfs pull` in the sandbox.

## Step 4: per-instance bind dirs go with the instance

Bug: bind sources made per instance (a config or feature mount whose
source contains `${instance}` / `${devcontainerId}`, e.g. the zidane
feature's `${sharedVolumes}/zidane/<instance>`) outlive `rm`, and
`unique_instance_id` gives the same id to the next `run` of that name, which
inherits the old dir (live: real tokens).

- Like per-instance volumes (`Instance::volumes`, `merge_volumes`): a bind
  source whose substituted path depends on the instance and which
  devsandbox *created* (`ensure_bind_source`, or a
  `creatable_feature_source`) is recorded on the instance
  (`Instance::bind_dirs`, accumulated across rebuilds) and deleted by
  `rm`, after the container is gone. Only paths inside the config dir are
  ever deleted; a path that already existed is never recorded.
- Shell history stays as it is (kept on `rm`, reaped by `gc`).
- Tests: the record at `run`, deletion at `rm`, a pre-existing dir kept, a
  path outside the config dir refused.
- Docs: `lifecycle.md` (`rm` removes them), `mounts-and-state.md`.

## Step 5: worktree-include copies never follow symlinks

Bug: `copy_worktree_includes` / `copy_entry` keep leaf symlinks, but
`create_dir_all(parent)` and `fs::copy` follow a parent dir swapped for a
symlink after `worktree_include_paths` listed the files, on the source
(the container-writable base checkout: a host file copied into the
worktree) and the destination (a host file written).

- On unix, walk both paths component by component from their roots with
  `openat(…, O_NOFOLLOW | O_DIRECTORY)` (`libc`, already a dependency),
  `mkdirat` for missing destination dirs, the source file opened
  `O_NOFOLLOW`, the destination created `O_CREAT | O_EXCL | O_NOFOLLOW`.
  A symlink anywhere on either path skips that file with a warning (the
  copy is best effort today). Off unix unchanged.
- Tests: a symlinked parent dir on either side is skipped and nothing is
  read or written through it; ordinary nested files still copy.
- Docs: `worktrees.md` (*Copy*).
