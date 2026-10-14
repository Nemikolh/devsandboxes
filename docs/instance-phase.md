# Instance phase: no half-built sandboxes, build logs, a TUI that doesn't suspend

Status: plan, in progress. Rollout steps 1–2 of `docs/client-server.md`,
all in-process (no daemon changes). The verbs-as-requests work, the
dashboard as a pure client and machines get their own plans.

## Problem

`run` writes the instance to `state.toml` only after the container exists
(`src/commands/run/mod.rs:640`). By then the worktree, branch, `folders`
worktrees, services and image are already made, so a `run` that fails or
is killed earlier leaves them with nothing pointing at them, and `rm` can't
find them. The TUI runs `run`/`rebuild`/`start` suspended in its own
process (`src/tui/mod.rs:610` `run_suspended`), full screen, and keeps no
trace of the output once you press a key.

## Decisions

- **Record first.** `run` records the instance before creating anything,
  with an in-flight `progress`, and fills in each created resource as it's
  made. The final save clears `progress`.
- **Phases**: `preparing` (worktree, branch, folders, links, services),
  `building` (image), `creating` (container), then the lifecycle step
  running (`onCreateCommand` … `postAttachCommand`, plus `shell-rc` for the
  rc wiring). Plus `failed`, carrying the step it was at and the error.
- **Failure before the container exists → automatic rollback.** Nothing
  is worth inspecting yet, and that's what users expect today. Undo what
  was recorded (services/network, `folders` worktrees, the worktree, a
  branch this run created that has no commits beyond its start point), then
  remove the record. *(Orchestrator's call, not discussed with the user
  yet: the design doc said every failure becomes `failed`.)*
- **Failure from the container on → `failed`, kept.** This is today's
  "container kept" behaviour, now visible. Resume = `rebuild <instance>`,
  which already re-materializes from the recorded worktree/branch. Remove =
  `rm`.
- **Cancel** (SIGINT/SIGTERM during `run`): roll back everything, the
  container included, even after the container exists. A second signal
  exits immediately (default disposition restored after the first).
  Cancelling a `rebuild` leaves `failed` (the instance existed before).
- **Interrupted** (SIGKILL, OOM, reboot): `progress` keeps a `pid` and the
  boot id (`src/commands/autostart.rs:276` `boot_id`). A reader that finds
  an in-flight phase whose pid is dead or whose boot id changed shows it as
  `failed` ("interrupted"). That's derived at read time, with nothing
  rewritten, so there's no daemon dependency.
- **One build log per instance**:
  `<data>/devsandbox/logs/<instance_id>/build.log`, rotated to
  `build.1.log` by the next `run`/`rebuild`/`start`, removed by `rm`.
- **The TUI never suspends for `run`/`rebuild`/`start`**: it spawns a
  detached `devsandbox` subprocess, so the work outlives the dashboard. The
  status column shows the phase, and a key opens the build log. No jobs or
  operations list anywhere.

## Constraints (every step)

- Mimic surrounding code. Doc comments say why. Tests in `#[cfg(test)]`
  modules, pure logic factored to be testable without docker; docker tests
  use `#[test_utils::docker_test]`.
- `cargo test --workspace` must pass at the end of every step.
- `state.toml` changes are additive (`#[serde(default, skip_serializing_if
  …)]`): an older binary must still read the file.
- Every state write goes through `State::update` (never save a stale
  snapshot: see the comment at `src/commands/run/mod.rs:638`).
- Don't touch git. Don't touch `docs/helper-rpc.md` (unrelated, untracked).

## Step 1: the `progress` model

`src/state.rs`: add `Instance::progress: Option<Progress>` (serde default,
skipped when `None`). `Progress { phase: Phase, pid: u32, boot: Option<String>,
error: Option<String> }`, with `Phase` an enum: `Preparing`, `Building`,
`Creating`, `Lifecycle(String)` (the step name), `Failed { at: String }`.
Pick a TOML-friendly serde shape and test the round-trip.

A pure fn deriving what readers show, with liveness injected:
`Progress::view(&self, alive: impl Fn(u32) -> bool, boot: Option<&str>) -> PhaseView`,
where `PhaseView` is `InFlight(label)` or `Failed { at, error }`
(interrupted = `Failed` with error "interrupted"). Plus a real-liveness
helper: unix `libc::kill(pid, 0)` (EPERM counts as alive), non-unix assumes
alive. Make `boot_id` in `src/commands/autostart.rs:276` `pub(crate)` for
it.

