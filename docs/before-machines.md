# Before machines: shared poll, upgrades, held verbs, build log

The pre-tasks of `docs/client-server.md` rollout step 5 (machines). Steps
1–4 of the rollout are done (`docs/instance-phase.md`,
`docs/verbs-as-requests.md`, `docs/dashboard-client.md`). This doc covers
four things the user found, the shared poll first among them: the user
asked for it before machines.

1. **Binary upgrades** break the dashboard's terminals (and, briefly, an old
   daemon's verbs).
2. **Quitting the dashboard** interrupts its `stop` / `rm` / `service rebuild`.
3. **The build log modal** has no colour and doesn't scroll with the wheel.
4. **Shared poll**: dashboards register as observers. The daemon polls for
   them and pushes the snapshot.

## Decisions (the user's)

- **Upgrades**: new spawns run the *new* binary at the same path
  (re-resolved per spawn, ` (deleted)` stripped). The version stamp a
  process reports in `hello` stays its own binary's, so an old process never
  claims to be new.
- **Held verbs**: hang-up = cancel stays the CLI's Ctrl-C semantics; the
  TUI's held verbs must survive a quit.
- **Colour**: the user meant the old dashboard's suspended `run` in their
  terminal. They miss docker build's progress most. Approach: **a pty**,
  so the build behaves as on a terminal; the log is replayed through vt100
  for reading. Mouse-wheel scrolling in the modal too.
- **Shared poll**: the daemon polls while **at least one** dashboard
  watches a root. **Push**: `instances.watch {dir}` on the subscribed
  connection, `instances.snapshot` notifications (coalesced, latest wins per
  root), the daemon owns the cadence (2 s listing, 7 s stats, the startup
  Disk → Listing → Full walk, cpu/mem carry-over). **One loop**: `host.rs`'s
  poll lists every container once per tick and feeds the bridges, the
  forwards and the snapshots from that one listing.
- `instances.list` stays uncached for the CLI / npm. Procs
  (`instances.procs`) stay per request, per dashboard: they're per expanded
  instance, and a dashboard expands its own.

## Flagged (mine; say if you disagree)

- **F1** One `exe` helper module (`src/exe.rs`): `devsandbox()` = the path
  to spawn (current_exe, ` (deleted)` stripped, re-resolved each call);
  `own_build_stamp()` = this process's binary's mtime, read **once** at
  first use (Linux: `/proc/self/exe` metadata, which still sees the old
  inode; elsewhere `current_exe` at first use; call it early in `main`).
- **F2** Held verbs get an additive `detach` param (`stop`, `rm`,
  `services.rebuild`). For them it means only "not cancelled when the client
  leaves": they still answer at exit (`DetachAt::Never`, `Left::Detached`
  on a hang-up). The TUI always sends it, the CLI never does.
