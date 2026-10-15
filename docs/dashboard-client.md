# The dashboard as a pure client

Status: done (steps 1–9). Rollout step 4 of `docs/client-server.md` (read
its *The dashboard as a pure client*, *Config roots*, *Machines* and
*Rollout* first). Rollout steps 1–3 are built: `docs/instance-phase.md`,
`docs/verbs-as-requests.md`. The reference is `docs/api.md`, *The
dashboard as a client*.

As built, where it differs from the steps below (the *Decisions* and
F1–F16 are current otherwise; the steps are kept as the record):

- Off unix, `mod tui` is gated in `src/main.rs` (`#[cfg(unix)]`), not with
  cfgs inside `src/tui/mod.rs`; the Inbox clock helpers `devsandbox inbox`
  shares (`short_age`, `local_utc_offset`, …) moved to
  `src/inbox/clock.rs`.
- The CLI subcommand is `service rebuild` (singular); the method is
  `services.rebuild`.
- `sandboxes.get` answers `config::SandboxText`, `instances.buildLog`
  `build_log::Tail`; the dashboard keeps them as received.
- The poll worker (`src/tui/poll.rs`, procs only since
  `docs/before-machines.md` step 7) never lazy-starts a daemon: it waits
  up to 5 s for one, and `src/tui/daemon.rs` is the one that starts it
  (and waits for a handoff's successor). `daemon.rs` also holds the
  snapshot watch: a lost connection marks the header stale and owns the
  disconnected status line.
- `PendingSignal` carries the instance name, not the container.
- `s`'s `stopping`/`starting` guards clear on the verb's answer.
- `after_start` runs on a snapshot showing the instance running with no
  phase (`App::run_after_start`).
- An instance terminal's argv is `<exe> -C <root> exec -i -t --
  <instance> [cmd…]`; the exe is `current_exe()` taken at dashboard start
  (`App::exec_cli`).
- `open_started_terminal` (step 7) and `log_error` (step 8) were removed:
  nothing used them once the verbs and `:exec` stopped running in the
  dashboard's process.

Known limitations:

- **F16**: the dashboard no longer runs an autostart pass, so opening it
  doesn't autostart a config root with no recorded instances (the daemon's
  startup pass only knows recorded roots).
- **F14**: a detached `s` start that fails after it's registered only shows
  in its build log (`b`) and the row's status; its waiting thread action
  never fires.
- Quitting the dashboard during a held `stop`, `rm` or `services.rebuild`
  used to hang up = SIGINT it (a `stop` with services takes ≈20 s on
  alpine). Fixed (`docs/before-machines.md`, step 2): the dashboard sends
  them with `detach`, so a quit leaves them running.
- Two dashboards each polled the daemon, which ran the runtime calls per
  request. Fixed (`docs/before-machines.md`, steps 6–7): the dashboard
  watches its root and the daemon's one poll pushes the snapshot to every
  watcher; only `instances.procs` stays per dashboard.
- The Inbox is still read from the local store file (and written to it
  while disconnected) until federation (rollout step 5); VS Code (`o`,
  `:code`, a thread's `vscode`) reads `state.toml` (rollout step 7).
- `current_exe()` is captured at dashboard start, so replacing the binary
  while it runs (an upgrade) makes new terminal tabs fail until restart
  (on Linux the path gains " (deleted)").
- A header shows `(stale Ns: …)` as soon as the daemon connection drops,
  even under the 10 s `STALE_AFTER_SECS`.
- `d` ignores modifiers, so Ctrl-D on an instance removes it without a
  prompt (pre-existing, not fixed).
- A lazily started daemon appears to keep the starter's inherited non-stdio
  fds open (seen as a smoke-test pipeline that didn't end until the daemon
  was killed). Pre-existing, unverified.
- Service terminals still run a direct `<runtime> exec -it`; one terminal
  path for instances and services is for later (user's note).
- `services.rebuild` sends no `progress` (it has no instance phase).

## Problem

The dashboard still reads `state.toml`, the config files and the runtime
itself: it runs `snapshot::collect_with` every 2 s (`src/tui/mod.rs:1128`),
`proc_list` per expanded instance (`:1089`), bare `docker stop`/`start` for
`s` (`:959`/`:992`), suspends for `rm`/`stop`/`services rebuild` (`:661`),
and the "I/O-free" `App` calls the runtime and loads the config in its key
handlers (`src/tui/app/view.rs:687`, `:769`, `:795`, `:859`,
`src/tui/app/command_line.rs:161`, `src/tui/app/terminal.rs:402`/`:430`).
Two dashboards double the runtime load, and a remote machine (step 5) can't
be reached that way. After this step the dashboard reads everything
through its daemon and sends every action as a request; a remote instance
becomes just another row.

## Decisions

Settled with the user (don't relitigate). From earlier steps, still in
force: no job/task concept anywhere user-facing; the TUI always detaches
`run`/`rebuild`/`start`; Ctrl-C/`d` = cancel; any failure keeps `failed`;
the client sends an absolute root, never a cwd; env per request = the
caller's whole env; one connection per verb request; no daemon → the CLI
warns and runs in-process, **the TUI has no fallback**.

New for this step:

- **Snapshot**: `instances.list` gains a `depth` param (`disk` | `listing`
  | `full`; absent = `full`, today's). The client polls at today's cadence
  (2 s listing, stats at most every 7 s: `StatsClock`), on a connection of
  its own so a 1.5 s stats call never stalls other calls.
  `instances.changed` also fires when `state.toml` changes.
- **Procs**: `instances.procs {instances: […]}`, polled by the client at
  `PROC_TICK`: per instance its parsed process rows + `signalable` + runs,
  or an error. `instances.signal {instance, pid, signal}` for the kill keys.
- **Config view and `:` completion**: `sandboxes.get {root, name}` →
  original/resolved TOML text + hash, `services.get {root, name}`.
  Completion's sandbox names and `worktree-branch` come from the
  snapshot's sandbox rows. The TUI never loads a `Config`.
- **Logs, inspect, build log**: `containers.logs {container, lines}`,
  `containers.inspect {container}` (the Services tab uses them too),
  `instances.buildLog {instance, lines}` re-polled while `b` is open
  (camelCase like the rest of the API, not the design doc's
  `instances.build_log`).
- **`s`**: through the `start` (detached) / `stop` verbs; `spawn_stop` /
  `spawn_start` go. Behaviour change, accepted: `s` start runs
  `postStartCommand` + the autostart pass like CLI `start` (today a bare
  `docker start`). A thread action waiting on the start runs when a
  snapshot shows the row running.
- **`stop`, `rm` (and `services rebuild`) are held requests** on their own
  connection; quitting the dashboard mid-op hangs up, which leaves them
  running: they're sent with `detach`, which for a held verb only waives
  the hang-up's cancel (`docs/before-machines.md`, step 2). The answer or error (rm refusing a dirty worktree) is the
  status line.
- **`rm`**: sent with `ask: true`; a `confirm` answer opens the existing
  Confirm modal with rm's question: `y` resends with `deleteBranch: true`,
  `n` with `false`, `esc` aborts. No up-front confirm on `d` (as today).
- **Inbox: unchanged this step.** The dashboard keeps reading the store
  file and its disconnected local-write fallback. The App works on the
  store model; remote threads are a step-5 federation question.
- **Daemon down**: the last snapshot stays, marked stale in the header
  with the reason; actions refuse with a status line. Never connected: an
  empty tree and "needs devsandbox serve: …".
- **Terminal tabs**: the PTY runs `devsandbox exec <instance>` (current
  exe) instead of a hand-built `docker exec -it`: the CLI's `exec` already
  does the argv, the in-flight guard and `bridges.ensure`, and step 5 only
  prefixes `ssh -t`. Services keep a plain `docker exec -it`. **Later
  (investigate, not now)**: one terminal path for instances and services
  alike (user's note).
- **The rest**: `rename` and done/undone become methods
  (`instances.rename`, `instances.done`); **`services rebuild` becomes a
  verb request**; VS Code (`o`, `:code`, a thread's `vscode`) stays local
  (it reads `state.toml`; remote VS Code is step 7); the dashboard drops
  its own startup autostart pass.
- **Off unix the dashboard only says it needs devsandbox serve** (Windows
  becomes remote-only, `docs/windows-support.md`); the off-unix paths go.

Mine, flagged for review (say so if any is wrong):

- **F1 `depth` values**: `"disk"` (state + config, no runtime),
  `"listing"` (+ `ps` + version), `"full"` (+ `stats`), `snapshot::Depth`
  as is. Unknown value: `invalid`.
- **F2 connections**: a new `src/tui/poll.rs` worker owns two connections
  of its own (lazy start like `daemon.rs`'s `open`, no subscribe): one for
  `instances.list`, one for `instances.procs` (a docker exec per instance,
  seconds on a busy box). Each kind keeps today's at-most-one-in-flight
  rule. One-shot calls (logs, inspect, config, build log, signal, rename,
  done) go through `src/tui/daemon.rs`'s long-lived connection as new
  `Cmd`s, the `Cmd::Cancel` pattern: they're quick. Verbs keep their own
  connection each (`spawn_verb`).
- **F3 `thread_children` in `instances.list`**: the Inbox thread pane's
  owner id → key → child name map (`dispatch::thread_children`,
  `src/tui/mod.rs:1133`) becomes a top-level `thread_children` field of
  `instances.list`'s result (snake_case like the snapshot's fields), not of
  `status --json`.
- **F4 sandbox rows grow**: `SandboxRow.dispatcher` is serialized (it's
  `#[serde(skip)]` now) and `worktree_branch: Option<String>` is added
  (the resolved `worktree-branch`, for `--branch` completion). `status
  --json` / `ls --json` gain both (additive; `docs/json-output.md`).
- **F5 snapshot types deserialize**: `Snapshot`, `InstanceRow`,
  `SandboxRow`, `ServiceRow`, `ContainerStatus`, `PhaseRow` get
  `Deserialize`; `&'static str` fields (`runtime_name`, `ServiceRow.scope`)
  become `String`; `collected_at` defaults to when it was received, `stats`
  is set by the client from the depth it asked for. Same JSON out.
- **F6 state watch**: `serve-watch` (`src/serve/daemon.rs:593`) also
  stamps `state.toml` (mtime + size, as `ops::stamp` does the store) each
  tick and publishes `instances.changed` when it moved. Phase changes of a
  build then reach the dashboard within the tick, not the 2 s poll.
- **F7 procs wire**: per instance either `{"processes": [{"pid", "ppid",
  "args"}], "signalable": bool, "runs": [{"id", "state", "started",
  "argv"}]}` or `{"error": "…"}`, keyed by instance name. The parsing
  (`parse_top`, `src/tui/procs.rs`, and the `devsbd run ls` line split in
  `run_rows`) moves daemon-side into a shared pure module
  (`src/runtime/procs.rs`); the TUI keeps `build_forest` and the row
  formatting. Instances resolve to containers from `state.toml`; one that
  isn't running is an `error` entry, not a failed call.
- **F8 `containers.*` take only `devsandbox-` names** (`NAME_PREFIX`), so
  the API is no generic docker proxy. `containers.inspect` answers the
  parsed inspect JSON; the TUI pretty-prints it (`pretty_inspect`).
- **F9 `instances.buildLog`**: `{instance, lines}` (`instance`: exact name
  or id, as `instances.cancel`), answer `{"text", "cut"}` or `null` when it
  has none (`build_log::tail`).
- **F10 `sandboxes.get`** answers `{"original", "resolved", "hash"}`; a
  sandbox that doesn't resolve answers its error in `resolved` (the view
  shows it, as today), an unknown name is `not-found`, a config that
  doesn't load is `invalid`. `services.get` answers `{"original"}`.
