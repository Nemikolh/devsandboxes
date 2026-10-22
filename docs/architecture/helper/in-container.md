# Inside the container: daemon, boot, outbox, control, runs, sessions

What `devsbd` does once installed: the long-lived `devsbd daemon`, the boot
hook, and the verbs scripts and dispatchers call. Read when changing
helper-side behavior or debugging a sandbox from the inside.

Code: `devsbd/src/daemon.rs` (`run`, `take_over`, `Bridges::route`,
`flush`, `control_exchange`, `control_follow`), `devsbd/src/bridge.rs`,
`devsbd/src/boot.rs`, `devsbd/src/outbox.rs`, `devsbd/src/ctl.rs`,
`devsbd/src/runs.rs`, `devsbd/src/sessions.rs`, `devsbd/src/vscode.rs`, `devsbd/src/json.rs`,
`devsbd/src/identity.rs`;
`src/commands/run/mod.rs` (`container_command`, `BOOT_HOOK_LABEL`).

## Files and sockets

| path | what |
|---|---|
| `/run/devsandbox/bin/devsbd` | the binary (symlinked as `/usr/local/bin/devsbd`) |
| `/run/devsandbox/devsbd.ctl` (0600) | bridges attach here (root only) |
| `/run/devsandbox/ssh-agent.sock` (0666) | agent socket for container processes |
| `/run/devsandbox/api.sock` (0666) | in-container clients: first byte `n` = outbox poke, `c` = control request |
| `/run/devsandbox/devsbd.pid` | flock + `<pid> <build hash>` |
| `/run/devsandbox/boot`, `boot.log` | boot file (host-written) and its commands' output |
| `/var/lib/devsandbox/outbox/` (1777) | queued notify/thread records |
| `/var/lib/devsandbox/runs/` (1777) | tracked runs |
| `/var/lib/devsandbox/sessions/` (1777) | interactive sessions; `<id>/sock` (0600) is a session's socket |

`/run` content is lost when an image mounts a tmpfs there; `/var/lib` sits in
the writable layer, so outbox, runs and sessions' records survive a restart, not a rebuild (a
session's processes don't: it reads `lost`).

## `devsbd daemon`

