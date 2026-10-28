# Resumable exec: interactive sessions that outlive their connection

Status: planned (step 4, the part after fleet step 1)
Steps 1-3 shipped: helper sessions (`devsbd/src/sessions.rs`), `exec`
through them with `--attach` / `--sessions` / `--session`
(`src/commands/exec.rs`), reconnect over ssh and in remote dashboard tabs
(`serve::machines::cli` `Reconnect`, `tui::app::terminal`). Step 4's "now"
part (4a-4d) shipped too: `session prune` and the prune on `new`, a
displaced client that never blocks, the session wire version
(`VERSION_EXIT`), the session count in `instances.procs`. The living docs
describe all of it. Left: step 4's items after fleet step 1 (surfacing
detached sessions, a policy for abandoned ones).

## Problem

`devsandbox exec x` (a CLI shell, a dashboard terminal tab, `exec m/x` over
ssh) is `<runtime> exec -it … <command>` (`commands::exec::exec_argv_with`).
When the client side dies (ssh drops on a network blip, the terminal
closes), the runtime's exec client goes with it but the process in the
container keeps running: the runtime holds its pty, nothing sends it a
`SIGHUP`. Neither docker nor podman can re-attach to an exec session, and
devsandbox keeps no handle on it. The process is leaked: unreachable,
running until the container stops, and blocked once it fills its pty's
output buffer. A long agent session (zidane, claude) started in such a
shell is lost to a short outage.

## Goals

1. **Every interactive exec is resumable** by default: the process lives
   in a session the helper holds, and any later client can attach to it.
2. **Reattach by hand**: `devsandbox exec x --attach [<id>]`, and a
   listing of a sandbox's sessions.
3. **Reattach by itself** where devsandbox owns the transport: `exec m/x`
   over ssh and the dashboard's remote terminal tabs re-dial and reattach
   after a dropped connection.
4. **Cleanup** of ended sessions and of sessions nobody will come back to
   (a later step, below).

Non-goals: restoring the exact screen on reattach (the program redraws,
as with dtach; a screen-keeping emulator may come later); non-tty execs
(`exec x -- cat f > out` has piped stdio, nothing to re-attach to; it keeps
plain exec); Apple `container` and any instance without the helper
(`Instance::devsbd_arch` `None`): plain exec, as today.

## Design

### Sessions in the helper (`devsbd/src/sessions.rs`)

A session is dtach's shape: one supervisor process owns the pty master and
the command, and listens on a unix socket; a client attaches to the socket
and relays its own terminal to it.