- **F11 `instances.signal`**: `{instance, pid, signal: "TERM" | "KILL"}`;
  pid all digits; the daemon resolves the container from `state.toml` and
  runs `backend().signal_proc`.
- **F12 `instances.rename {instance, newName}`** → `{"instance": new}`
  (`rename::rename_exact`); **`instances.done {instance, done}`** →
  `{"instance", "done"}` (`done::set_saved`). The TUI words the status line.
- **F13 `services.rebuild` verb**: params `root`, `env`, `name`; no
  `detach`; same subprocess path as the five verbs (`--in-process -C <root>
  service rebuild -- <name>`, the CLI spells it singular). The CLI's `service rebuild` goes through
  `verbs_client` too (in-process fallback as the others), so "a verb
  request" means the same thing everywhere.
- **F14 detached `start` reports little**: it answers right after the
  spawn, so a start that fails later only shows in its build log (`b`) and
  the row's status. The waiting thread action (`after_start`) simply never
  fires then; the next `s`/action replaces it.
- **F15 `:exec` (suspended) also runs `devsandbox exec`** as a child with
  inherited stdio rather than `commands::exec::exec_status` in-process, so
  the dashboard reads no state for it either.
- **F16 autostart gap**: dropping the dashboard's startup pass means a
  root with no recorded instances isn't autostarted by opening the
  dashboard (the daemon's startup pass only knows recorded roots). Not
  fixed here; noted under *Known limitations* when built.

## Wire (what steps 2–4 add to `docs/api.md`)

| method | params | result |
|---|---|---|
| `instances.list` | `dir`, `depth?` | the snapshot + `thread_children` |
| `instances.procs` | `instances: [name…]` | `{name: {processes, signalable, runs} \| {error}}` |
| `instances.signal` | `instance`, `pid`, `signal` | `{"ok": true}` |
| `instances.buildLog` | `instance`, `lines` | `{text, cut}` \| `null` |
| `instances.rename` | `instance`, `newName` | `{instance}` |
| `instances.done` | `instance`, `done` | `{instance, done}` |
| `containers.logs` | `container`, `lines` | `{text}` |
| `containers.inspect` | `container` | the inspect JSON |
| `sandboxes.get` | `root`, `name` | `{original, resolved, hash}` |
| `services.get` | `root`, `name` | `{original}` |
| `services.rebuild` (verb) | `root`, `env`, `name` | as the verbs |

`instances.changed` now also means "`state.toml` changed".

## Constraints (every step)

- Mimic surrounding code. Doc comments say why. Tests in `#[cfg(test)]`
  modules; pure logic testable without docker; docker tests use
  `#[test_utils::docker_test]`. No test may touch the user's `state.toml`,
  daemon or containers: scratch dirs and the existing helpers
  (`serve::daemon::tests::{scratch, opts, spawn_daemon}`, handlers with
  injected `state` paths as `instances_cancel` at `src/serve/api.rs:389`).
- `cargo test --workspace` passes at the end of every step.
- Protocol additive (`PROTOCOL` stays 1). Every new wire struct is in
  `npm/devsandboxes/index.d.ts` (`ApiMethods`, interfaces) and checked by
  `src/serve/dts.rs`. Each step documents its methods in `docs/api.md`.
- `state.toml` writes only through `State::update` (via the existing
  command fns).
- Handlers in `src/serve/api.rs` stay socket-free, take injectable paths
  / seams, and answer explicit wire structs.
- The `App` stays I/O-free: it queues requests (`take_pending_*`) and gets
  answers through setters; the event loop and workers do the I/O.
- Nothing the dashboard does may write to the terminal (stderr included).
- No git. Never touch `docs/helper-rpc.md` (untracked, not ours).

## Step 1: off unix the dashboard says it needs the daemon

- `src/tui/mod.rs:137` `dashboard`: off unix, bail before `setup()` with
  "the dashboard needs devsandbox serve (unix only for now)". Remove the
  `cfg(not(unix))` arms in `run`/`run_suspended`/`log_error` (`:444`,
  `:464`, `:673`–`:691`) and the ones made pointless; keep the crate
  compiling off unix with the least cfg (the `tui` module is also used by
  `devsandbox inbox` for `short_age`/`local_utc_offset`). `cargo check`
  for a Windows target isn't available here: reason it through.
- Docs: `docs/windows-support.md` (the TUI line), `docs/api.md` *The
  dashboard as a client* if it mentions off-unix behaviour.

## Step 2: the snapshot over the wire (daemon)

- F5 in `src/snapshot.rs` (`:25`–`:212`). F4 in `sandbox_rows` (`:535`);
  `docs/json-output.md` gains the two fields.
- `instances.list` (`src/serve/api.rs:336`–`:352`): `depth` (F1),
  `thread_children` (F3) next to the snapshot's fields (one `State` read
  for both is fine, but `collect_with` loads its own; don't restructure
  `collect_with` for it).