`Instance` has many constructors in tests: add the field everywhere it's
needed. No behaviour change in this step.

## Step 2: `run` records first; rollback before the container

`src/commands/run/mod.rs`:
- After naming and id allocation (`:144`), and after `base_in_use` is
  computed (`:174`, against the state *without* the new record), insert a
  provisional `Instance` via `State::update`, checking inside the closure
  that the name and id are still free (this also closes the race between
  two concurrent `run`s picking one default name). It has every field
  known now (`sandbox`, `instance_id`, `project`, `container`,
  `config_dir`, `base_folder`, `folder` = planned source, `dispatcher`,
  `extra_env`, `created_unix`) and `progress: Preparing`.
- Record `worktree`/`branch`/`branch_created` right after `create_worktree`
  succeeds (`:193`), `folders` right after `folders::mount_folders` (`:448`),
  volumes when resolved. Check what `create_worktree` and `mount_folders`
  read from `state` (`&state` at `:194`, `:452`) so the provisional record
  doesn't change their decisions.
- Phase updates at `materialize`'s boundaries: `Building` around
  `image_for` (`run_container`, `:842`, so that's where the call goes, or
  split the image step out of `run_container`), `Creating` before
  `docker run`, `Lifecycle(name)` per step (`:693`, `:706`). Keep them as
  cheap one-field `State::update`s through one small helper.
- The final save (`:640`) keeps `progress: None` but must **merge**, not
  overwrite: it already builds the full record, so just make sure it
  doesn't race the provisional one away (same key, `insert` replaces).
- Errors before the container exists: roll back and remove the record,
  then return the original error. Put the rollback in one fn, reusing
  `rm`'s pieces (`src/commands/rm.rs`: `remove_container` for
  services/network, `remove_folder_worktrees`, `remove_or_prune`,
  `delete_branch_ref`), so it can be extended in step 3. The branch is
  deleted only when this run created it and it has no commits beyond its
  start point.
- Errors from the container on: set `Failed { at: <current phase> }` +
  error, keep everything, and reword the error context (today `"… failed
  (container `x` kept)"`) to point at `devsandbox rebuild <name>` to retry
  and `devsandbox rm <name>` to remove.
- `rebuild_instance` (`src/commands/rebuild.rs:84`) also calls
  `materialize`: there `progress` starts at `Building` on the existing
  record, and a failure leaves `Failed` (never a rollback).

Tests: the rollback decision and the branch rule as pure fns. A
`docker_test` that a `run` failing in `initializeCommand` (or a bad image)
leaves no record and no worktree, and one failing in `postCreateCommand`
leaves a `Failed` record.

## Step 3: cancel, interrupted, and guards

- `run`/`rebuild` install a SIGINT+SIGTERM handler (unix, `libc::sigaction`
  setting an `AtomicBool`, then restoring the default disposition so a
  second signal kills), scoped by a guard that restores the previous
  handler on drop. Children in the foreground process group get the
  terminal's SIGINT and die, so the step in progress returns an error. On
  any error with the flag set: `run` rolls back fully (container included,
  the step-2 rollback extended with `remove_container` + volumes), and
  `rebuild` leaves `Failed { at }` with "cancelled". The error message says
  "cancelled".
