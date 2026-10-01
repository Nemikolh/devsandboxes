# Dispatchers: which branches are already checked out

Status: implemented.

## Problem

A dispatcher has no way to know that a branch it is about to work on is already in use by someone. The PR babysitter (`.devsandboxes/dispatcher`) ran `gh pr update-branch` and pushed agent merges on PRs whose branches were checked out in the user's own instances (`bolt-restate`, `bolt-restate-2`, `cleanup-daytona-sandboxes`). That leaves those checkouts behind `origin`, and a later force-push from them silently undoes the dispatcher's work.

What `devsbd` offers today:

- `devsbd ls` lists only the dispatcher's own children, and their `branch`
  comes from `state.toml` (`src/commands/dispatch.rs:557`). That value is the
  branch at creation, so it goes stale after a `git switch` inside the
  instance (`bolt-restate` was switched to
  `joan/durable-agent-private-registries` by hand).

- `devsbd ensure --branch B` fails when `B` is a local branch checked out
  elsewhere (`create_worktree`, `src/commands/run/worktree.rs:88`). This only
  happens at creation, and only on the `Local` path. A dispatcher that acts
  through GitHub alone (`update-branch`) never calls `ensure`, and for a branch
  that exists only on `origin` the check never runs.

A skip label on the PR is not an option for this user. The signal has to come from where the branches are actually checked out.

## Design: `devsbd branches <sandbox>`

A new read-only control op. It reports every branch checked out in any worktree of `<sandbox>`'s base repository, and who holds it.

```bash
devsbd branches <sandbox> [--ahead]
```

```json
[
  {"branch": "joan/durable-agent-private-registries", "holder": "instance", "instance": "bolt-restate"},
  {"branch": "fix/supabase-wake-suspended-project-on-query", "holder": "instance", "instance": "bolt-restate-2"},
  {"branch": "feat/agent-run-cost-event", "holder": "child", "instance": "bolt-restate-pr-6900"},
  {"branch": "master", "holder": "base"},
  {"branch": "joan/hand-made", "holder": "external"}
]
```

With `--ahead`, every row gets an `ahead` count, and local branches that are ahead of `origin` but not checked out anywhere are listed too:

```json
[
  {"branch": "joan/durable-agent-private-registries", "holder": "instance", "instance": "bolt-restate", "ahead": 2},
  {"branch": "master", "holder": "base", "ahead": 0},
  {"branch": "joan/wip-not-pushed", "holder": "local", "ahead": 3}
]
```

- **Source of truth: live `git worktree list --porcelain`** on the sandbox's
  resolved `folder` (the base repo), not `InstanceInfo.branch`. This catches
  `git switch` inside an instance, and worktrees created by hand outside
  devsandbox.

- **`holder`**:
  - `child`: a worktree of one of *this* dispatcher's children.
  - `instance`: a worktree of any other instance, from any config root. The instance is found by matching the path against `state.toml` (`instance_at`).
  - `base`: the base checkout itself.
  - `external`: a worktree that devsandbox doesn't know.
  - `local`: (`--ahead` only) a local branch in no worktree, with `ahead > 0`.

- **`ahead`** (`--ahead` only): `git rev-list --count origin/B..B`, the
  commits on the local branch that `origin` doesn't have. A dispatcher pushing
  to that PR would make it diverge, even if nobody has it checked out. The
  field is left out when `origin/B` doesn't exist (never pushed, so no PR to
  act on). It costs one git call per branch, hence opt-in.

- Detached worktrees (e.g. `folders` extras, `docs/folders-worktrees.md`) have
  no branch and are left out. A `prunable` record (its directory is gone) is
  left out too: git would let the branch be checked out again.

- Stopped instances still hold their worktree, so they count. Stopping an
  instance is not the same as giving up its branch.

- **Scope / auth**: `<sandbox>` must be in the dispatcher's `spawn` (same
  check as `ensure`, exit 77 otherwise). The dispatcher can already create
  worktrees of that repo, so branch names and instance names reveal nothing
  new. Host paths are not returned.

- Kept separate from `ls`. `ls` means "my children" and its scripts reconcile
  against it. Mixing in other instances would make every consumer filter
  `holder`.

### Why not have the dispatcher read git itself

The dispatcher's mount of a worktree is just a `.git` file pointing at a host path (`/home/<u>/…/.git/worktrees/<name>`), and that path isn't mounted in the container. Only the host side can list worktrees.

## Consumer: the PR babysitter (separate repo)

In `handlePr`, before `decide`: if `pr.headRefName` is held by anything other than its own child `<sandbox>-pr-<N>`, the PR is ineligible. With `--ahead`, a `local` row for `pr.headRefName` makes it ineligible too. That covers update-branch, agent runs and comment drafts alike. Notify once per holder change (`pr-<N>-checked-out`, info; `pr-<N>-held` is already taken by the held-comments notice, "checked out in `bolt-restate`") so a skipped PR is visible. Call `branches --ahead` once per repo per pass, not once per PR.

## Steps

One commit per step. Checks: `cargo test --workspace` (steps 1-3); `pnpm check && pnpm test` in `../.devsandboxes/dispatcher` (step 4).

### Step 1: host op (`src/devsbd/control.rs`, `src/commands/dispatch.rs`, `src/commands/run/worktree.rs`)