- F6 in `watch` (`src/serve/daemon.rs:593`); `State::path()` for the file.
  Test it like `subscribers_hear_inbox_changes_between_responses` (`:983`)
  with a scratch state path (find how the daemon tests isolate
  `state.toml`; add a seam if there is none).
- A round-trip test: a collected `Snapshot` serializes and deserializes to
  the same JSON.
- `index.d.ts`: `Snapshot`/`SandboxRow` fields, `instances.list` params,
  `thread_children`; `docs/api.md` *instances.list* and the
  `instances.changed` row.

## Step 3: read methods (daemon)

- `instances.procs` (F7): move `parse_top` + the run-line split out of
  `src/tui/procs.rs` into `src/runtime/procs.rs` (pure, tests move with
  them); the handler runs `backend().proc_list` + the `devsbd run ls`
  probe (`src/tui/mod.rs:1067`–`:1083` `run_ls`, `RUN_LS_CAP`/`TIMEOUT`
  move too) per instance. The TUI keeps compiling on the old code until
  step 5 switches it (re-export or call the moved fns).
- `containers.logs`, `containers.inspect` (F8), `instances.buildLog` (F9),
  `sandboxes.get` / `services.get` (F10, the bodies of
  `src/tui/app/view.rs:755`–`:815` `build_sandbox_view`/`build_service_view`
  move daemon-side as pure fns over a `Config`).