- Readers use `Progress::view` with real liveness:
  - `start` (`src/commands/start.rs:42`) and `exec` on an in-flight
    instance refuse ("still <phase>"). On `failed`, they refuse and point
    at `rebuild`/`rm`.
  - `rebuild --all` and autostart (`src/commands/autostart.rs`) skip
    in-flight and failed instances.
  - `rm` on a live in-flight instance refuses ("still <phase> (pid N): wait,
    or Ctrl-C it"), even with `--force`. Interrupted or failed ones are
    removable as usual. `rm` must cope with a record whose container never
    existed (it mostly does already: `remove_container` is best-effort).
  - The dispatcher's `ensure` (`src/commands/dispatch.rs`) on a child that
    is in flight or failed: report it in the response instead of
    starting/creating. Read the code and pick the response that fits
    `docs/automations.md`, and document it there.
- Grep every `state.instances` consumer (list in the step brief) and
  confirm each one either works with a provisional record or is guarded.
  Report the list in the summary.

## Step 4: build log

- `run`, `rebuild` and `start` tee their stdout+stderr (their children
  included) into `<data>/devsandbox/logs/<instance_id>/build.log` from
  the moment the instance id is known, rotating an existing `build.log` to
  `build.1.log`. Unix: fd-level tee (pipe + `dup2` onto 1 and 2, a thread
  copying to the original fds and the file, joined/flushed on drop). Must
  compose with `run --json` (`src/json_stdout.rs`, which already points fd 1
  at stderr): the tee goes on top of whatever fds 1/2 are. Non-unix: no
  tee (note it). Children then see a pipe, not a TTY: check
  `exec_lifecycle` and the image build still behave (plain progress output
  is fine), and that the pick/confirm prompts happen before the tee or
  read `/dev/tty`.
- `rm` removes `logs/<instance_id>/`.
- `devsandbox logs --build <instance> [--follow]` (`src/commands/logs.rs`,
  `src/main.rs`): print the build log. `--follow` tails until the
  instance's `progress` is no longer in flight (by `Progress::view`).
- Docs: the `logs` section of the user docs (find where `logs` is
  documented under `site/` and `docs/`) and the CLI help.

Build log, as built (`src/build_log.rs`): the tee is opt-in
(`build_log::enable()` from `main` for the three verbs), since the dashboard
and `serve` call them in-process; until step 6 dashboard runs have no log.
Two pipes and two copier threads: the terminal gets bytes as they come, the
log whole lines. The command's error is appended as a last `error:` line.
A rolled-back `run` removes the log dir with the record, like `rm`. What
`main` prints after the command (the instance name, autostart) isn't logged.

## Step 5: phase in `ps` and the snapshot

- `src/snapshot.rs` `InstanceRow` (`:53`): add `phase: Option<PhaseRow>`
  (serialized, additive: `{"state":"building"}` / `{"state":"failed","at":…,"error":…}`).
  Fill it in `instance_row` (`:702`) via `Progress::view` with real
  liveness.
- `devsandbox ps` STATUS shows the phase (`building`, `postCreateCommand`,
  `failed`) instead of `missing`/`exited` when set.
- TUI status column (`src/tui/ui.rs:1852`, instance rows) shows the same,
  with a style for in-flight and one for failed. The Detail panel shows a
  `failed` row's step + error. `tui/app/actions.rs:118` (one-key actions by
  status): in-flight rows offer nothing that would race the build, and
  failed rows offer rebuild (retry) and rm.

## Step 6: the TUI runs verbs detached, with a build-log key

- `run_suspended` (`src/tui/mod.rs:610`) keeps `exec`, `rm`, `stop`,
  `services rebuild`. `run`, `rebuild` and `start` instead spawn
  `devsandbox -C <dir> <verb> …` detached: own session (`setsid`, like
  `src/serve/client.rs:327`), null stdin, stdout/stderr to `/dev/null` (the
  child writes its own build log), `GIT_TERMINAL_PROMPT=0`, the runtime
  pinned as in `src/commands/dispatch.rs:1245`. The work survives quitting
  the dashboard. A one-line help-bar status ("run foo: started"), then an
  immediate snapshot refresh (`followup`). A non-zero exit shows a help-bar
  line pointing at the build-log key. Prompts can't happen: the TUI always
  passes the sandbox, and `rebuild`'s `--force` is passed through.
- A key on an instance row (pick a free one, check the `?` help modal and
  `tui/app/actions.rs`) opens the build log in a `TextModal` (like
  `open_logs_for`, `tui/app/view.rs:477`), re-read on each snapshot while
  the instance is in flight. The I/O stays off the `App` state machine, the
  way other modal data is loaded.
- Remove `log_error`'s `run`/`rebuild`/`start` uses (`src/tui/mod.rs:669`)
  if nothing else needs them.
- `docs/tui-*.md` / site TUI docs: document the key and the
  non-suspending verbs.

## Step 7: docs and changelog

- `docs/client-server.md`: mark rollout 1–2 done, and fold in the
  decisions that differ (pre-container failure rolls back, resume =
  `rebuild`, interrupted derived at read time).
- `CHANGELOG.md` `## Unreleased`: user-facing entries (no half-built
  sandboxes, cancel rolls back, `failed` status, build logs + `logs
  --build`, TUI runs in the background).
- `skills/config-toml-spec/SKILL.md`: no change expected (no config field
  changes). Confirm.
