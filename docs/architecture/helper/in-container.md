# Inside the container: daemon, boot, outbox, control, runs

What `devsbd` does once installed: the long-lived `devsbd daemon`, the boot
hook, and the verbs scripts and dispatchers call. Read when changing
helper-side behavior or debugging a sandbox from the inside.

Code: `devsbd/src/daemon.rs` (`run`, `take_over`, `Bridges::route`,
`flush`, `control_exchange`, `control_follow`), `devsbd/src/bridge.rs`,
`devsbd/src/boot.rs`, `devsbd/src/outbox.rs`, `devsbd/src/ctl.rs`,
`devsbd/src/runs.rs`, `devsbd/src/vscode.rs`, `devsbd/src/json.rs`;
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

`/run` content is lost when an image mounts a tmpfs there; `/var/lib` sits in
the writable layer, so outbox and runs survive a restart, not a rebuild.

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

## `devsbd vscode-goto <abs>[:line[:col]] [--wait SECS]`

Opens a file in the VS Code window attached to this container by running
the VS Code server's remote CLI (`code -g`) against the window's IPC socket:
the `/tmp/vscode-ipc-*.sock` held by the newest `--type=extensionHost`
process. Root that can't read that process's fds re-runs itself as its
owner. Exit 3 = no window attached within the wait. Called by `devsandbox
vscode` (`src/commands/vscode.rs`).

## `devsbd features`

Prints the append-only list of CLI capabilities (`thread-send`,
`thread-withdraw`, `thread-ls-feed`, `events-follow`) a dispatcher may probe
before relying on them.

Limitations: [status.md](../../status.md#helper)