- **Layout**: `/var/lib/devsandbox/sessions/<id>/` next to the runs dir
  (same writable layer, 1777 + sticky): `argv`, `meta` (`started`,
  `supervisor`, `pid`/`pgid`, then `ended` + `exit`/`signal`, the runs
  format) and `sock` (0600, the session's user). Same liveness rule as
  runs (pid *and* cmdline name the id), so a container restart leaves them
  `lost`. A separate dir from runs, so dispatchers' `run ls` is unchanged;
  the bookkeeping is `runs.rs`'s, factored to take the dir.
- **Supervisor** (`devsbd session supervise <id>`, spawned detached like
  `run supervise`): opens a pty (`posix_openpt`/`grantpt`/`unlockpt`/
  `ptsname` through `extern "C"` like `runs.rs`'s `kill`, std already links
  libc; `setsid` + `TIOCSCTTY` in `pre_exec`), spawns the command on the
  slave in the caller's cwd and env, then relays: pty output to the
  attached client, its input to the pty, its size to the pty.
  - **The pty is always drained**: with no client (or a client that
    stops reading), output is dropped, never left to block the program,
    and never blocks the supervisor (today's leak freezes on a full pty).
  - **One client at a time; a new attach takes over**: the previous
    client gets a `detached` record and exits. Needed, not cosmetic: when
    ssh drops, docker leaves the old in-container client running, still
    "attached", so a reattach must displace it.
  - On the command's exit: `exit <code>` to the client, `meta` written,
    socket removed, supervisor exits. No output log: a session lives
    forever, a log would too.
- **Client** (`devsbd session new|attach`): runs in the runtime's `exec
  -it` pty, puts it raw (restored on exit), connects to `sock`, sends its
  size (and again on `SIGWINCH`), relays bytes both ways, and exits with
  the command's exit code on `exit`.
  - `new --id <id> [--cwd DIR] -- <argv>`: create, then attach; an id
    that is already a live session attaches instead.
  - `attach <id>`: attach only. An ended session exits at once with its
    recorded code; an unknown or `lost` one exits with a dedicated code
    (`NO_SESSION_EXIT`, distinct from the program codes a shell reports
    and from the helper's existing ones), message on stderr.
  - `detached` from the supervisor: message on stderr, exit with the same
    dedicated-code family (a second constant), so callers can tell
    "taken over" from "the program exited".
- **Redraw on attach** (dtach's): the supervisor sets the pty to a size
  one row off, then the client's, so the program gets `SIGWINCH` and
  redraws. A plain shell shows only what comes next.
- **Socket wire**: helper-only (the host never speaks it, it goes through
  `exec -it` like today): framed records `u8 tag | u32 len | payload`:
  `→ size` (`u16 cols, u16 rows`), `→ data`, `← data`, `← exit`,
  `← detached`. Both ends are the same binary.
- **`ls` / `rm`**: `devsbd session ls` (id, state, attached or not,
  started, argv), `rm <id>` (SIGHUP then SIGKILL to the pgid, like `run
  rm --force`).

### Host: `devsandbox exec`

- **Through a session when** the instance has the helper
  (`Instance::devsbd_arch` is `Some`) and the exec gets a tty (`-t`, or
  the login-shell default on a terminal; `shell_flags`): the argv becomes
  `<runtime> exec -it … <container> <helper path> session new --id <id>
  -- <command>`, with the same user, workdir and env flags as today, so
  `SSH_AUTH_SOCK` and remoteEnv still reach the command. Otherwise plain
  exec. The helper is current by the time it matters: `start` and the
  daemon's bridge (`devsbd::ensure_recorded`) reinstall a stale one.
- **Ids**: generated by the host (short random, `control::valid_run_id`
  rules). Every command-less `exec x` is a new session: a new shell, not
  the last one.
- **`--attach [<id>]`**: with an id, attach; without, the one detached
  live session of that instance, else on a TTY a pick list, off a TTY an
  error listing them. **`--sessions`**: the listing (`devsbd session ls`).
- **`--session <id>`**: create-or-attach with a caller-chosen id (the
  first dial of the reconnect loops below, scripts).
- **Exit status**: the command's, as today (the client exits with it).

### Reconnect

- **CLI over ssh** (`serve::machines::cli`): for a tty exec on a machine,
  the local CLI picks the id and runs the remote `exec --session <id> …`.
  ssh exiting 255 (connection lost) → `connection lost, reconnecting…
  (Ctrl-C to stop)` on stderr, backoff, re-dial with `exec --attach <id>`
  (attach only: a reconnect never starts a second process). The loop ends
  on any other exit: the command's code, or `NO_SESSION_EXIT` (the
  instance had no helper so there was no session, or it's gone: say so).
- **Dashboard remote tabs** (`tui::remote`, `tui::term`): the tab keeps
  its session id; a 255 exit re-dials with `--attach <id>` into the same
  tab instead of ending it.
- **Local**: nothing to re-dial; a closed local tab or terminal leaves the
  session for `--attach`.
- **Host daemon idle/exit** changes nothing: sessions live in the
  container, owned by their supervisors, not by `serve` or `devsbd
  daemon`. Stopping the sandbox ends them.

## Checks for implementers

The full `cargo test --workspace` doesn't run cleanly on the dev machine
(docker-gated tests, macOS host, slow). Run the targeted tests for what
you touch (`cargo test -p devsbd <filter>`, `cargo test --lib <filter>`,
`cargo test --test docs_links`) and `cargo check --workspace`; report
exactly what ran and what didn't. CI (`.github/workflows/ci.yml`, Linux,
docker, `DEVSANDBOX_DEVSBD_REQUIRED=1`) is the source of truth; the
orchestrator pushes each commit and reads its checks. Helper tests run on
the host too, so macOS must at least compile and pass them (or gate
Linux-only parts with `cfg(target_os = "linux")`, saying why).

## Step 1: helper sessions

Build `devsbd/src/sessions.rs` per *Sessions in the helper*.

- Wire `Some("session")` in `devsbd/src/main.rs` (next to `Some("run")`).
  Append `"session"` to `FEATURES` there.
- Reuse runs' bookkeeping (`devsbd/src/runs.rs`: `create`, `read_argv`,
  `parse_meta`, `state_of`, `is_supervisor`, `load_state`, `kill_group`,
  `exit_line`, the `DIR` constant): factor what's needed to take a root
  dir, keep runs' behavior and tests unchanged. Sessions' root is
  `/var/lib/devsandbox/sessions` (and must be created like the runs dir:
  find where `INSTALL_SCRIPT` in `src/devsbd.rs` makes `runs`, add it
  there).
- Pty via `extern "C"` libc calls, no new dependency (`devsbd/Cargo.toml`
  has none). ioctl numbers differ per OS (`TIOCSCTTY`, `TIOCSWINSZ`,
  `TIOCGWINSZ`): per-`target_os` constants for linux and macos.
- Constraints: the supervisor never blocks on a client; always drains the
  pty; one client, new attach takes over (old gets `detached`); raw mode
  restored on every client exit path; exit codes as constants with doc
  comments.
- Tests (`#[cfg(test)]` in `sessions.rs`): wire codec; supervisor + client
  over a real pty with a temp root (echo round trip, exit code
  propagation, resize reaching the program, takeover, attach to an ended
  session returns its code, attach to unknown returns `NO_SESSION_EXIT`,
  `lost` after killing the supervisor, output dropped while detached
  doesn't block the program).
- Docs: `docs/architecture/helper/in-container.md` (files table: the
  sessions dir; a `## Sessions: devsbd session …` section like *Runs*).

## Step 2: host exec through sessions

- `src/commands/exec.rs`: `exec_status` / `exec_argv_with` build the
  session argv when it applies (see *Host*). The helper's in-container
  path: find how `src/commands/dispatch.rs` (`exec_in`) or
  `src/devsbd.rs` name it, reuse that.
- `src/main.rs` `Command::Exec` (`--attach [ID]`, `--sessions`,
  `--session ID`; conflicts declared in clap) and its dispatch (~line
  700).
- `src/serve/machines/cli.rs` `remote_args` (`Command::Exec`): pass the
  new flags through to the machine.
- Dashboard local tabs spawn `devsandbox exec` (`tui::app::terminal`
  `term_argv` / `exec_argv`), so they get sessions without changes; check
  that nothing there breaks (tests in `src/tui/app/terminal.rs`).
- Tests: the argv builder (session vs plain: no helper, no tty, explicit
  `-t`), flag parsing.
- Docs: `docs/architecture/helper/in-container.md` (who starts sessions),
  `docs/architecture/dashboard/terminal.md` if what a tab runs changes,
  `docs/architecture/helper/ssh-agent.md` if its exec description changes.

## Step 3: reconnect

- CLI: `src/serve/machines/cli.rs` (`route` / `resolve_and_route` →
  `run_ssh` / `exec_ssh`). A tty exec on a machine can't `exec` ssh any
  more (it must outlive it): spawn it (`spawn_ssh_owning_signals`), loop
  on 255 per *Reconnect*. Pure decision fn (exit code → retry / done) with
  tests.
- Dashboard: `src/tui/remote.rs` `exec_argv`, `src/tui/term.rs`
  `TermSession` (exit detection: `exited`), `src/tui/app/terminal.rs`.
  The tab keeps its id and re-dials on 255; the decision logic in the
  I/O-free `app/` with tests.
- Docs: `docs/architecture/daemon/machines.md` (*The CLI*, *Dashboard*),
  `docs/architecture/dashboard/terminal.md`.

## Step 4: cleanup (4a-4d shipped; the items after fleet step 1 are left)

Order with the other plans: [helper-rpc](helper-rpc.md#order-across-plans).

Now, before [fleet](fleet.md) step 2:

- Prune ended/`lost` session dirs.
- The displaced client (below).
- A version byte in the session wire, refused with a clear message on
  mismatch: a supervisor keeps its old binary while `ensure_recorded`
  installs a new helper, so a new client can meet an old supervisor.
  Fleet's worker self-update makes that routine and unattended.
- A session count on the instance row (`session ls`).

After fleet step 1 (fleet root, `lend.toml`):

- Surface detached sessions (dashboard rows, maybe an Inbox note). The
  Inbox is per host until fleet step 14, so a note for a hub-placed
  instance would land in the worker's own Inbox.
- A policy for sessions nobody re-attaches to (idle timeout or
  explicit); on a worker they hold lent capacity, so it may belong in
  `lend.toml` or the capacity report.

The "now" part shipped as four commits (4a-4d). Landmarks are from recon
(line numbers drift; trust the symbol).

### Step 4a: `session prune`

- `devsbd session prune [--keep N]` (`devsbd/src/sessions.rs` `parse` /
  `Cmd` / `USAGE`) on `runs::prune(root, keep)` (`devsbd/src/runs.rs`,
  make it `pub(crate)`; already root-parameterized, skips running dirs,
  goes on past another user's dir in the sticky 1777 dir).
- Automatic, quiet prune in `create_with` (before `create_dir`, errors
  ignored), as the session's own user: every `lost` dir, and ended dirs
  whose `ended` is older than a few minutes (a constant with a doc
  comment). The age guard matters: `join` returns an ended session's
  recorded code, which the ssh reconnect loop (backoff up to 10 s,
  `serve::machines::cli`) relies on when the program ends during an
  outage. Needs an age-aware variant next to `runs::prune`; runs'
  behavior and tests stay unchanged.
- Fixes: `ls` lists only the newest `runs::MAX_LS_RUNS`, so ended dirs
  piling up hide an old live session from `ls` and from `exec --attach`'s
  pick list (`src/commands/exec.rs` `detached_sessions`).
- Not in the daemon, `boot` or `ls` (the daemon may run as root; `ls`
  stays read-only).
- Tests in `sessions.rs` `mod tests` (model: `runs.rs`
  `prune_keeps_the_newest_ended_runs_and_never_running_ones`,
  `fake_run`), `parses_forms`.
- Docs: `docs/architecture/helper/in-container.md` *Sessions*.

### Step 4b: a displaced client never blocks

- Today `read_records` (`sessions.rs`) writes pty output with a blocking
  `write_all` on the same thread that reads the socket, so a stalled
  stdout means `detached`/`exit` are never read; the supervisor's
  `Supervisor::accept` sends `Detached` once, non-blocking, then drops the
  old socket, so a full socket buffer loses it (the client then reports
  `NO_SESSION_EXIT`, not `DETACHED_EXIT`).
- Fix in the client: the socket reader hands output to a writer thread
  through a bounded queue and drops when full (the supervisor already
  drops past `OUT_CAP`), so records always arrive. On the outcome:
  restore the terminal, write the note with a bounded wait (thread + ~1 s
  or skip), `std::process::exit(code)` (ends a stuck writer). No signals,
  no new libc calls.
- Test: extend `a_new_attach_takes_over` with a first client whose `out`
  blocks forever (like `Sink`, stalled): it still exits `DETACHED_EXIT`.
- Docs: `in-container.md` (client bullet); drop the review note above.

### Step 4c: session wire version

- Today a new client meeting an old supervisor is bad twice:
  `Supervisor::accept` displaces the attached client before reading
  anything, then the unknown record drops the new one (`Lost`, 250).
- `create_with` writes `proto <N>` into `meta` (`runs::Meta` gets the
  field; `parse_meta` already skips unknown keys). `join`/`connect` check
  it before dialing; missing or different → refuse without connecting:
  `session <id> was started by another devsbd (proto X, this is Y); end it
  with session rm`.
- `TAG_HELLO` (`u8` version) as the client's first record; the supervisor
  checks it before `accept` displaces anyone, answers or closes. Guards
  future versions inside the wire.
- `VERSION_EXIT = 252`, same family as `NO_SESSION_EXIT` (250) /
  `DETACHED_EXIT` (251). Not 250: the host prints "gone" for 250.
  `serve::machines::cli::Reconnect` already ends on it; a comment by its
  `NO_SESSION_EXIT` mirror. `src/commands/exec.rs`'s code-list test.
- Tests: codec (new tag), refusal without displacing an attached
  `TestClient`.
- Docs: `in-container.md` (records, exit codes),
  `docs/architecture/daemon/machines.md` where it lists 250/251.

### Step 4d: session count in `instances.procs`

- Not in the snapshot (`snapshot::InstanceRow` is built without
  per-instance execs; a count there costs an exec per running instance
  per poll).
- `src/runtime/procs.rs`: `session_ls` next to `run_ls` (same bound and
  `None` on failure, e.g. an old helper), a pure parser/count of `session
  ls` lines (state is the 2nd word). `ProcsListing` (`src/serve/api.rs`)
  gains `sessions` (running, detached), `#[serde(default)]`;
  `fetch_procs` fills it. `npm/devsandboxes/index.d.ts` `ProcsListing`
  (checked by `src/serve/dts.rs` via the api test).
- Dashboard: carried with the procs rows (`src/tui/procs.rs`
  `ProcState::Rows` or beside it), a `sessions: N (M detached)` Detail
  line (model: `App::agent_count` → `ui.rs` `agents_line`) and a tree-row
  badge in `instance_tree_row` when cached. Only expanded / selected
  running instances are fetched (`app/procs.rs` `proc_fetch_targets`), so
  others show none: accepted.
- Tests: the parser, the dts conformance, `app/procs.rs` (model:
  `agent_count_counts_agent_rows_in_cache`), ui.
- Docs: `docs/reference/api.md` (`instances.procs`),
  `docs/architecture/dashboard/tabs-and-actions.md`,
  `docs/architecture/runtimes.md` where it describes the procs listing.

## Open questions

- **Detach key**: a key (dtach's `Ctrl-\`) that leaves the session running
  and returns to the host shell, rather than only closing the terminal.
  Every key it takes is a key the program can't get, and closing the
  tab/terminal already detaches. Proposal: none for now, an opt-in later
  if missed.
- ~~Relation to [helper-rpc](helper-rpc.md)'s `run.attach`~~ settled by
  [fleet](fleet.md) step 4 (decision 13): a fleet worker's daemon
  attaches through `session.attach` over `RPC`; local execs stay on
  `exec -it`.