1. `src/devsbd/control.rs`: `Op::Branches` (`"branches"`), added to `Op::ALL` (`:77`, size 10 → 11) and `as_str`. A bare `ahead` flag on `Request` (`ahead: bool`), coded exactly like `force` (`encode_request` `:327`, `decode_request` `:358`, rejects a value and a repeat). Update the module doc's op list and field table (`:11`-`:21`) and the `body` line (`:29`: JSON for `branches` too). Codec tests: extend the round-trip fixtures or add one for `op branches\nsandbox web\nahead\n`.
2. `src/commands/dispatch.rs`:
   - `check_fields` (`:270`): `Branches` → `sandbox` required, `key` forbidden. `ahead` only on `Branches` (optional), via the existing `only` helper. The `branch`/`env` guard (`:290`) and the other `field` lines already forbid the rest.
   - Handler arm in `handle_with` (`:197`): deny (77) unless `decl.may_spawn(sandbox)`; deny an unknown sandbox, as `Ensure` does (`:202`-`:210`). Resolve the base repo the way `run` does (`src/commands/run/mod.rs:102`-`:108`): `config_dir.join(folder).canonicalize()`; no `folder` or a missing one → `failed`.
   - `Executor` (`:85`) gets one new method, `fn git(&mut self, repo: &Path, args: &[String]) -> Result<String, String>` (stdout on success, else a short message). `Subprocess` implements it with `crate::commands::run::host_git` (captured output, never inherited: this runs on the TUI's bridge worker). The `Fake` (`:773`) serves canned stdout keyed by the argv, and records the calls.
   - Rows: `git worktree list --porcelain` on the base repo, parsed (below). Per record with a branch: `instance_at` (`src/commands/run/worktree.rs:258`, make it `pub(crate)`) → `child` if that instance's `dispatcher == Some(owner_id)`, else `instance`; no instance and the path is the first record (git always lists the main worktree first) → `base`; else `external`. An instance on the base folder itself therefore reports as `instance` (more useful than `base`). Host paths never reach the body.
   - With `ahead`: `git for-each-ref --format=%(refname) refs/heads refs/remotes/origin` once, then `git rev-list --count refs/remotes/origin/B..refs/heads/B` only for local branches that have `refs/remotes/origin/B` (full refs, so a host branch name can't read as an option). Worktree rows get `ahead`; local branches in no worktree with `ahead > 0` get a `local` row; no `origin/B` → no `ahead` field (`skip_serializing_if`). `refs/remotes/origin/HEAD` is not a branch.
   - JSON via `serde_json::to_string_pretty` like `ls` (`:557`), rows sorted by branch for stable output. A `BranchRow` struct alongside `ChildRow` (`:549`): `branch`, `holder` (lowercase enum), `instance: Option` (skipped when `None`), `ahead: Option<u64>` (skipped when `None`).
   - Module doc (`:1`-`:31`): one sentence on `branches`.
   - Not added to `writes_state` (`src/devsbd/bridge.rs:234`): read-only, so it skips the control lock; the `only_state_writing_ops_take_the_control_lock` test must still pass unchanged.
3. `src/commands/run/worktree.rs`: replace `worktree_with_branch` (`:240`) with a `pub(crate)` parser returning every record as `(path, Option<branch>, prunable: bool)` (strip `refs/heads/`), and rebuild `checked_out_at` (`:233`) on top of it. Keep `worktree_list_porcelain_finds_branch` (`:1109`) passing, adapted to the new parser, plus a `prunable` record.
4. Tests (`dispatch.rs`, alongside `ls_lists_owned_children_as_json` `:1367`), all on canned porcelain/refs through the `Fake`:
   - one row for each holder kind;
   - a hand-switched instance reports its live branch, not the recorded `InstanceInfo.branch`;
   - detached and prunable records are skipped;
   - without `ahead`, no `ahead` field, no `local` rows, and no `for-each-ref`/`rev-list` calls;
   - with `ahead`: counts on worktree rows, a `local` row for an unpushed-ahead branch, none for a branch at `ahead: 0`, no `ahead` when `origin/B` is missing;
   - a sandbox not in `spawn` is denied (77), with no git call;
   - a `key` is rejected as a usage error (2); `ahead` on another op too.

### Step 2: helper CLI (`devsbd/src/ctl.rs`, `devsbd/src/main.rs`)

- `parse_args` (`devsbd/src/ctl.rs:100`): `branches <sandbox> [--ahead]`; exactly one positional, put in `req.sandbox`; `--ahead` takes no value (like `--force`, `:136`); every other flag is unknown. Usage line in `USAGE` (`:19`), module doc (`:1`).
- The default arm of `execute` (`:250`) already prints the body; nothing else to do there.
- `devsbd/src/main.rs:51`: add `"branches"` to the control verbs, and to the usage line (`:59`).
- Tests next to `parses_each_verb` / `usage_errors`: parse with and without `--ahead`, `--ahead=1`, no/extra positional, `--key`; codec round trip.

### Step 3: docs

- `docs/automations-guide.md`: `devsbd branches` in the control commands block (`:65`), a bullet next to `ls`, and a pattern line in `## Patterns` (`:111`) ("skip items whose branch is checked out elsewhere").
- `docs/automations.md`: the op list and the codec description (`:351`).
- `site/`: check `site/src/content/docs/going-further.mdx` and `site/src/content/examples/babysit-pr.mdx` for a control-command list that should gain `branches`; run `cd site && pnpm check && pnpm build` if anything changed.
- `CHANGELOG.md`: an `### Added` entry under `## Unreleased`.
- This doc: `Status: implemented.`

## Non-goals

- **Other clones.** A branch checked out in a clone devsandbox doesn't manage
  (another machine, or a second clone of the repo) is invisible here. That's
  ignored.

## Future

- **Hand-over.** When the user stops working on a branch (switches away, or
  removes the instance), the PR silently becomes eligible on the next pass.
  For now that's what's wanted: removing the instance is the signal that the
  dispatcher can take the branch over. A finer hand-over (e.g. a "was held"
  mark kept until a push from someone else) may come later.