- Tests per handler without docker where the logic allows (validation,
  not-found, the pure parts); a `#[test_utils::docker_test]` for procs/logs
  on a throwaway container if cheap.
- `index.d.ts` + `docs/api.md` for each.

## Step 4: mutations and the `services.rebuild` verb (daemon + CLI)

- `instances.signal` (F11), `instances.rename`, `instances.done` (F12) in
  `src/serve/api.rs`; `rename_exact` / `set_saved` take the global state
  today: give them (or a thin wrapper) an injectable state path for the
  tests, without changing their CLI behaviour.
- `services.rebuild` (F13) in `src/serve/verbs.rs`: `Verb::ServicesRebuild
  { name }`, params struct, `argv` (+ the parse-with-the-real-`Cli` test,
  `:985`), `detach_at` → `Never`, `is_verb`. CLI: `CliVerb` in
  `src/serve/verbs_client.rs:53`–`:95` and the `from_command` mapping, so
  `devsandbox service rebuild` streams through the daemon.
- `index.d.ts` + `docs/api.md` (*Verbs* table, *The CLI as a client*).

## Step 5: the dashboard reads the snapshot and procs from the daemon

- New `src/tui/poll.rs` (F2; add to the `AGENTS.md` module map): a worker
  with `request_snapshot(depth)` / `request_procs(names)` and a
  `try_recv`. Replace `spawn_collect`/`spawn_collect_with`/`Collected`
  (`src/tui/mod.rs:1114`–`:1140`) and `spawn_proc_fetch`/`run_ls`
  (`:1067`–`:1112`); keep the startup Disk → Listing → Full walk, the
  `StatsClock`, `FollowUp` and `procs_after_snapshot` logic, now choosing
  the `depth` sent. `snapshot.stats` from the depth asked.
  `instances.changed` keeps scheduling the follow-up.