- **F3** The pty lives **in the verb process**, not the daemon: the build
  log's tee (`build_log::imp::Tee`) opens a pty instead of pipes, puts the
  slave on fds 1/2, and copies the master to the original fd 1 and the log.
  If the daemon held the master, a daemon exit (handoff, drain) would hang
  up the pty and kill the verb, breaking "a daemon exit leaves verbs
  running". The tee uses a pty when fd 1 is a terminal (its winsize), or
  when `DEVSANDBOX_TTY=<cols>x<rows>` is set (the daemon sets it for a
  request with `tty`). A side effect: an in-process `devsandbox run` in a
  terminal gets its colour back too (today's pipe tee drops it).
- **F4** Verb requests get an additive `tty: {cols, rows}` param. The
  daemon passes it to the subprocess as `DEVSANDBOX_TTY`. The dashboard
  always sends its terminal's size. The CLI sends one when its stdout is a
  terminal and it isn't `--json`, so streamed output is coloured on a
  terminal and clean in a pipe.
- **F5** The build log records its pty size in the header
  (`# tty <cols>x<rows>`). Readers replay it through `vt100` at that size,
  with scrollback, so buildkit's cursor-up redraws come out as the final
  screen and not as noise. `instances.buildLog` gains `ansi: true`, which
  returns lines with SGR escapes only, rendered by vt100; without it, the
  lines are plain text (npm, old clients). `logs --build` prints the raw
  bytes to a terminal and the plain replay otherwise.
- **F6** The modal parses SGR into ratatui spans (a small parser, or vt100
  per line). Selection keeps working on the plain text.
- **F7** Wheel scrolling for every text modal (`Logs`, `BuildLog`, `Help`)
  through the existing `scroll_by` (3 lines, like the config view).
- **F8** The cache is keyed by config root, and a row set can be tagged
  with a machine later (step 5): the local daemon will merge a remote
  machine's `instances.snapshot` into it.
- **F9** `instances.watch` / `instances.unwatch {dir}` are methods. They
  are released on hang-up and need a `subscribe` to `instances` first,
  since the pushes are notifications. The dashboard re-watches on
  reconnect. Watching a root the cache already holds pushes the cached
  snapshot at once.
- **F10** The autostart gap (a root with `autostart` sandboxes but no
  instance isn't autostarted by opening the dashboard) is **not** fixed
  here. `instances.watch` is the hook for it later (*After step 5*).

## Step 1: run the current binary after an upgrade

The problem: `std::env::current_exe()` is captured once, or called after
the file was replaced. On Linux that returns `<path> (deleted)`, so spawns
fail until a restart.

- New `src/exe.rs` (F1), declared in `src/main.rs`. Use it at:
  `src/tui/mod.rs:225` (`ExecCli`: store the helper's result per spawn, or
  resolve in `ExecCli::argv`'s caller. `ExecCli.exe` is used by
  `src/tui/app/terminal.rs:30` and `run_suspended` ~:606. Re-resolve at
  each spawn, not once at startup), `src/serve/verbs.rs:391`
  (`Runner::new`: `program` must re-resolve per spawn in production. Keep
  tests' injected `program` working, e.g. an enum or an `Option` meaning
  "the devsandbox binary"), `src/commands/dispatch.rs:1232`,
  `src/serve/client.rs:393` (`spawn_detached`), `src/serve/proto.rs:131`
  (`exe_mtime` → `own_build_stamp`, cached).
- Leave `src/serve/install.rs:111` alone (one-shot `serve install`, writes
  the path into a unit file). Leave the test at `src/commands/run/mod.rs:1490`
  alone too.
- Strip only a trailing ` (deleted)`, and only if the stripped path exists.
  Otherwise return the path as is, so the error names it.
- Tests: the stripping as a pure fn; the stamp is stable across calls.
- Smoke test: start a dashboard in a smoke dir, `cp` a rebuilt binary over
  the path (a new inode, like an upgrade or `cargo build`), then open a
  terminal tab and check it works.

## Step 2: held verbs survive a dashboard quit

- `src/serve/verbs.rs:106` `StopParams`, `:117` `RmParams`, `:138`
  `ServicesRebuildParams`: add `#[serde(default)] pub detach: bool`, and
  pass it through `parse` (`:226`–`:238`; today it's hard-coded `false`).
  `detach_at` (`:337`) keeps `Never` for them. The loop's hang-up
  (`:607`) and `Left::after_write` already turn `req.detach` into
  `Left::Detached`. Fix the doc comment at `:136`, which says "No
  `detach`".
- Tests: update `params_are_checked` (`:1089`–`:1091`, which asserts that
  services.rebuild ignores detach) and `detach_answers_at_registration_or_spawn`.
  Add a test that a detached held verb whose client hangs up isn't SIGINTed,
  modelled on `verbs_stream_over_the_socket_and_a_hang_up_cancels`
  (`src/serve/daemon.rs:1321`).
- `src/tui/mod.rs:743` `verb_call`: `detach: true` for stop/rm/services.rebuild.
  Fix the doc comments (`:735`–`:742`, `spawn_verb` `:845`–`:852`,
  `verb_held` `:729`).
- `src/serve/verbs_client.rs` (CLI): `detach: false` for them (they have
  no `-d`).
- rm's `confirm` round must still work: the `ask` request answers `confirm`
  at exit, which is unchanged.
- Docs: `docs/api.md` *Verbs* (the `detach` row/paragraph), the
  `npm/devsandboxes/index.d.ts` params (checked by `src/serve/dts.rs`), and
  `docs/dashboard-client.md` if it says quitting cancels.
- Smoke test: a sandbox with a service (stop takes ~20 s on alpine). `s` to
  stop it, `q` at once, then check the instance ends up stopped
  (`devsandbox ps`) and that `serve.log` says "left running".

## Step 3: wheel scrolling in text modals

- `src/tui/app/mod.rs:594` `route_mouse`: today only `Modal::Config` gets
  the wheel. Route `ScrollUp` / `ScrollDown` to `Modal::Logs`,
  `Modal::BuildLog` (its own `scroll_by`, `view.rs:272`, which keeps `stick`
  right) and `Modal::Help`'s text, ±3 (F7).
- First, reproduce the keys while a build is live. If a re-read
  (`set_build_log`, `view.rs:649`) moves the view when the user has
  scrolled up (`stick == false`), keep the user's place (e.g. keep `top`
  relative to the bottom when the 500-line window slid). Otherwise leave it
  alone and say so in the review notes.
- Tests in `view.rs` / `mod.rs` test modules, with the `test_support.rs`
  fixtures.

## Step 4: verbs on a pty (producer side)

- `src/build_log.rs` `imp::Tee::start` (~:300): pty mode (F3). Open a pty
  (`libc::openpty`, or `portable-pty`, which is already a dependency; prefer
  libc for fd-level work like the rest of `imp`) sized from fd 1's winsize
  when it's a terminal, else from `DEVSANDBOX_TTY`. dup2 the slave onto 1
  and 2 and close the original slave fd. One copier thread reads the master
  and writes to the saved fd 1 and the log (`copy`, `:212`, unchanged). On
  drop: restore fds 1/2 (closing our slave copies), then drain the master
  until EIO / EOF. A grandchild still holding the slave keeps it open, so
  the `StopRead` stop flag must still bound the wait. Linux returns EIO,
  not 0, on a master whose slave is closed: treat it as EOF. The pipe mode stays for the
  no-terminal, no-env case. Keep stdin as it is.
- The header (`header`, `:142`) gets `# tty <cols>x<rows>` in pty mode (F5).
- `error_message` (`src/serve/verbs.rs:933`) reads the stderr spool. Check
  that `main`'s `Error:` line still goes to the real stderr after
  `BuildLog::outcome` drops the tee (it should). Then nothing changes there.
- `src/serve/verbs.rs`: `tty: Option<TtySize>` on `RunParams`,
  `RebuildParams` and `StartParams` (F4; stop/rm/services.rebuild don't
  build-log, so skip them). `Running::spawn` (`:668`) sets
  `DEVSANDBOX_TTY=<cols>x<rows>` after `envs(&req.env)`. Add it to
  `VerbRequest`.
- Clients: `src/tui/mod.rs` `verb_call` sends the dashboard's size (pass
  `terminal.size()` in; clamp rows to something sane like 40 if the
  terminal is huge). `src/serve/verbs_client.rs` sends the terminal's size
  when stdout is a terminal and the verb isn't `run --json`. The
  in-process fallback needs nothing: fd 1 is the terminal.
- A docker build under the pty: check that buildkit's TTY progress shows
  in `devsandbox run` on a terminal, and that `devsandbox run … | cat`
  stays plain.
- Docs: `docs/api.md` *Verbs* (`tty`), `index.d.ts`, `docs/instance-phase.md`
  or wherever build logs are described (pty, header).

## Step 5: verbs on a pty (reader side)

- `src/build_log.rs` `tail_at` / `last_lines` (`:95`–`:126`): with a
  `# tty` header, replay the tail bytes through `vt100::Parser` at the
  recorded size with a scrollback of at least the requested lines. Return
  the last N lines as plain text, or with SGR (`ansi`) via vt100's
  formatted row output. Without the header (an old log, or pipe mode),
  keep today's line logic. The 256 KiB tail can start mid-escape; vt100
  copes. The header is in the first bytes, so read it separately.
- `src/serve/api.rs` `instances.buildLog` (~:623–:730): an `ansi: bool`
  param (F5). `src/tui/daemon.rs:402` asks with `ansi: true`.
- `src/commands/logs.rs:28` `logs --build` (and `--follow`): raw bytes to
  a terminal; replayed plain text otherwise.
- `src/tui/app/view.rs` `BuildLogModal` / `set_build_log` and
  `src/tui/ui.rs:2358` `draw_build_log_modal`: render SGR as styled spans
  (F6). Keep `text.body` plain (selection, `line_count`, scroll math) and
  hold the styled lines next to it.
- Tests: replay unit tests (cursor-up redraw ends as one final line;
  colours kept with `ansi`, stripped without), the SGR parsing, and the
  `api.rs` param.
- Docs: `docs/api.md` `instances.buildLog`, `index.d.ts`.

## Step 6: shared poll, daemon side

- New `src/serve/snapshots.rs`: the watchers (connection id → roots), the
  per-root cache (`InstancesList` as JSON, plus what carrying cpu/mem over
  needs), and the cadence as pure fns over `Instant`s (`STATS_INTERVAL` 7 s
  moves here from `src/tui/mod.rs:67`). "Due" decisions are unit-tested
  without a runtime, and the collection is injectable (F8).
- `src/snapshot.rs:615` `collect_with`: split into the runtime half
  (one `list(true, NAME_PREFIX)` + optional `stats()` + version) and a
  per-root join that takes them (e.g. `collect_from(dir, depth, &Listing)`),
  so N roots cost one `ps` (and one `stats`) per tick. `collect_with` keeps
  its behaviour for `instances.list` / `status`.
- `src/serve/host.rs:180` `poll`: the one loop. Each tick lists every
  devsandbox container (`list(true, …)`) and derives the running list from
  it for the bridges and forwards (unchanged behaviour). While anything is
  watched, the tick is 2 s, stats run when due, each watched root is
  joined, and changed snapshots are handed to the daemon to publish.
  Unwatched, it's 5 s as now. An urgent wake (`Waker`) never waits on stats.
  A `state.toml` change (today noticed by `serve-watch`, `daemon.rs:609`)
  should prompt a listing-depth re-collect soon (within ~0.5 s), so a
  verb's phase shows as fast as today's `instances.changed` → re-poll.
  Wire it so the watcher can wake the poll.
- A new watch's first snapshot: cached → pushed at once. Otherwise Disk
  at once (no runtime call), then the loop's next Listing and Full ticks.
  The walk is the daemon's now.
- `src/serve/daemon.rs`: `instances.watch` / `instances.unwatch` (F9),
  handled like `subscribe` (connection-level). Unwatch on hang-up. The
  `Pending` (`:478`) gets latest-wins snapshots per root, written as
  `instances.snapshot {dir, …InstancesList fields}`. Keep
  `instances.changed` as it is. Without a host side (tests, `host: false`)
  watch still answers. Test the pushes with an injected collector.
- `src/serve/api.rs`: `InstancesList` (`:377`) is the payload. Check the
  serialization carries cpu/mem (the dashboard's `stats` flag becomes moot:
  every pushed snapshot is complete).
- Docs: `docs/api.md` (*Instances*, *Notifications*), `index.d.ts` (+ dts
  test), `docs/serve.md` (the poll). `src/serve/idle.rs`: a watcher is a
  connection, so it already holds the daemon. No new holder.

## Step 7: the dashboard as an observer

- `src/tui/daemon.rs` (subscribe at `:533`, reconnect loop `run` `:555`):
  after `subscribe`, `instances.watch {dir}`, again on every reconnect.
  Relay `instances.snapshot` as `DaemonUpdate::Snapshot(InstancesList)`.
  A failed watch is reported like a failed subscribe.
- `src/tui/poll.rs`: drop the list lane (`request_snapshot`,
  `PollUpdate::Snapshot`, `list_call`, `list_answer`). Keep procs.
- `src/tui/mod.rs`: drop `StatsClock`, `refresh`, `refresh_after_change`,
  `FollowUp`, the `deeper` walk and the 2 s `TICK_INTERVAL` snapshot
  requests (`:63`–`:140`, `:244`–`:253`, `:527`–`:579`). Keep
  `procs_after_snapshot`-style behaviour where it matters (proc targets
  come from the snapshot). `App::set_snapshot`'s stats carry-over
  (`src/tui/app/mod.rs:335`) goes or becomes a no-op. Staleness: the
  header's age is time since the last pushed snapshot, and a disconnected
  daemon marks it stale with why (`snapshot_error`, as today). Check
  `src/tui/data.rs:192`'s 10 s threshold against a 2 s push cadence; a
  root whose snapshot doesn't change may not be re-pushed, so either push
  every collection or have the age come from the daemon. Decide and say
  which.
- Fix the module docs (`poll.rs` header, `tui/mod.rs` loop comments,
  `daemon.rs` header) and `AGENTS.md`'s module map (`poll.rs`, `daemon.rs`
  and `host.rs` entries, new `snapshots.rs`).