One per container per build, started by the host (`exec -d -u root …
devsbd daemon` on install), by `devsbd boot`, or by a `devsbd bridge` that
finds no daemon listening. A second daemon of the same build exits 0; one of
a different build sends `Quit` to the running one and takes over
(see [protocol.md](protocol.md#daemon-takeover)).

It binds `devsbd.ctl`, `ssh-agent.sock` and `api.sock` (removing stale
socket files first). Binding the agent socket fails in mount mode, where the
host agent is bind-mounted at that path; the daemon then serves no agent
streams and everything else as usual.

- **Bridges**: each `devsbd bridge` connection is a `Hello` then a mux; the
  daemon keeps a list, oldest first, and routes every new stream to the
  newest bridge whose host serves that channel. A short-lived bridge ending
  falls back to an older one. Keepalive drops a wedged bridge.
- **Agent clients**: each connection on `ssh-agent.sock` becomes an
  `Open(SSH_AGENT)` on the newest agent-capable bridge, held up to 1 s
  (`HOLD`) for one to appear; none → the connection closes (ssh sees a
  refusing agent).
- **`Connect`**: dials `host:port` inside the container for port forwards.
- **Outbox flusher**: sends queued records oldest first, one `Open(NOTIFY)`
  each, deleting a file only after the host's `ok` and stopping at the first
  failure so order holds. Woken by a poke on `api.sock`, a bridge attaching
  or its `Caps` arriving, and every 30 s. A file it can't parse is renamed
  `*.bad` and skipped.
- **Control relay**: a `c` client on `api.sock` sends one encoded request;
  the daemon relays it on `Open(CONTROL)` to the newest `CONTROL`-capable
  bridge and returns the host's response verbatim. No such bridge → `no-host`
  at once; no reply within 30 min or a dying bridge → `failed`.
  `events-follow` is relayed as a stream until either side closes or 90 s
  of silence (`FOLLOW_SILENCE`).

## `devsbd bridge`

Run by the host over `exec -i`. Connects to `devsbd.ctl` (spawning a daemon
once if nothing listens: the self-heal for containers restarted outside
devsandbox), handshakes with the daemon, then with the host, then copies bytes
both ways. Exits `MISMATCH_EXIT` (3) when the daemon speaks another protocol
version. Details: [protocol.md](protocol.md#transport-path).

## `devsbd boot`

The container command is `sh -c '[ -x <BIN> ] && <BIN> boot & exec sleep
infinity'` (containers created with it carry the label
`devsandbox.boot_hook=1`; on first create the helper isn't installed yet, so
the hook is a no-op). When the runtime restarts the container by itself,
`boot` starts a detached daemon, then, if `/run/devsandbox/boot` exists, runs
its `cmd`s in order (`cwd`, `env`, and `user` when running as root), stopping
at the first failure like the host's lifecycle chain, appending output to
`boot.log`. The host writes the boot file only for `autostart = "runtime"`
on a backend with restart policies ([bridges.md](bridges.md#boot-hook-host-role)).
In that mode `start` leaves `postStartCommand` to the hook; a container
without the label (created before it) gets it from the host, with a warning
to `rebuild --force`.

## Outbox: `devsbd notify`, `devsbd thread put|send|withdraw|rm`

Queue a record (format: [protocol.md](protocol.md#notify)) as one file in
the outbox, named `<secs>-<nanos>-<pid>` so name order is queue order, then
poke the daemon. Works with no host attached: records wait. `thread put` and
`thread send` take JSON (`--json` or stdin); `json.rs` syntax-checks it and
extracts `key` / `thread` + `id`, nothing more. Thread records coalesce with
queued ones they supersede (`Message::supersedes`: a newer put of the same
key, an rm over that key's put/send/withdraw, a send/withdraw over the same
`(thread, id)`), taking the oldest replaced file's place (`<base>+<n>`) so
order is kept. Plain notify records are never dropped. `thread ls` is a
control request instead (below).

## Control verbs (`ctl.rs`)

`ensure`, `ls`, `branches`, `stop`, `rm`, `done`, `exec`, `run
ls|logs|wait|rm|prune <key>`, `events [--wait|--follow] [--thread]`,
`events ack`, `thread ls [--feed]`: each builds one `control::Request`,
sends it over `api.sock`, prints the body, and exits with the status's code
(0 ok, 1 failed, 2 usage, 75 no host, 77 denied). Child ops need the
sandbox to declare `dispatcher`, thread/event ops `inbox = true`; the host
decides (`src/commands/dispatch.rs`, via [bridges.md](bridges.md)).

Long operations loop over short requests rather than holding one stream:
`exec` without `--detach` and `run logs --follow` poll in 5 s steps,
`run wait` in 60 s steps. `events --follow` is the exception, one held-open
`events-follow` stream copied to stdout.

## Runs: `devsbd run start|ls|logs|wait|rm|prune`

Tracked commands in this container, one dir per run under
`/var/lib/devsandbox/runs/<id>/` (`argv`, `out.log`, `meta`). `start`
spawns a detached `devsbd run supervise <id>` (no libc, so no fork), which
runs the command in its own process group and records `ended` + `exit` or
`signal`. States: `running`, `exited N`, `killed N`, `lost` (supervisor gone
without an end, e.g. container restart). Runs stay until `rm`/`prune`.

`runs::is_local` routes: with a child `<key>` positional (`run ls <key>`,
`run logs <key> <id>`, …) the verb is a control request to the host, which
execs the same `devsbd run …` inside the child as its remote user
(`dispatch` `exec`/`run-*` ops); without one it acts on this container.

## Sessions: `devsbd session new|attach|ls|rm`

Interactive commands that outlive their client, dtach's shape
(`devsbd/src/sessions.rs`). One dir per session under
`/var/lib/devsandbox/sessions/<id>/`: `argv` and `meta` in the runs format
(the bookkeeping is `runs.rs`'s, over this root), `sock` (0600) while the
command runs, `client` while a client is attached. The id is the caller's
(`control::valid_run_id` rules).

The host starts them: `devsandbox exec` on an instance with the helper runs
every tty exec as `<runtime> exec -it … <container> /run/devsandbox/bin/devsbd
session new --id <id> -- <cmd>...` (`commands::exec::session_for`,
`session_new_command`), with a fresh id (`mint_session_id`) or
`--session`'s; the runtime exec's `-w`/`-u`/`-e` give the supervisor the
cwd and env the command gets, so `new` needs no `--cwd`. `exec --attach
[<id>]` runs `session attach`, `exec --sessions` `session ls`.

- `new --id <id> [--cwd DIR] -- <cmd>...` (`create_with`) creates the dir
  and spawns a detached `devsbd session supervise <id>` (its own session,
  null stdio, in `DIR` with the caller's env), then attaches; an id that
  already exists attaches instead. `attach <id>` only attaches.
- The supervisor (`supervise`) binds `sock`, opens a pty (`open_pty`,
  libc through `extern "C"`, per-OS ioctl numbers in `sessions::sys`) and
  runs the command on it as a session leader with the pty as its
  controlling terminal, so its pid is its pgid. One `poll` loop over
  non-blocking fds (`Supervisor::relay`): the pty is always read, its
  output queued for the client up to `OUT_CAP` and dropped past it or with
  no client, so neither the program nor the supervisor ever waits on a
  client. One client at a time: an accepted connection takes over and the
  previous client gets `detached`. A client's first size is applied one row
  off, then as is, so the program gets `SIGWINCH` and redraws (there is no
  output log). When the command exits: `sock` removed, `ended` + `exit` /
  `signal` appended to `meta`, `exit` sent to the client, supervisor gone.
- The client (`terminal`, `relay`) puts its stdin terminal raw (restored on
  return and by a panic hook), sends its size at once and whenever a
  `SIZE_POLL` check sees it change, relays bytes both ways, and exits with
  the command's code (`128 + n` for signal `n`). Without a terminal on
  stdin it relays plain bytes and sends no size. An ended session's attach
  exits at once with its recorded code; an unknown or `lost` one, or a
  connection that ends without a record, exits `NO_SESSION_EXIT` (250);
  one taken over exits `DETACHED_EXIT` (251); both with a note on stderr.
- Socket records (`Record`): `u8 tag | u32 len | payload`; `size`
  (`u16 cols, u16 rows`) and `data` from the client, `data`, `exit`
  (`i32`) and `detached` from the supervisor. Only the helper speaks it.
- `ls`: `<id> <state> <attached|detached|-> <started, UTC> <argv…>`, the
  newest `runs::MAX_LS_RUNS`, states as in `run ls`. `rm <id>`: a running
  session's command group gets SIGHUP, then SIGKILL after `HUP_WAIT`
  (verified with `runs::kill_target`), the supervisor records the end, the
  dir is deleted.

## `devsbd identity apply`

`devsbd identity apply <user> [--remap UID:GID | --expect UID:GID |
--root-home] [--socket-gid GID] [--volume PATH]...` (`identity::run`) fits
the adopted account to the identity plan, as root, once per container; the
edits live in the container layer and a repeat run changes nothing. `<user>`
is an account name, else a uid. At most one mode:

- none: ids kept;
- `--remap UID:GID`: the account moves to those ids (`remap_passwd`,
  `remap_group`). `/etc/passwd` and `/etc/group` are rewritten directly, no
  `usermod`. Another account holding the uid moves to the lowest free uid
  from 1000 (not the account's old one); its files keep their numeric owner.
  A group that already has the gid becomes the account's primary group as
  is, else the account's own group is renumbered, else one is added. uid 0,
  or an account that is uid 0, is refused. `/etc/shadow` and
  `/etc/gshadow` are name-keyed and nothing is renamed, so they're left alone;
- `--expect UID:GID`: ids kept, a warning on stderr when the account's real
  ids differ (keep-id: workspace files would be owned by a subuid);
- `--root-home`: ids kept, the home goes to `0:0` (rootless docker: root is
  the host user).

The home (from `/etc/passwd`) is walked (`chown_tree`, per entry `step`):
only entries owned by the account's old uid change owner, their group too
when it's the old gid; symlinks are `lchown`ed, never followed; an entry on
another device than the home (a bind mount or volume) is skipped and not
entered. The walk runs before the `/etc` writes, so a run cut short is
finished by the next. `--socket-gid` puts the account in the group with that
gid, adding `docker` (or `docker-<gid>` when the name is taken) if none has
it (`add_member`). `--volume PATH` hands a root-owned, empty mount point to
the account (`claim_volume`); a missing one is a warning. `/etc` files are
replaced by a temp file renamed over them, keeping mode and owner
(`write_replacing`), and only when their content changes.

Stdout is one line, `home=<path>`, the account's home. Exit 0 on success
(warnings on stderr), 1 with `devsbd: identity: <why>` on stderr when it
fails (no such account, a refused remap, an I/O error), 2 on a usage error.

## `devsbd vscode-goto <abs>[:line[:col]] [--wait SECS]`

Opens a file in the VS Code window attached to this container by running
the VS Code server's remote CLI (`code -g`) against the window's IPC socket:
the `/tmp/vscode-ipc-*.sock` held by the newest `--type=extensionHost`
process. Root that can't read that process's fds re-runs itself as its
owner. Exit 3 = no window attached within the wait. Called by `devsandbox
vscode` (`src/commands/vscode.rs`).

## `devsbd features`

Prints the append-only list of CLI capabilities (`thread-send`,
`thread-withdraw`, `thread-ls-feed`, `events-follow`, `session`) a dispatcher may probe
before relying on them.

Limitations: [status.md](../../status.md#helper)