- `proc_fetch_targets` (`src/tui/app/procs.rs:149`) can send names only;
  `apply_proc_fetch` builds the forest from the wire rows
  (`build_forest`, `procs::run_rows`' formatting on the structured runs).
- Daemon down (Decisions): keep the last snapshot; the header's stale
  marker (`src/tui/data.rs:167` `totals_line`) carries the reason; never
  connected: empty tree + status. `thread_children` from the result (F3).
- `:` completion (`src/tui/app/command_line.rs:155`–`:178`, `:231`–`:255`):
  sandbox names and `worktree_branch` from `self.snapshot`'s sandbox rows;
  drop `Config` from `candidates_for` (tests at `:618` adapt).
- Drop the startup `autostart` call (`src/tui/mod.rs:140`).
- After this step `src/tui/mod.rs` no longer calls `data::collect_with`,
  `State::load` or `backend()` for the tree or procs.

## Step 6: modals and one-shot actions through the daemon

- New `Cmd`s in `src/tui/daemon.rs` (pattern: `Cmd::Cancel`, `:94`/`:537`)
  answering with `DaemonUpdate`s the loop hands to `App` setters.
  Disconnected: a status line ("needs devsandbox serve: …"), as `cancel`.
- Logs modal (`src/tui/app/view.rs:686` `open_logs_for`): opens at once
  with "(loading…)", queues the request, filled by a setter (ignore an
  answer for a modal no longer open).
- Config view (`:701` `open_config`): queues `sandboxes.get`/`services.get`
  and `containers.inspect` for the inspect pane (`:853` `set_inspect`).
- Build log (`src/tui/mod.rs:409`, `take_build_log_read` →
  `instances.buildLog`, `set_build_log` unchanged in shape).
- Process signal (`spawn_signal`, `src/tui/mod.rs:1051`) →
  `instances.signal` (instance name instead of container in
  `PendingSignal`, `src/tui/app/procs.rs:41`).
- `:rename` (`src/tui/mod.rs:360`) → `instances.rename`; done/undone
  (`spawn_done`, `:1035`) → `instances.done`, keeping `PendingDone.report`.
- Afterwards `src/tui/app/` has no `backend()` and no `Config::load`.

## Step 7: actions as requests

- Generalize `spawn_verb` (`src/tui/mod.rs:896`) / `verb_call` /
  `verb_status` to `stop`, `rm` and `services.rebuild` (held, F13/Decisions;
  their answers are status lines; `failed` quotes the verb's error).
- `s` (`src/tui/app/actions.rs:128`): `pending_stop`/`pending_start`
  become `stop` / detached `start` verb requests; `stopping`/`starting`
  guards clear on the answer. Delete `spawn_stop`/`spawn_start`
  (`src/tui/mod.rs:959`–`:1030`) and their drain loops. `after_start`
  (`src/tui/app/thread_actions.rs:119` `start_finished`) runs when a
  snapshot shows the instance running with no phase (F14); test it.
- `rm` (`d`, `:rm`): `ask: true`; a `confirm` error opens
  `Modal::Confirm` (`src/tui/app/view.rs:262`–`:271`) with a new
  `Confirmed` variant: `y` → `deleteBranch: true`, `n` → `false`, `esc`
  → nothing. `ConfirmModal`'s key handling (`:510`) grows the `n` case for
  it. The CLI's loop is the model (`src/serve/verbs_client.rs`
  `with_confirm`).
- `run_suspended` (`src/tui/mod.rs:661`) keeps only `Exec` (step 8);
  `log_error` goes if nothing else uses it.

## Step 8: terminal tabs and `:exec` run `devsandbox exec`

- `src/tui/app/terminal.rs:394`–`:430`: for instances the argv is
  `[current_exe, "-C", dir, "exec", "--", <instance>]` (check how `exec`
  with no command picks `SHELL_FALLBACK_CMD` and its TTY flags under a
  PTY); `App` gets the exe path as a field set by the loop, so it stays
  I/O-free. Drop the `State::load`, the `no_container_yet` check (the CLI
  prints it in the tab) and `pending_bridges` + its loop code
  (`src/tui/mod.rs:457`–`:465`, `DaemonWorker::ensure_bridge` if unused).
  Services keep `[backend bin, "exec", "-i", "-t", container, shell…]`
  (the runtime binary name may come from the snapshot's `runtime_name`).
- `:exec` (F15): `run_suspended`'s `Exec` arm spawns the same command with
  inherited stdio and waits.
- Docs: `docs/tui-terminal.md` where it describes the argv/bridge.

## Step 9: docs

- `docs/client-server.md`: status line, *The dashboard as a pure client*,
  *Rollout* step 4 done. `docs/api.md` *The dashboard as a client*
  rewritten. `docs/serve.md` *Future work*: drop the "collects its own
  snapshot" trade-off. `CHANGELOG.md` `## Unreleased`: the user-visible
  changes (`s` start runs lifecycle commands, rm's question as a modal,
  `services rebuild` through the daemon, the dashboard needs the daemon,
  off unix it's unavailable). `AGENTS.md` module map (`poll.rs`,
  `runtime/procs.rs`, the new methods). `site/` pages that describe the
  dashboard's suspended `rm`/`stop` or local collection (grep; `cd site &&
  pnpm check && pnpm build && pnpm test`).
- This doc: "As built" + *Known limitations* at the top (F16 at least).