- Smoke test: two dashboards on one root. Count `docker ps` / `docker
  stats` calls (e.g. a wrapper `docker` on `PATH` that logs) and check
  they're the same as with one. Quit both, and the daemon stops polling.

---

## Carried over from the handoff (for step 5 and after)

**Rollout step 5 (machines)**, after this doc: design in
`docs/client-server.md` *Machines* and *Exec and terminal*: `machine
add|ls|rm` (registry `machines.toml`; `add` without `--config` probes
roots with `machines.probe`, then `machines.add`), the ssh transport over
`ssh <host> devsandbox api --stdio` (`ssh -A`; env from the ssh login
session), federation merging each machine's `instances.list` /
`sandboxes.list` (`my-server/foo-2` names, `unreachable` rows), `ssh -t
<machine> devsandbox exec …` terminals. Open question: whether a default
machine makes `--machine` implicit (Windows). Service tabs run
`<runtime> exec -it` locally, so they won't reach a remote service:
decide or defer. The dashboard only talks to its daemon (`tui/poll.rs`,
`tui/daemon.rs`, `spawn_verb`), so federation can live entirely in the
daemon. `instances.list` takes `dir`, the verbs and `sandboxes.*` take
`root` (protocol 1, no renames). New wire structs go into
`npm/devsandboxes/index.d.ts` (`src/serve/dts.rs`). Off unix the dashboard
isn't built (`mod tui` is `#[cfg(unix)]`); it comes back with machines.
Steps 6–7 after: remote forwards, VS Code spike.

