# Verbs as requests: `run`, `rebuild`, `start`, `stop`, `rm` through the daemon

Status: plan. Rollout step 3 of `docs/client-server.md` (read its
*Requests*, *Execution, internally*, *Failure* and *Handoff* first). Steps
1–2 are built in-process: `docs/instance-phase.md`. `services *` and the
other verbs, the dashboard as a pure client (rm/stop included) and machines
come after, in their own plans.

## Problem

Every verb runs in the process that was started. The dashboard works around
it by spawning detached `devsandbox` processes (`src/tui/mod.rs:846`
`spawn_detached`) and cancels them by signalling a pid it reads from
`state.toml` (`:917` `spawn_cancel`). Nothing owns the work, the caller's
env is whatever the process inherited, and a remote client (step 5) has no
way in. This step makes the five verbs RPC methods of `devsandbox serve`,
run by the daemon as subprocesses, with the CLI and the TUI as clients.

## Decisions

Settled with the user (don't relitigate):

- **No job/task concept anywhere user-facing.** A verb is an RPC method. The
  daemon's in-memory table of what's in flight is internal.
- **One connection per verb request, answered in order** like every other
  request (`docs/api.md`, *Framing*: unchanged). The connection is held while
  the verb runs; nothing else is read from it meanwhile, except that a
  hang-up is noticed.
- **Hang-up = cancel** (unless `detach` and already answered): the daemon
  SIGINTs the verb's process group. A second SIGINT kills (the verb's
  `cancel.rs` guard restores the default disposition after the first).
- **Ctrl-C in the CLI**: the first opens a second connection and sends
  `instances.cancel {instance}`, then keeps streaming (the rollback's output
  shows); the second hangs up (→ second SIGINT → kill). Before the instance
  is known (a `run` still naming it), the first Ctrl-C hangs up.
- **`-d` / `detach: true`** for `run`, `rebuild`, `start`: the response
  comes once the instance is registered. `-d` prints the instance name;
  `run -d --json` prints the run record of the in-flight instance (phase
  included). The TUI always detaches.
- **Env per request = the caller's whole env.** The subprocess runs with
  `env_clear()` + the request's env + `GIT_TERMINAL_PROMPT=0`, so
  `${localEnv:…}` (`src/config.rs:641` reads `std::env`), `SSH_AUTH_SOCK`,
  `GIT_*`, `DEVSANDBOX_RUNTIME`, `PATH`, `HOME` behave exactly as
  in-process. Never the daemon's own env.
- **Daemon can't start (unix)**: one `warning:` line, then the verb runs
  in-process as today.
- **`stop` and `rm` use the same streamed subprocess path** as the others.
- **The TUI's `rm`/`stop` stay suspended in-process until rollout step 4**
  (rm's branch prompt would need a TUI modal). Only `run`/`rebuild`/`start`
  and the cancel move to requests here.
- Any failure keeps the instance `failed`; only cancel rolls back (step 1–2
  behaviour, unchanged). The client sends an absolute root, never a cwd.

Mine, flagged for review (say so if any is wrong):

- **F1 `--in-process`**: a hidden global flag. The daemon's subprocess, the
  dispatcher's `Subprocess` executor (`src/commands/dispatch.rs:1243`) and
  the client's fallback run the verb with it. In-process mode **never starts
  a daemon**: it skips `serve::client::ensure_running` and its bridge
  requests connect without the lazy start (as `ensure_bridge_running`,
  `src/serve/client.rs:178`), but still report the caller's agent (the
  request env's `SSH_AUTH_SOCK`, fresher than the daemon's). Why: a
  subprocess of a draining daemon must not lazy-start a successor race
  (the reason `ensure_bridge_running` exists), and in the fallback nothing
  answers anyway (each lazy start costs 5 s). A flag, not an env var, so
  host-side commands the verb spawns (`initializeCommand` calling
  `devsandbox`) don't inherit it.
