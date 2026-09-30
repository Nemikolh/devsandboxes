# Worktrees for `folders` entries

## Problem

`folders` entries (`[sandbox.*].folders`, container path -> host dir) are
always bind-mounted directly (`src/commands/run/mod.rs:437`). When the entry
is another sandbox's project, every instance shares that checkout: the owner
switching branches changes what the dependent sees, and two instances can
write to one working tree. The primary `folder` has had worktrees for a long
time (`base_in_use`, `src/commands/run/mod.rs:161`, `:671`); extras never do.

## Rule

Each host folder has one **owner** of its direct checkout:

- a folder that is some sandbox's `folder` is owned by that sandbox;
- otherwise the first instance that mounts it (as its `folder` or a `folders`
  entry) holds it.

A `folders` entry in mode `auto` (the default, also the plain-string form):

| Condition (checked in order) | Mount |
| --- | --- |
| not a git repo root (`<path>/.git` is not a directory) | direct bind, as today |
| instance is a dispatcher child | worktree |
| folder is **owned** (see below) | worktree |
| some other instance **directly mounts** it (primary or extra) | worktree |
| otherwise | direct bind (this instance now holds it) |

- `worktree = "always"`: worktree whenever it's a git repo root; error if it isn't.
- `worktree = "never"`: always direct bind (the live view, e.g. a shared scratch dir).

**Owned** = any sandbox in the *current* config whose resolved `folder` is this
path, **or** any instance in the global state (any config root) whose
`base_folder` is this path. The state check guards two config roots that
collide. A root with no instances can't be seen; that's accepted.

**Directly mounts** = any instance other than the one being built whose
`folder` (the primary checkout it mounts) is this path, or whose recorded
`folders` has this path with no worktree.

The primary folder's rule is still `base_in_use`, now extended to count direct
`folders` mounts as well. If an instance already holds a folder as an extra, a
new instance of the folder's own sandbox gets a worktree rather than sharing
the checkout.

## Extra-folder worktrees

- **Detached HEAD at the base checkout's current `HEAD`** (`git worktree add
  --detach <wt> HEAD`). No branch is created: this avoids "branch already checked
  out" errors, leaves nothing for `rm` to delete, and gives a clear starting point.
  An agent that wants a branch can create one. An unborn HEAD is an error, like
  the primary worktree's.

- Path: `<config_dir>/.worktrees/<instance_id>.folders/<basename>-<hash8>`
  (`hash8` = `short_hash` of the canonical base path, same as `link_store`).
  It lives outside `.worktrees/<instance_id>/` because that directory *is*
  the primary worktree.

- Each one needs `git_companion_mount(base)` (`src/commands/run/worktree.rs:12`),
  so the worktree's `.git` pointer resolves in the container.

- `worktree-include` / `worktree-link` are **not** applied to extra worktrees
  (they're configured for the primary repo). This is a known limitation.

## State

New `Instance.folders: Vec<FolderMount>` (`src/state.rs`, `#[serde(default, skip_serializing_if = "Vec::is_empty")]`):

```rust
pub struct FolderMount {
    pub target: String,          // container path
    pub base: PathBuf,           // canonical host folder
    pub worktree: Option<PathBuf>,
}
```

Every extra entry is recorded, direct mounts included, because "directly mounts"
needs them. Pre-upgrade instances have none recorded, so their direct extra
mounts are invisible to the rule until they're rebuilt. That's accepted.

## Step 1: config schema for `folders` entries

- `src/config.rs:77`: `folders: Option<BTreeMap<String, FolderEntry>>`, where
  `FolderEntry` is an untagged enum: a plain `String` (path, mode `auto`) or a
  table `{ path = String, worktree = "auto" | "always" | "never" }` (`worktree`
  optional, default `auto`; unknown keys error, `deny_unknown_fields`). Give it
  accessors (`path()`, `worktree_mode()`) and a `FolderWorktree` enum
  (`Auto`/`Always`/`Never`, kebab-case serde).

- `resolve_folders` (`src/commands/run/mounts.rs:103`) returns the mode next to
  each `(target, host)` pair. Callers keep their current behaviour: everything
  is still direct-mounted in this step.

- Tests: extend `folders_parses_and_merges_through_extends`
  (`src/config.rs:979`) with the table form and a bad `worktree` value; check
  that `config_hash` still works.