**After step 5**:
- The Inbox store and VS Code's launch (`launch_code`, `src/tui/mod.rs`
  ~:663, reads `state.toml`) are still local to the dashboard. Move them
  behind the daemon.
- The autostart gap (F10).
- The site's `mini-tui.ts` / tests / `MiniTui.astro:353` still model
  suspended stop/rm/exec. `serve-lifecycle.mdx` and
  `docs/high-level-architecture.md` :115/:140/:157/:161 are stale.
- Dead code: `PendingDone.report` (`src/tui/app/actions.rs:42`, `:72`),
  `DaemonUpdate::Done.report` (`src/tui/daemon.rs:100`) and its check in
  `src/tui/mod.rs` ~:454. Also the three `serve::client::ensure_running`
  calls in `src/main.rs` (~:474, ~:494, ~:521).

**Settled earlier, still in force**: no job/task concept in UI or API; no
jobs pane; Ctrl-C = cancel, `-d` detaches, the TUI detaches
`run`/`rebuild`/`start`; failures keep the instance `failed`, only cancel
rolls back; the client sends an absolute root, never a cwd; env per
request = the caller's whole env (remote: the ssh login session's, only the
agent crosses); one connection per verb request; no daemon → the CLI runs
in-process with a warning, the TUI has no fallback; a detached start
failing after its answer shows only in `b` and the row (expected).

**Known limitations carried**: see `docs/verbs-as-requests.md` and
`docs/dashboard-client.md` *Known limitations*; `BUGS.md` (untracked):
Ctrl-D on an Instances row removes the instance; a lazily started daemon
seems to hold inherited fds.

**Working rules**: never stage `BUGS.md`, `docs/helper-rpc.md`. Smoke
tests only in a throwaway dir with its own `XDG_DATA_HOME`,
`XDG_RUNTIME_DIR` (0700, must exist), `XDG_STATE_HOME`, `image = "alpine:3"`
sandboxes and a git repo as `folder`. Clean up afterwards: `rm --force
--delete-branch` each instance, kill the smoke `serve` (`pgrep -af 'serve
--socket-dir <dir>'`), delete the dir. Never touch `../.devsandboxes` or
the user's containers. Drive the TUI with `script -qec "stty cols 160 rows
40; $DS -C <cfg>" out.txt`, bash `printf`, a trailing `sleep`, and an
explicit `q` (when the pipe closes, `script` sends Ctrl-D, which removes
the selected instance). F12 (`\x1b[24~`) leaves a terminal tab.
