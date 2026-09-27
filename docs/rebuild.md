# Plan: `rebuild` — recreate a drifted container, keep the worktree

## Goal

`devsandbox rebuild <name>` (alias: `recreate`) removes an instance's
container and re-materializes it from the *current* config, preserving the
worktree, branch, instance name, and per-instance state (shell history,
`${instance}`-anchored mounts, per-instance volumes, which only `rm` deletes). No drift → no-op with a message. In the TUI,
`s` on a stopped instance with drift rebuilds (suspending the TUI) instead of
bare-starting.

## Background (code landmarks)

- `src/commands/run.rs:16-219` — `run()`: resolve config → pick instance name
  → worktree decision (`base_in_use`, `create_worktree`, run.rs:122-141) →
  materialize (services `ensure_services` run.rs:119, mounts/caches/history
  run.rs:142-156, `run_container` run.rs:157-169, workspace file, state
  insert run.rs:173-191, vscode config, lifecycle chain run.rs:198-215).
- `src/commands/run.rs:325-335` — `warn_on_drift`; message says "remove and
  re-run", must point at `rebuild` after this lands.
- `src/commands/rm.rs:37-47` — today's only fix path; destroys worktree.
- `src/commands/start.rs:78-86` — `resolved_sandbox`: config-root/project
  match pattern to copy (but rebuild bails instead of falling back).
- `src/commands/services.rs:126-127` — `ensure_services` already recreates
  drifted *service* containers; rebuild gets that for free.
- `src/tui/app.rs:701-728` — `stop_or_start_instance` (`s` key);
  `src/tui/data.rs:56,704-709` — per-row `drift` flag already computed.
- `src/tui/mod.rs:111-127,231-267` — pending prompt action → `run_suspended`
  path (suspends TUI, inherited stdio, "press any key", error log).
- `src/main.rs:26-109` — clap `Command` enum.

## Step 1 — extract `materialize` from `run()` (pure refactor)

Factor the part of `run()` after the worktree decision into a
`pub(crate) fn materialize(...)` in `src/commands/run.rs` taking roughly
`(dir, &Config, sandbox_name, &ResolvedSandbox, instance, source, worktree,
branch)` and doing: `initializeCommand`, `ensure_services`, var-ctx /
workspace / mounts / folders / caches / shell-history resolution,
`run_container` (with `git_companion_mount` when `worktree.is_some()`),
workspace file, state upsert (`insert` — overwrite is wanted for rebuild),
vscode name config, lifecycle chain. `run()` keeps: config load, name pick,
existence checks, `base_in_use` + `create_worktree`, and the final
`println!("{instance}")`.

Constraints:
- Zero behavior change to `run`; byte-identical container args.
- The `${instance}` context must be built from the passed instance name so
  per-instance mounts/history resolve to the same host paths on rebuild.
- All existing tests keep passing (`cargo test`).

## Step 2 — `rebuild` command + alias

New `src/commands/rebuild.rs`, wired in `src/main.rs` as
`Rebuild { name }` with `#[command(visible_alias = "recreate")]`.

Behavior:
1. `resolve_instance` against state (same name forms as rm/start).
2. Load config at `dir`; **bail** (don't fall back like `start` does) when:
   config doesn't load, `info.project` is non-empty and ≠
   `services::project_id(dir)` ("created from a different config root; rerun
   with `-C <root>`"), or the instance's sandbox is gone from config.
3. Drift check via `backend().label(container, "devsandbox.config_hash")`
   vs `sandbox.config_hash`:
   - equal → `println!("no config drift for `{key}`; nothing to do")`, exit 0;
   - label/container missing (container was removed manually) → proceed:
     rebuild is the only way to re-materialize without losing the worktree
     (`run` refuses the taken name, rm.rs kills the worktree);
   - differs → proceed.
4. `backend().remove_force(&info.container)`, then `run::materialize` with
   the stored `folder` (source), `worktree`, `branch`. State entry is
   replaced, refreshing config-derived fields (`workspace`, `remote_env`,
   `remote_user`, `workspace_file`). Lifecycle chain re-runs in full
   (container is fresh; the working tree survives — that's the point).
5. Print `rebuilt {key}`.

Also: update `warn_on_drift` (run.rs:325) text to
"run `devsandbox rebuild <instance>` to apply changes".

Tests: pure parts only (no runtime in CI) — e.g. the bail/no-op decision
logic if factored testable; follow existing `#[cfg(test)]` style.

## Step 3 — TUI: `s` on stopped + drifted instance rebuilds

- Add `PromptAction::Rebuild { instance }` (`src/tui/prompt.rs`), handle it
  in `run_suspended` (`src/tui/mod.rs:231`) calling
  `commands::rebuild::rebuild`, and in `log_error`'s verb match.
- `stop_or_start_instance` (`src/tui/app.rs:701`): on
  `ContainerStatus::Exited` with `row.drift`, queue the suspend-path action
  (same mechanism the `:` prompt uses) instead of `pending_start`; status
  line "rebuilding {name}…" is unnecessary — the TUI suspends immediately.
  No drift → unchanged background bare-start.
- Also register `:rebuild` / `:recreate` in the `:` prompt parser for parity
  with `:start`/`:rm` *(orchestrator suggestion — cheap once the action
  exists; drop if unwanted)*.
- Unit tests in `app.rs`: `s` on exited+drift queues the rebuild action and
  not `pending_start`; exited without drift still bare-starts; running row
  unaffected by drift (still stops).

## Step 4 — docs

- `README.md`: add `rebuild <name>` row to the command table.
- `skills/devsandbox-cli/SKILL.md:147-148`: drift note now points at
  `rebuild` (worktree preserved) instead of `rm` + `run`; add to command
  list.
- `AGENTS.md` module map: add `rebuild` to the `src/commands/*.rs` verb list.
- TUI help/keybind text if `s` is described anywhere in `ui.rs`/help bar.

## Out of scope

- `--force` rebuild without drift (user chose strict no-op).
- `rebuild --all`.
- Any git operation on the worktree/branch; dirty trees pass through
  untouched.