- **F2 typed params, not argv**: each method takes the verb's flags as
  fields; the daemon builds the argv (pure fn, tested by parsing it with the
  real `Cli`, as `src/tui/mod.rs:1230` does today). `root` (as in the design
  doc's example) where `instances.list` says `dir`. `run`'s hidden dispatcher
  flags (`--env`, `--dispatcher`) aren't on the wire: a `run` given them
  stays in-process (only the dispatcher passes them, and it runs in the
  daemon already).
- **F3 output via spool files, not pipes**: the subprocess's stdout and
  stderr go to `<data>/devsandbox/logs/verbs/<pid>.out|.err`, which the daemon
  tails. With pipes, a daemon exiting mid-verb (handoff, crash) would EPIPE
  the verb's `println!`s (a panic) in `stop`/`rm`, which have no tee. A
  successor finds live verbs by those files (step 5).
- **F4 errors carry data**: `ErrorBody` gets an optional `data` (additive).
  New codes: `failed` (the verb exited non-zero; `data: {"exit": n,
  "instance": name|null}`, `message` = its last `error:` line),
  `confirm` (rm needs an answer; `message` = the question), `ambiguous`
  is *not* a code: `instances.resolve` returns every match instead.
- **F5 rm's question moves to the client** through a hidden `rm
  --fail-on-ask`: where `branch_action` says `Ask` (decided before any
  teardown, `src/commands/rm.rs:53`), it exits with code 3 and the question,
  having removed nothing. The daemon answers `confirm`; the CLI asks on its
  TTY and resends with `--delete-branch`/`--keep-branch`. Off a TTY the
  client sends neither and no `--fail-on-ask`: today's behaviour (merged or
  gone branches deleted, local-only work kept).
- **F6 the run pick and the example config**: `run` with no sandbox calls
  `sandboxes.list {root}` and picks on the TTY (else today's error listing
  them). An empty or missing config: the CLI runs `offer_example_config`
  itself (local roots only, it writes into the user's dir).
- **F7 no timeout** on user verbs (the dispatcher's 30 min stays its own).
- **F8 the TUI has no fallback**: with no daemon, `run`/`rebuild`/`start`
  say the daemon is needed on the status line, like an add on the Ports tab.
  `spawn_detached`, `spawn_cancel`, `interrupt` and `detached_args` go.
- **F9 exit codes**: the CLI exits with the verb's `exit` on `failed` (and
  prints nothing more: its stderr already streamed), 130 after a cancel.
- **F10 losing the daemon mid-verb** (handoff, crash): the verb keeps
  running. The CLI says so on stderr; for `run`/`rebuild`/`start` with a
  known instance it follows the build log locally (`commands::logs`'s
  follow) until the instance is no longer in flight, then exits 0 or 1 by
  its phase. `stop`/`rm`: exit 1 after the note. (`instances.follow` for
  remote clients is step 5 of the rollout.)

Parked (not in this plan): autostart's runs on the same subprocess path
(would give them a build log; `src/commands/autostart.rs`,
`src/serve/host.rs` startup pass), `services *`, `rename`/`done`/`gc`.

## Wire (what steps 1–2 add to `docs/api.md`)

```
→ {"id":1,"method":"run","params":{"root":"/home/me/.devsandboxes","sandbox":"web","branch":"x","env":{…},"detach":false}}
← {"method":"progress","params":{"request":1,"instance":"web-2","phase":"preparing"}}
← {"method":"output","params":{"request":1,"stream":"stderr","data":"…"}}
← {"id":1,"result":{"instance":"web-2","exit":0}}
```

| method | params (besides `root`, `env`) |
|---|---|
| `run` | `sandbox`, `name?`, `branch?`, `base?`, `json?`, `detach?` |
| `rebuild` | `instance?` \| `all?`, `force?`, `detach?` |
| `start` | `instance?` \| `all?`, `detach?` |
| `stop` | `instance?` \| `all?` |
| `rm` | `instance`, `deleteBranch?` (true/false/absent), `force?`, `ask?` |
| `instances.resolve` | `{name}` → `{"matches": ["web", "web-2"]}` (`not-found` for none) |
| `instances.cancel` | `{instance}` → `{"ok": true, "pid": n}`; `invalid` when not in flight |
| `sandboxes.list` | `{root}` → `[{"name": "web"}, …]` (objects: step 4 adds fields) |

- `root` absolute (`invalid` otherwise, `not-found` when missing), like
  `instances.list`'s `dir`. `env` required on the five verbs: an object of
  strings (`invalid` when absent: never fall back to the daemon's env).
- `output` `data` is UTF-8 (lossy), chunked as read; `stream` is `stdout`
  or `stderr`. `run --json`'s document arrives on `stdout` like any output.
- `progress` when the verb's instance's phase changes (from `state.toml`;
  `phase` is the `PhaseView` label, or `failed`).
- Detached: `{"instance": name|null, "detached": true}` (+ `"record"`, the
  run record, for `run` with `json`), sent when the instance is registered:
  for `run`/`rebuild`, once an instance's `progress.pid` is the child's pid;
  for `start` and `--all`, right after the spawn; any of them earlier if
  the child exits first (then it's the normal result or `failed`).

## Constraints (every step)

- Mimic surrounding code. Doc comments say why. Tests in `#[cfg(test)]`
  modules; pure logic testable without docker; docker tests use
  `#[test_utils::docker_test]`. No test may touch the user's `state.toml`,
  daemon or containers: use scratch dirs and the existing isolation helpers
  (`serve::daemon::tests::{scratch, opts, spawn_daemon}`, `isolated` in
  `src/commands/run/mod.rs:1479`).
- `cargo test --workspace` passes at the end of every step.
- Protocol changes are additive (`PROTOCOL` stays 1). Every new wire struct
  or notification is in `npm/devsandboxes/index.d.ts` and checked by
  `src/serve/dts.rs`.
- `state.toml` writes only through `State::update`.
- unix-only code behind `#[cfg(unix)]` like the rest of `serve`; off unix
  the CLI runs everything in-process as today.
- No git. Never touch `docs/helper-rpc.md` (untracked, not ours).

## Step 1: the daemon runs verbs

New `src/serve/verbs.rs` (add to the module map in `AGENTS.md`):

- Params structs per verb (table above) and a pure `argv(&VerbRequest) ->
  Vec<String>`: `--in-process -C <root> <verb> --flag=value … -- <target>`,
  the way `src/tui/mod.rs:768` `detached_args` keeps values starting with
  `-` values. Test: every variant parses with the real `Cli` (`src/main.rs:27`).
- Add the hidden global `--in-process` to `Cli` (F1). For now it only has to
  exist and select today's in-process path; step 3 makes it the switch.
  In this step also: with it, `main` skips `ensure_running`, and the bridge
  waits in `src/commands/exec.rs:109` `ready`/`wait` use the no-lazy-start
  connect while still reporting `own_agent()`. Make the dispatcher's
  `Subprocess` (`src/commands/dispatch.rs:1243`) pass it.
- Execution, modelled on `Subprocess::devsandbox` (`dispatch.rs:1231`):
  `current_exe()` (injectable program + leading args, so tests run `sh -c`
  scripts instead of devsandbox), `process_group(0)`, stdin null,
  `env_clear()` + request env + `GIT_TERMINAL_PROMPT=0`, stdout/stderr to
  spool files (F3: create under a temp name, rename to `<pid>.out|.err` after
  the spawn; removed once the verb's exit is reported). A thread per
  request tails both files into `output` notifications and polls
  `state.toml` (≈250 ms, `State::load`) for `progress`. The connection
  thread answers when the verb ends: `{instance, exit: 0}`, else `failed`
  (F4, `data.exit`, `message` = last `error: ` line of stderr without the
  prefix, as `src/tui/mod.rs:906` `last_error_line`).
- Wiring in `src/serve/daemon.rs`: `handle` (`:620`) dispatches the five
  methods to `verbs`, which writes notifications through the connection's
  `Outbox::send` (`:477`, the writer lock keeps lines whole). Hang-up
  detection while the verb runs: poll the socket for readability and
  `recv(MSG_PEEK)`; 0 bytes = hang-up → SIGINT the group; pending data is
  left alone (don't spin on it). **A drain is not a hang-up**: `drain`
  (`:387`) shuts the read side down, which must not cancel anything; check a
  daemon-side `closing` flag first, and let the verb run on (orphaned) when
  the daemon exits. Verb connections don't hold the drain past its usual
  bound.
- Detach (table above): the answer goes out early, the thread keeps
  reaping the child and cleaning up; the client may hang up.
- Idle: a new `Holders` field for verbs in flight (`src/serve/idle.rs:12`;
  `docs/serve.md` *Idle exit*), counted by the daemon's table.
- `ErrorBody.data` (`src/serve/proto.rs:44`), optional, skipped when null.
- Docs in this step: `docs/api.md` (a *Verbs* section: the methods,
  notifications, detach, hang-up, `failed`, holders) and `index.d.ts`.

Tests: argv mapping; a fake program that prints, exits 0/1, sleeps — the
result, `failed` + data, output order per stream, detach early answer,
hang-up SIGINTs the group (the script traps INT and writes a marker), a
drain does not; holders count it. No docker.

## Step 2: resolve, cancel, sandboxes, rm's question

- `src/serve/api.rs` `call` (`:84`): `instances.resolve` (the matching keys
  in `resolve_instance_with`'s tiers, `src/commands/mod.rs:103`: refactor it
  so both share one fn returning the matches), `instances.cancel` (move
  `src/tui/mod.rs:937` `interrupt` to a shared place; read the instance
  fresh and signal only while `PhaseView::InFlight`, as `spawn_cancel` does),
  `sandboxes.list` (config names for `root`).
- `rm --fail-on-ask` (hidden, F5): `Ask` exits 3 with the question before
  any teardown. `verbs` maps exit 3 of an `rm` started with `ask: true` to
  `confirm`. Test the decision as a pure fn next to `branch_action_asks_…`
  (`src/commands/rm.rs:456`).
- `docs/api.md`, `index.d.ts`.

## Step 3: the CLI is a client

- `src/main.rs`: on unix and without `--in-process`, `Run`/`Rebuild`/
  `Start`/`Stop`/`Rm` go through a new client fn (in `serve::client` or a
  `serve/verbs.rs` client half): canonicalize `-C` (`root`), collect the
  env (non-UTF-8 vars skipped), connect (lazy start), send, print `output`
  to the matching stream as it arrives, exit per the result (F9). Connect
  failure → `warning:` + the in-process path (the fallback). Today's
  in-process branch (build log enable, autostart, `ensure_running`) is what
  `--in-process` runs; keep `autostart` running in-process in the client
  after a successful verb, as now (it's a once-per-boot pass; flag in the
  summary if that turns out wrong).
- Prompts before sending: a missing `run` sandbox → `sandboxes.list` + `pick`
  on a TTY (F6, empty config → `offer_example_config`); an instance name →
  `instances.resolve`, `pick` on a TTY when several, else today's
  "ambiguous" error; `rm` on a TTY without a flag sends `ask: true` and
  loops once on `confirm`.
- `-d`/`--detach` on `run`, `rebuild`, `start` (clap; help text says it
  returns once the instance is registered). Prints as decided.
- Ctrl-C (Decisions): a SIGINT handler in the client like
  `src/commands/run/cancel.rs` (reuse it if it fits); 130 after a cancel.
- Daemon lost mid-verb: F10.
- Tests: the param building from parsed `Cli` (pure), the result → exit code
  mapping, the confirm loop with a fake call fn. One `tests/serve.rs`-style
  integration test, if the harness there allows it without docker, of `rm`
  on a missing instance through a scratch daemon (an error round trip).

## Step 4: the TUI sends requests

- `run`/`rebuild`/`start` (`src/tui/mod.rs:343`): a thread per verb that
  connects (no fallback, F8) and sends the request with `detach: true` and
  the dashboard's env, reporting `OpDone` ("run web: web-2 started" /
  the error). Keep the status-line wording of `detached_outcome` where it
  still fits; the final outcome now shows as the row's phase (the build
  finishes after the response), so drop the exit line.
- `d` on an in-flight row (`:399`): `instances.cancel` via the daemon
  worker's connection (`src/tui/daemon.rs`, a new `Cmd` like `ForwardRm`),
  status line from the answer.
- Delete `spawn_detached`, `spawn_cancel`, `interrupt`, `detached_args`,
  `detached_label`/`detached_outcome`/`last_error_line` if unused, and their
  tests (the argv mapping test lives in `serve::verbs` now).
- `rm`/`stop`/`services rebuild` stay in `run_suspended` (`:656`).
- TUI docs: `docs/tui-*.md` and the site's dashboard pages where they say
  the dashboard spawns detached processes.

## Step 5: verbs across a handoff

- At start, the daemon sweeps `logs/verbs/`: a `<pid>` that's alive is an
  adopted verb (counted as a holder until its pid dies, checked on the
  host poll), a dead one's files are removed.
- `instances.cancel` already works by recorded pid, so an adopted verb can
  be cancelled.
- Test: a scratch daemon with a fake live/dead pid pair of spool files.
- `docs/serve.md` *Version handoff*: in-flight verbs are left running.

## Step 6: docs and changelog

- `docs/client-server.md`: rollout 3 done for the five verbs (what's left:
  `services *`, the rest), fold in F1–F10 where they differ from the
  design, close the *Env filter* open question.
- `docs/serve.md`: *Start* (who starts it: every verb), *Idle exit*
  (verbs), *Version handoff*.
- CLI reference on the site (find where `run`'s flags are documented) for
  `-d`; `skills/config-toml-spec/SKILL.md` unchanged (confirm).
- `CHANGELOG.md` `## Unreleased`: verbs run in the daemon (survive the
  terminal, Ctrl-C still cancels), `-d`, the caller's env per request.
