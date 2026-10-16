# Client/server: the CLI as a client of `devsandbox serve`

Status: design, rollout steps 1–5 done. Steps 1–2 (instance phase, build
log, a TUI that doesn't suspend) in-process: `docs/instance-phase.md`. Step
3 (verbs as requests) for `run`, `rebuild`, `start`, `stop` and `rm`:
`docs/verbs-as-requests.md`. Step 4 (the dashboard as a pure client):
`docs/dashboard-client.md`. Step 5 (machines): `docs/machines.md`. Steps
6–7 (remote forwards, remote VS Code) are not implemented.

The daemon (`docs/serve.md`) owns the live host side: bridges, forwards,
the API, and now the five verbs above plus `service rebuild`. Other CLI
verbs (`service ls`, `rename`, `done`, `gc`, …) still do their work in the
process you started; the dashboard's `rename` and done/undone are API
methods.
This doc moves the verbs' work into the daemon, the way `docker` hands
everything to `dockerd`. The CLI and the TUI become clients, and the client
can be on another machine.

## Goals

- **Work outlives its caller.** A `run` from the TUI, or `run -d`, finishes
  without anyone watching, and its log is kept.
- **No half-built sandboxes.** Cancelling rolls back cleanly. A run cut short
  by a crash, OOM or a reboot is visible and can be resumed or removed.
- **Status where the sandbox is.** The TUI shows a building instance under
  its sandbox with its current step, and has a shortcut to its build log.
  There's no separate list of operations.
- **Remote machines.** `devsandbox machine add ssh://me@host --name my-server`,
  then `run foo` (or `run my-server/foo` when `foo` is defined on both).
  The local daemon knows about the machine, and the dashboard shows its
  sandboxes next to the local ones, with a machine column. This is the VS Code SSH Remote +
  devcontainers model, built into the CLI.

Non-goals: scheduling or placement across machines (no Swarm), daemons
talking to each other except through the local one, and a TCP listener.

## Requests

There's no "job" or "command" object. A verb is an RPC method on the
existing protocol (`docs/api.md`): `devsandbox run foo --branch x` sends a
`run` request carrying the same arguments. That's all of it.

```
→ {"id":1,"method":"run","params":{"root":"/home/me/.devsandboxes","sandbox":"foo","args":{…},"env":{…},"detach":false}}
← {"method":"progress","params":{"request":1,"instance":"foo-2","phase":"building"}}
← {"method":"output","params":{"request":1,"stream":"stderr","data":"…"}}
← {"id":1,"result":{"instance":"foo-2", …}}
```

- **Params** follow the verb's flags. Clap stays the parser, and the client
  sends what it parsed.
- **`root`**: an absolute config root the server can resolve. Locally the
  client canonicalizes `-C` (the only thing cwd matters for today,
  `src/main.rs:28`). For a machine, it's one of the machine's roots
  (*Config roots*). The client never sends a cwd.
- **`env`**: the caller's whole environment (non-UTF-8 variables left out),
  required: the verb runs with it and `GIT_TERMINAL_PROMPT=0`, never with
  the daemon's. Not an allowlist of the config's `${localEnv:…}` names:
  that would need the config parsed first. In-process, the verbs read
  the calling shell's env today. A daemon only has the env of whoever
  started it (`docs/serve.md`, *Start*): an old shell, or systemd. Things
  that would quietly break without this field:
  - `${localEnv:VAR}` in the config (`skills/config-toml-spec`) would
    expand to the daemon's value.
  - `SSH_AUTH_SOCK`, recorded on the instance (`state::Instance::ssh_auth_sock`)
    and used to clone over ssh during the lifecycle chain.
  - `DEVSANDBOX_RUNTIME`, `GIT_*`.

  The bridges already work around this by taking the agent from a client
  ("until a client reports one"). This field makes the same thing
  per-request and explicit.

  **Whose env**: the env of the user's session on the machine that runs
  the containers, never the daemon's. Locally that's the calling shell.
  For a machine it's the ssh login session of the user the machine was
  added with: the `api --stdio` relay runs in that session and attaches
  its env to every request it relays. The laptop's env is not sent to a
  remote. `${localEnv:…}` mostly builds host paths for mounts
  (`${localEnv:HOME}/…`, `${localEnv:XDG_RUNTIME_DIR}/docker.sock`), which
  only make sense on the machine running docker. This is also how
  `${localEnv}` resolves for VS Code Remote-SSH + devcontainers. The one
  value that comes from the caller is the ssh-agent, through `ssh -A`
  (*Machines*).
- **Streaming**: one connection per verb request, held until the verb
  ends; nothing else is read from it meanwhile. `progress` (phase changes)
  and `output` (the verb's stdout/stderr) arrive as notifications on it,
  and the CLI prints them the way it does in-process. `run --json`'s
  document arrives as `stdout` output.
- **Hang-up = cancel**, unless `detach`: the daemon SIGINTs the verb's
  process group and it rolls back (*Failure*). A detached request is never
  cancelled by its client leaving, answered yet or not. A client that
  stops reading for 5 s is disconnected, but its verb runs on (a paused
  pager isn't a cancel), and so does a daemon exit (*Handoff*). The CLI's
  first Ctrl-C sends `instances.cancel` on a second connection and keeps
  streaming the rollback, then exits 130; the second hangs up.
- **`-d` / `detach: true`** (`run`, `rebuild`, `start`): the response
  comes as soon as the instance is registered. The CLI holds the verb's
  output back and prints only the name (`run -d --json`: the run record),
  or replays the output if the verb fails first. The TUI always detaches.
- **Prompts** happen on the client, before the request: a `run` without a
  sandbox picks from `sandboxes.list` (an empty config: the CLI offers the
  example config itself, it writes into the user's dir), an ambiguous name
  picks from `instances.resolve`'s matches. `rm`'s delete-branch question
  is a round trip: the client sends `ask: true`, the verb stops before
  removing anything and the daemon answers the error `confirm` with the
  question; the client asks and resends with `deleteBranch`. Daemon-side
  verbs never prompt (same rule as `dispatch.rs`'s null stdin +
  `GIT_TERMINAL_PROMPT=0`).
- **No daemon**: off unix, or when no daemon answers, the CLI prints one
  `warning:` line and runs the verb in its own process (`-d` is an error
  there). The dashboard has no fallback: its status line says the daemon
  is needed.

### Execution, internally

The daemon runs each verb as a `devsandbox --in-process -C <root> <verb> …`
subprocess (`src/serve/verbs.rs`), like `src/commands/dispatch.rs`'s
`Subprocess` executor: own process group, null stdin, env from the request.
`--in-process` (hidden) runs the verb in that process and never starts a
daemon, so a draining daemon's verb can't lazy-start a racing successor;
it's also what the CLI's fallback runs as. The verb writes its own build
log, as in-process, and runs the autostart pass after it with the caller's
env. Its stdout and stderr go to spool files,
`<data>/devsandbox/logs/verbs/<pid>.out|.err`, which the daemon tails to
the requester: with pipes, a daemon exiting mid-verb would break the verb's
writes. Why a subprocess and not a thread:

- a verb that panics or hangs takes down one request, not the daemon;
- the version handoff doesn't have to wait for a 10-minute build (*Handoff*);
- the verbs' code doesn't change.

The daemon keeps an in-memory table of what's in flight (pid, request,
instance). That's the internal *task*. The word never appears in the CLI,
the TUI or the API. What persists is the instance's phase (next section)
and the spool files, not a separate tasks file. Every verb in flight,
detached or adopted (*Handoff*), is an idle-exit holder: a detached `run`
keeps the daemon alive until it ends.

## Instance phase

Built (`docs/instance-phase.md`). `run` records the instance **first**,
before the worktree, with an in-flight `progress`, and updates it as it
goes:

| phase | |
|---|---|
| `preparing` | name chosen, worktree/branch/folders/services |
| `building` | feature image / Dockerfile build |
| `creating` | the container |
| `onCreateCommand` … `postAttachCommand` | the lifecycle step running (plus `shell-rc`) |
| `failed` | stopped partway: error + the step it was at |
| *(none)* | done. Status comes from the runtime |

`progress` also keeps the verb's `pid` and the boot id, and each created
resource (worktree, branch, `folders` worktrees, volumes) is recorded on
the instance as it's made, not at the end, so a cancel or `rm` can undo a
partial run. `rebuild` starts at `building` on the existing record. `ps`,
`status --json` (`phase`, docs/json-output.md) and the dashboard show the
phase as the status.

Unfinished instances are guarded: `stop`, `rename`, `rebuild <name>` and
`rm` (even `--force`) refuse a live in-flight one; `start` also refuses a
failed one and points at `rebuild`/`rm`; `rebuild --all` and autostart skip
both; the dispatcher's `ensure` fails naming the phase instead of acting
(docs/automations.md). `exec` refuses until the container exists, then
reaches it during a lifecycle step, failed included, for debugging.

### Build log

One log per instance, `<data>/devsandbox/logs/<instance_id>/build.log`:
the whole output of its last `run`, `rebuild` or `start`, which rotate the
previous one to `build.1.log`. `rm` (and a cancelled `run`) remove the
directory. It's what the TUI's `b` opens and what `devsandbox logs --build
<instance>` prints (`--follow` until the instance is no longer in flight).
The daemon serves its tail as `instances.buildLog`, which is how the
dashboard's `b` reads it (following it remotely is `instances.follow`,
later). It's an fd-level tee in the verb's process (`src/build_log.rs`,
unix only), opt-in from `main` for those three verbs: the daemon's
subprocess, or the CLI in-process. On a terminal (or for a verb request
with `tty`) it tees through a pty of its own, so the build keeps its
colour and TTY progress; the log's header then records the pty size
(`# tty <cols>x<rows>`) for readers to replay it at. It replaced the TUI's suspended run and its
`logs/<verb>-<unix>.log` error files for those verbs.

### In the TUI

A building instance is listed under its sandbox config like any other.
Its status column shows the phase (`building`, `postCreateCommand`, …), and
`b` opens its build log in any phase (following it while in flight),
including after it's `running` or `failed`. A `failed` row shows the step
and error in Detail, and offers retry (`s` rebuilds it) and remove (`d`).
`d` on an in-flight row asks to cancel its build instead. The status column
is the only progress UI.

The dashboard sends `run`, `rebuild` and `start` to the daemon as detached
verb requests (`docs/api.md`, *The dashboard as a client*), so they outlive
it, with a one-line status when one is registered; `d` cancels through the
daemon (`instances.cancel`).

## Failure

- **Any error → `failed`, kept.** A `run` that fails at any step, before
  the container exists included, keeps the instance as `failed` (step +
  error) with everything it made, and says how to retry or remove it. A run
  started from the dashboard runs unwatched, and a rollback would take its
  row and build log with it.
- **Cancel is the only rollback** (Ctrl-C or SIGTERM in-process; through
  the daemon a hang-up or `instances.cancel`, which SIGINT the verb's
  process group; the CLI's Ctrl-C and the dashboard's `d` are
  `instances.cancel`). A cancelled `run` undoes
  everything recorded: container, per-instance services/network, volumes,
  `folders` worktrees, the worktree, the branch if it created it and it has
  no commits beyond its start point, then the record and its build log. A
  second signal kills at once. A cancelled `rebuild` leaves `failed` (the
  instance existed before).
- **Crash / OOM / reboot**: the verb's process is gone and the phase is
  still in flight. Every reader derives it at read time: a recorded `pid`
  that isn't alive, or a boot id that changed, shows as `failed` with
  "interrupted" as the error. Nothing rewrites the record, so no daemon is
  needed for it.
- **Retry**: `rebuild <instance>` on a `failed` instance re-materializes
  from the recorded worktree and branch. (`run` never resumes: a name in
  use is an error.)
- **Remove**: `rm` on a partial instance undoes only what was recorded, and
  copes with a container that never existed.

## Handoff

A newer client still triggers the version handoff (`docs/serve.md`). The
verbs in flight are subprocesses with their output going to spool files,
so the old daemon doesn't wait for them. It leaves them running and exits
(as on an idle exit, `shutdown` or a crash). The new daemon sweeps the
spool dir at start: a live pid's verb is adopted, a holder until the pid
dies, unstreamed but still cancellable (`instances.cancel` signals the
instance's recorded pid); a dead one's files are removed. A CLI whose
connection ends before the answer says so and, for a build with a known
instance, follows its build log locally (`logs --build --follow`) and
exits by the instance's phase. Following over the protocol
(`instances.follow`) is for remote clients, later.

## Config roots

Requests name a root the server can resolve, so roots become a server-side
notion:

- **Local**: any directory the client canonicalizes. The daemon already
  works per root (one forward registry per root, `Instance::config_dir`).
- **Machine**: `machine add` saves the remote root(s) on the machine
  entry, from `--config <remote path>` (repeatable) or picked during the add
  (*Adding a machine*). A machine's roots are only the registered ones:
  `run foo` (or `run my-server/foo`) resolves `foo` against the local root
  and every machine's roots, one match runs where it is, several are a pick
  on a TTY or an error listing them qualified. `-C` stays a local path,
  never forwarded: a command re-run on a machine gets `-C <its root there>`.
- The dashboard asks for sandboxes grouped by root (`sandboxes.list`) and
  never reads a config file itself.

## The dashboard as a pure client

Built (`docs/dashboard-client.md`; connections in `docs/api.md`, *The
dashboard as a client*). The dashboard no longer reads config files or
asks the runtime. It watches its root (`instances.watch`,
`docs/before-machines.md`): the daemon's one container poll collects every
watched root every 2 s (stats at most every 7 s) and pushes
`instances.snapshot`, so any number of dashboards cost one collection;
a `state.toml` change re-collects within ~0.5 s. It polls
`instances.procs` for expanded instances. The config view, logs, inspect
and build log are `sandboxes.get` / `services.get`, `containers.*` and
`instances.buildLog` (with `ansi`: builds run on a pty, so the log keeps
its colours); signals, rename and done are `instances.signal`,
`instances.rename`, `instances.done`. Every verb is a request: `run`,
`rebuild`, `start` (and `s` on a stopped row) detached, `stop`, `rm` and
`services.rebuild` held but sent with `detach`, so quitting the dashboard
never cancels one. Instance terminals run `devsandbox exec`. With
the daemon down it shows the last tree, marked stale, and refuses actions;
it has no in-process fallback, so off unix there is no dashboard for now.

Still local, for now:

- **The Inbox.** It reads the store file and, while disconnected, writes
  it through the same ops code the API runs. A remote machine's threads
  aren't shown yet (step 5 federates instances, not the Inbox).
- **VS Code** (`o`, `:code`, a thread's `vscode` action) reads
  `state.toml` for the container and workspace; remote VS Code is step 7.
- Its own files: `dashboard.toml` settings and the prompt history.

This is the prerequisite for machines: once the dashboard only talks to
its daemon, a remote instance is just another row.

## Exec and terminal

`exec -it` runs `docker exec -it` locally, and so do the TUI's terminal
tabs: an instance's through `devsandbox exec`, a service's directly. That
works as long as the runtime is local.

- **Remote, first cut** (built, `docs/machines.md`): the client runs `ssh
  -t <target> devsandbox -C <root> exec <instance> …` through the user's
  login shell (`-T` off a terminal). The local daemon resolves the instance
  to its machine (`instances.resolve {machines: true}`) and hands back the
  target (`machines.get`). Same PTY path as today. The dashboard's terminal
  tabs do the same for a remote instance, and `ssh -t <target> <runtime>
  exec -it …` for a remote service. `logs`, `inspect`, `done`, `undone` and
  `rename` on a remote name re-run the same way.
- **Later**: PTY attach over the protocol (an `exec` stream with resize),
  for clients that can't spawn ssh.

## Machines

Built (`docs/machines.md`, which has the details and the known
limitations). Where it differs from the sketch below: named commands take
`my-server/foo` (or a bare name found on one machine) instead of
`--machine`, which is only for commands without a name (`ps`, `ls`,
`stop --all`, …); the env is the remote login shell's, swapped in by the
remote relay (`api --stdio --session-env`) on each verb request; verbs go
over an ssh of their own each, quick calls over the held link, which is up
only while wanted (a dashboard watching, a call, 60 s after); forwards
from a machine are not built.

```
devsandbox machine add ssh://me@my-server.com --name my-server [--config '~/sandboxes']…
devsandbox machine ls | rm <name>
devsandbox run foo                # foo only defined on my-server; else my-server/foo
devsandbox ps --machine my-server
```

### Adding a machine

Two API calls, so the CLI and a GUI (over `api --stdio`) go through the
same steps, and the daemon still never prompts (*Requests*):

1. **`machines.probe {target}`**: the local daemon connects (*Transport*)
   and sends a hello. That checks ssh, that devsandbox is on the remote
   and that the versions are compatible, so a bad host fails at `add`
   instead of at the first `run`. It returns the remote's candidate roots,
   each with its sandboxes:
   - every distinct `config_dir` in the remote's `state.toml`,
   - `~/.devsandboxes` if it exists,
   - nothing from scanning the disk.
2. **`machines.add {target, name, roots}`**: saves the entry. A root that
   wasn't among the candidates (typed in by hand) is checked on the remote
   first: it exists and its config parses.

Without `--config`, the CLI:
- on a TTY, shows a multi-select of the probed roots, each with its
  sandboxes, plus an "other path…" entry;
- with no TTY, uses the root automatically if there's exactly one. Zero or
  several is an error that lists the candidates and points at `--config`.

A GUI calls probe, shows its own picker, then calls add.

- **Registry**: on the local daemon, in `machines.toml` next to
  `state.toml`. The TUI doesn't know machines exist beyond a column.
- **Transport**: `ssh <target> devsandbox api --stdio` (`src/serve/relay.rs`),
  the same way docker's `ssh://` hosts use `docker system dial-stdio`. ssh
  does all the auth, the remote daemon starts lazily as usual, and there's
  no new listener. One connection per machine, held open by the local
  daemon and reconnected with backoff. A machine that's down shows its
  instances as `unreachable` rather than dropping them.
- **Federation**: the local daemon merges each machine's `instances.list`
  and `sandboxes.list` into its own, and forwards requests for remote
  instances. Each machine keeps its own `state.toml`, config and runtime.
- **Names**: instance names are unique per machine, not globally. Merged
  rows carry `machine`. On the CLI a remote instance is `my-server/foo-2`,
  and an unqualified name that matches only one machine is accepted.
- **Forwards**: a forward to a remote instance binds on the local host and
  tunnels over the machine connection to the remote daemon's forwarder.
  `forwardPorts` of remote instances are opt-in per machine, so a remote
  box doesn't grab local ports by surprise.
- **ssh-agent**: `ssh -A` on the machine connection. The remote daemon's
  existing relay picks the forwarded agent up as a client-reported
  `SSH_AUTH_SOCK`.
- **Env**: requests to a machine carry the env of that machine's ssh
  login session, attached by the relay, not the laptop's (*Requests*,
  `env`). A non-interactive `ssh host cmd` gets a thinner env than an
  interactive login (no `.profile` on some shells), so the relay may
  need to capture a login shell's env once per connection.
- **Access**: a machine's socket gives full control of its containers, like
  `docker.sock`. Adding a machine means trusting ssh access to it. Write
  that down in `machine add --help`.

### VS Code on a remote machine (later)

`code` today attaches VS Code to a local container. For a remote one, the
candidates are Remote-SSH to the machine plus attach-to-container through
its docker context, or Remote-SSH straight into the container through the
helper. Needs a spike. Low priority, and nothing above depends on it.

## Windows

`serve` is unix-only, so on Windows the CLI is a remote-only client. Plan:
`docs/windows-support.md`.

## Rollout

1. Done (`docs/instance-phase.md`): instance phase recorded first,
   failures kept as `failed`, cancel rolls back, still in-process.
2. Done (`docs/instance-phase.md`): per-instance build log, phase in the
   TUI status column, `b` for the log, `run`/`rebuild`/`start` detached
   instead of suspended.
3. Done (`docs/verbs-as-requests.md`) for `run`, `rebuild`, `start`,
   `stop`, `rm`: streaming, cancel, `-d`, env, prompts on the client.
   `service ls`, `rename`/`done`/`gc` and the rest are still in-process
   (`service rebuild` became a verb request in step 4).
4. Done (`docs/dashboard-client.md`): dashboard as a pure client
   (`instances.list` with `depth`, procs, logs, config and edit methods,
   `stop`/`rm`/`services.rebuild` as held verb requests). Then, before
   step 5 (`docs/before-machines.md`): the shared, pushed snapshot
   (`instances.watch`), builds on a pty, held verbs that survive a quit,
   spawns that survive an upgrade.
5. Done (`docs/machines.md`): `machine add|ls|rm`, the ssh transport,
   federation, forwarded requests, names across machines, `ssh -t` exec
   and terminals.
6. Remote forwards.
7. VS Code remote spike.

## Open questions

- **Machine-to-local fallback**: when no local daemon can run (Windows),
  does a configured default machine make `--machine` implicit? Still open.
- **Keeping machines on one version**: auto-update a machine's devsandbox
  to the newest one linked to it (the user's idea; a sketch and its open
  questions in `docs/machines.md`, *Later*). Today a protocol mismatch
  marks the machine unreachable and a semver difference is a warning.
