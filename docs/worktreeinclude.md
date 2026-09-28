# Gitignored files in worktrees: `.worktreeinclude`, `worktree-include`, `worktree-link`

## Goal

A repeat instance of a repo gets a fresh git worktree
(`<config>/.worktrees/<id>`), and a fresh checkout contains no untracked files.
Gitignored local files a project needs to run (`.env`, `.env.local`, local
certs, `.vscode/tasks.json`, `.mise.local.toml`, …) are missing, so every new
instance has to be set up by hand.

These files come in two kinds, handled by two mechanisms:

| Kind | Examples | Mechanism |
|---|---|---|
| Seed: copied once, then owned by each tree | `.vscode/tasks.json`, `.mise.local.toml` | **copy**: `.worktreeinclude` + `worktree-include` |
| Shared: one copy, edits seen by every instance | `.env`, `.env.local` | **link**: `worktree-link` |

```toml
[template.base]
worktree-include = [".vscode/tasks.json"]   # copied into each new worktree
worktree-link    = [".env", ".env.local"]   # shared live by every instance
```

Both keys are arrays, so they concatenate under `extends`. With no
`.worktreeinclude` and neither key set, nothing happens, exactly as before.

## Why this works on every runtime

Both mechanisms work on the host, inside directories that are already mounted,
before the container is created. Nothing is bind-mounted as a single file,
which Apple `container` can't do (`Backend::supports_file_binds()`). Copies land
in the worktree directory. Links point into a store directory that is mounted
at its own host path. So both work on docker, podman and Apple `container`
alike.

## Copy: `.worktreeinclude` and `worktree-include`

`devsandbox run` honors a `.worktreeinclude` file at the root of the base repo,
the same convention used by Claude Code (`--worktree`), Conductor, the Codex
app, worktrunk and `git-worktreeinclude`. A repo already set up for those tools
works unchanged. `worktree-include` adds patterns from the config, for repos
you don't control.

Rules (shared with the other tools):

- `.gitignore` syntax: comments, `**`, root anchoring with a leading `/`,
  directory patterns with a trailing `/`, and `!` negation. `worktree-include`
  patterns are applied **after** the file, so they can negate its patterns.
- A path is copied only if it **matches a pattern and git reports it as
  ignored**. Tracked files are never copied because the checkout already has
  them. Untracked files that aren't ignored aren't copied either.
- Files are **copied, not symlinked**, at the same relative path. Each worktree
  then owns its copy and can change it without affecting the others.
- **Existing files are never overwritten.**
- The file is read from the **base repo** (where the ignored files live), not
  from the new worktree's branch.
- Copies are made **once, when the worktree is created**. `rebuild` keeps the
  worktree and doesn't copy again, so a file deleted on purpose stays deleted.

Mechanism. Git does all the matching, so there is no gitignore-parser
dependency:

1. `git -C <base> ls-files -z --others --ignored
   --exclude-from=<base>/.worktreeinclude --exclude=<pattern>…` lists
   untracked files that match the include patterns. Only these patterns are
   passed as exclude sources, so `.gitignore` plays no part in this step and
   negation works as expected. Command-line `--exclude` outranks
   `--exclude-from`, which is what lets config patterns override the file.
2. That list is piped into `git -C <base> check-ignore -z --stdin`, which keeps
   only paths git ignores (`.gitignore`, `.git/info/exclude`,
   `core.excludesFile`). That includes files under an ignored directory: with
   `secrets/` ignored, `secrets/x/key` is kept.
3. For each path: skip it if the destination exists (checked with
   `symlink_metadata`, so dangling symlinks count as existing), create the
   parent dirs, then copy. Symlinks are recreated as symlinks (unix). Regular
   files go through `std::fs::copy`, which keeps permissions, clones on APFS
   and uses `copy_file_range` (reflink-capable) on Linux.

Step 1 walks untracked directories (e.g. `node_modules`) looking for matches.
That's git's own directory walk: fast in practice, and it runs once per
worktree.

## Link: `worktree-link`

Entries are **literal repo-relative paths**, not globs. A link can be set up
before the file exists anywhere, and moving files into the store needs
concrete names. A trailing `/` is accepted but isn't needed. Entries with `..`,
`.`, `.git`, or an absolute path are a config error and fail `run`.

- **Store**: `<config>/shared-files/<sandbox>/<path>` holds the real file or
  directory. There's one store per sandbox, shared by the base instance and
  every worktree instance. `rm` never touches it.
- **Mount**: the store dir is mounted at its **identical absolute host path**,
  the same trick `git_companion_mount` uses for the base `.git`. The absolute
  symlinks then resolve inside the container too. The mount is read-write, so
  an edit from any instance is seen by all of them.

The link step runs on every `run` and `rebuild`, and does nothing when
everything is already in place. It runs after `initializeCommand`, so a file
that command generates is adopted. For each entry:

1. **Guard**: the path must be gitignored and untracked in the base repo
   (`git check-ignore`, which never reports tracked paths). Otherwise it warns
   and skips the entry. Putting a symlink where a tracked file is would show
   up as a modification.
2. **Adopt** (the one step that changes the base repo): if the store has no copy
   yet and the base repo has a real file or dir there, move it into the store.
   If `rename` fails across filesystems, a file is copied and then removed; a
   directory produces a warning asking you to move it yourself.
3. **Link**: in the base repo and in the worktree, if there is one, create an
   absolute symlink to the store copy. A correct link already in place is left
   alone. Anything else at that path (a real file, or a symlink pointing
   elsewhere) warns and is **never replaced**. Merge it into the store copy by
   hand and delete it; the next `run` or `rebuild` links it.
4. If neither the store nor the base repo has the path, the entry is skipped
   silently, and a later run links it once the file exists.

Caveat: a `dir/` gitignore pattern matches directories only, and git sees a
symlink as a file, so a linked directory shows up as untracked. The link step
checks for this after linking and warns. Fix it by dropping the trailing `/`
from the `.gitignore` pattern (e.g. `secrets` rather than `secrets/`).

If a path is in both lists, the link wins: links are made first, and the copy
never overwrites.

## Failure policy

Filesystem and git problems are **best effort**: they print a `warning:` on
stderr and never fail `run`, because a missing secret is easier to recover from
than an instance that won't start. Only an invalid `worktree-link` entry (a
config error) fails `run`. Nothing is printed on success: `run` is also driven
from the TUI, and stdout carries the instance name for scripts.

## Code landmarks

- `src/config.rs` — `worktree_include` / `worktree_link` on
  `SandboxProperties`.
- `src/commands/run/worktree.rs` — `copy_worktree_includes` and
  `link_shared_files`, beside `create_worktree`.
- `src/commands/run/mod.rs` (`materialize`) — after `initializeCommand`, first
  links (store mount pushed onto `extra_mounts`), then the copy when
  `fresh_worktree` (`run` passes true, `rebuild` false). Both happen before the
  container exists, so lifecycle commands see the files.
- `src/commands/rm.rs:67` — `git worktree remove` without `--force`. Verified
  that ignored files don't make a worktree unclean, so copies and links neither
  block removal nor survive it. The store is untouched.

## Out of scope (possible follow-ups)

- `.worktreeinclude.local` (per-user additions and negations, read last).
- Globs in `worktree-link`, read-only store mounts, a per-repo (rather than
  per-sandbox) store, and an `unlink`/restore command.
- A command to re-seed copies into an existing instance.
- Serving shared files through `devsbd` into a tmpfs, so secrets never sit in
  plaintext under the config dir.
