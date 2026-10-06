# `devsandbox serve`: the host daemon

A per-user, per-host background process that owns the live host side of
devsandbox (bridges, popups, control ops, the API; later forwards) so it
keeps working with the dashboard closed. Design and roadmap:
`docs/inbox-redesign.md`, "Host daemon". Unix only; the command doesn't
exist on Windows.

This page covers what exists today (steps 4-7 of that plan): the socket,
start, handoff, idle exit, the bridges and the startup autostart pass. The
API on the socket has its own page, `docs/api.md`. Marked below is what
later steps add.

Code: `src/serve/` (`endpoint.rs` paths + bind/connect/accept, `daemon.rs`,
`host.rs` bridges + autostart, `api.rs` methods, `client.rs`, `idle.rs`,
`proto.rs`).

## Paths and permissions

| file | where |
|---|---|
| `serve.sock` (0600) | the socket dir: `$XDG_RUNTIME_DIR/devsandbox/`, else `$XDG_STATE_HOME/devsandbox/`, else `~/.local/state/devsandbox/` |
| `serve.lock` | the socket dir |
| `serve.log` | the state dir: `$XDG_STATE_HOME/devsandbox/`, else `~/.local/state/devsandbox/` |

The daemon creates the socket dir 0700 (and resets it to 0700 if it was
looser). Empty or relative XDG values are ignored. The socket path stays
short on purpose (macOS caps socket paths at 104 bytes): one daemon per
user, no config root in the path.

**Access control is the file permissions.** Whoever can connect to the
socket can (once the API lands) exec into every container, like
`docker.sock`. There is no TCP listener.

## Start

`devsandbox serve [--keep-alive]` runs the daemon in the foreground. It's
plumbing: commands start it themselves (*lazy start*):

1. The client connects to `serve.sock` and sends a `hello`.
2. Nothing answers: it spawns `devsandbox serve` detached (`setsid`, cwd `/`,
   stdin `/dev/null`, stdout+stderr appended to `serve.log`) and polls the
   socket for up to 5 s. It starts at most one daemon per attempt.
3. **Races:** the daemon holds an exclusive lock on `serve.lock` for its whole
   life. A second daemon that finds the lock held and the socket answering
   exits 0 quietly, so of two concurrent starts the loser's client connects
   to the winner. A holder that doesn't answer yet (starting, or draining
   after a handoff) is waited for, up to 10 s. With the lock taken, a stale
   `serve.sock` from a crashed daemon is replaced.

Who starts it today: the dashboard (at launch, on a background thread; it
keeps the connection open for its whole life and reconnects, starting a
new daemon if needed, after a handoff or the daemon's death; see
`docs/api.md`, *The dashboard as a client*), and `run` / `start` once
their container is up (best effort: a failure is one `warning:` line on
stderr, the command still succeeds; they hang up right away). `exec`,
`port` and `inbox` follow in steps 6-8; `devsandbox api --stdio` in step 9.

The daemon inherits the environment of the process that started it: the
runtime choice (`DEVSANDBOX_RUNTIME`), `SSH_AUTH_SOCK` (the agent its bridges
relay), `DISPLAY` / `DBUS_SESSION_BUS_ADDRESS` (popups), `XDG_*`.

## Bridges

The daemon keeps one bridge per running instance with a helper, the way the
dashboard did until now (`devsbd::bridge::Bridges`): it lists the runtime's
running containers every 5 s and reconciles. Each bridge relays the host
ssh-agent (when there is one), drains the container's outbox (`devsbd
notify`, `devsbd thread put|rm`) into `inbox.toml`, shows desktop popups
(rate-limited per instance), and serves the dispatcher's control ops
(`ensure`, `exec`, `run …`, `events`, `thread ls`). Before each (re)spawn
it reinstalls a stale helper, so a running dispatcher gets new `devsbd`
verbs once a newer daemon runs. The dashboard no longer bridges.

With the runtime unreachable, the last list stands (bridges and holders are
kept, not torn down on a blip) and `serve.log` gets one line; another when
it answers again.

`events --wait` wakes as soon as the daemon itself writes the store (the
notify sink, an ack, a connected dashboard's clicks through the API), and
re-checks the file every 500 ms for writes by other processes (a dashboard
without a daemon connection, `devsandbox rm`).

Each message the sink stores that earns a status line also goes to `inbox`
subscribers as `inbox.shown`: that's the dashboard's status line.

## Autostart

At start the daemon runs the `autostart = true` pass
(`docs/automations-guide.md`) once for every config root recorded in
`state.toml`, on a background thread; its notes go to `serve.log`. The pass
is once per boot per root, so the dashboard, `run` and `start` still calling
it is harmless.

## Wire

JSON lines; every request carries an `id` that the response echoes. The
first request is the handshake:

```
→ {"id":0,"method":"hello","params":{"version":"0.6.0","build":1767225600,"client":"tui"}}
← {"id":0,"result":{"version":"0.6.0","build":1767225600,"protocol":1}}
```

`build` is the binary's mtime in unix seconds, so a rebuilt dev binary with
the same version counts as newer. The methods, errors and notifications
after the hello: `docs/api.md`.

## Version handoff

Versions compare as `(major, minor, patch, build)`. When a client's `hello`
is newer than the daemon, the daemon:

1. stops accepting and removes `serve.sock`,
2. answers `{"result":{…,"handoff":true}}`,
3. closes idle connections and lets in-flight requests finish (up to 5 s),
4. exits, releasing the lock.

The client then spawns its own daemon, which waits for the lock and takes
over. Older and equal clients just proceed. Connections that subscribed to
notifications get a `closing` notification (`"reason":"handoff"`) before
the close.

## Idle exit

The daemon exits 10 minutes after its last *holder* left; a new holder
cancels the countdown. Holders today: connected clients (an open
dashboard), running instances that declare `dispatcher` (as of the last
poll; `inbox = true` joins in step 11), in-flight control requests, and the
startup autostart pass while it runs. So with a dispatcher running the
daemon never idles. Later steps add port forwards (step 8) and `--follow`
subscribers (step 18).

Exiting (idle or handoff) kills every bridge before the start lock is
released, so a successor's bridges never overlap. A running autostart pass
is waited for.

`--keep-alive` disables the idle exit. A `serve.keep-alive` setting in a
global config is planned but there's no global config yet; `devsandbox serve
install` (step 10, systemd user unit / LaunchAgent) will imply keep-alive.

## Log

`serve.log` gets one timestamped line (`[<unix secs>] devsandbox serve: …`)
at start (socket, pid, version, keep-alive), at a handoff request, when the
runtime stops or starts answering, and at exit; plus the autostart pass's
own notes (untimestamped). It's appended to, never rotated.