- Spec: `skills/config-toml-spec/SKILL.md:65` documents the table form and
  modes. Behaviour text lands in step 3, so mark it as the syntax only here, or
  write the final wording now if that's simpler; step 3 must leave it accurate.

## Step 2: state record + pure decision logic

- `FolderMount` + `Instance.folders` as above. Update every `Instance { .. }`
  literal in tests (grep `branch_created: true,`).

- A pure, unit-tested decision function in `src/commands/run/` (new file
  `folders.rs`, re-exported as needed), e.g.
  `fn folder_needs_worktree(mode, is_repo_root, dispatched, owned, direct_in_use) -> Result<bool>`
  implementing the table above (`always` on a non-repo -> error).

- `fn folder_owned(config, dir, state, path) -> bool` and
  `fn folder_directly_mounted(state, path, except_instance: Option<&str>) -> bool`.
  Sandbox folders that fail to resolve or canonicalize are skipped. Compare
  canonical paths.

- Extend `base_in_use` (`src/commands/run/mod.rs:671`) to count direct
  `folders` mounts. Keep and extend its test (`:1029`).

- `create_detached_worktree(base, worktree) -> Result<()>` in
  `src/commands/run/worktree.rs`, modelled on `create_new_branch_worktree`
  (`:121`): verify `HEAD^{commit}`, `host_git(base)` `worktree add --detach`,
  then `ensure_populated`. Test it with a temp repo, like the existing worktree tests.

- Nothing is wired into `run` yet.

## Step 3: wire into `materialize` (run + rebuild)

- In `materialize` (`src/commands/run/mod.rs:313`), after `resolve_folders`
  (`:369`), build the `FolderMount` list per entry:

  - if the prior state entry (`prior`, `:329`) recorded the same `target` with
    the same `base`, **reuse** that record as-is (rebuild keeps the working tree,
    like the primary worktree);

  - otherwise decide with step 2's functions (`dispatched` =
    `dispatcher.is_some()`, `except_instance` = this instance) and create the
    detached worktree when needed. Run `check_repo(base)` before any host git,
    as `rm` does.

- Mount each entry's `worktree.unwrap_or(base)` at `target` (`:437`), and add a
  `git_companion_mount(base)` for each worktree entry (`:408`). Skip duplicate
  `.git` mounts if the primary worktree already has the same base.

- Record `folders` on the new `Instance` (`:529`). Keep prior records whose
  target is gone from config, so `rm` still cleans their worktrees (same idea
  as `merge_volumes`).

- `rebuild.rs` needs no change if the reuse happens inside `materialize`;
  confirm it.

- Tests: whatever can be tested without a runtime (the reuse/merge helper as a
  pure function). Add a `#[test_utils::docker_test]` only if it's cheap.
- Update the spec behaviour text (`skills/config-toml-spec/SKILL.md:56` `folder`

  row and `:65` `folders` row) and the `.worktrees/` note at `:20`.

## Step 4: `rm` cleans extra worktrees

- `src/commands/rm.rs`: during the preflight (`:31`), `check_repo(base)` +
  `check_worktree_removable` for **every** recorded extra worktree, before any
  teardown. After the container is gone, `remove_worktree(base, wt)` for each,
  then remove the empty `.worktrees/<id>.folders/` dir (best effort). No branch
  handling is needed, since the worktrees are detached.

- Tests: extend the existing temp-repo preflight test or add one for multiple worktrees.

## Step 5: docs + changelog

- `docs/run-command.md` (worktree section) and `docs/rebuild.md`, where they
  describe worktree decisions. Keep this doc as the topic reference.

- `CHANGELOG.md` `## Unreleased`: a `### Changed` entry covering the new
  behaviour (a second instance / another sandbox's folder now gets a detached
  worktree) and the `worktree = "never"` opt-out for the old live-share behaviour.

- Site: the spec is rendered verbatim, so check the site builds (`cd site && pnpm check && pnpm build`) if the spec changed shape.

## Out of scope (parked)

- `run --json` / `status --json` / TUI surfacing of extra worktrees.
- `worktree-include` / `worktree-link` for extra repos.
- `folders` entries that are subdirectories of a repo (not a repo root): still
  direct-mounted under `auto`.
