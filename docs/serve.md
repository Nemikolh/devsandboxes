# `devsandbox serve`: the host daemon

A per-user, per-host background process that will own the live host side of
devsandbox (bridges, forwards, popups, the API) so it keeps working with the
dashboard closed. Design and roadmap: `docs/inbox-redesign.md`, "Host
daemon". Unix only; the command doesn't exist on Windows.

This page covers what exists today (step 4 of that plan): the socket, start,
handoff and idle exit. The daemon doesn't do anything useful yet beyond
answering `hello`; marked below is what later steps add.

Code: `src/serve/` (`endpoint.rs` paths + bind/connect/accept, `daemon.rs`,
`client.rs`, `idle.rs`, `proto.rs`).

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

No command calls the daemon yet: the TUI, `exec`, `start`, `port` and
`inbox` start using it in steps 5-8; `devsandbox api --stdio` comes in
step 9.

## Wire

JSON lines; every request carries an `id` that the response echoes. The
first request is the handshake:

```
→ {"id":0,"method":"hello","params":{"version":"0.6.0","build":1767225600,"client":"tui"}}
← {"id":0,"result":{"version":"0.6.0","build":1767225600}}
```

`build` is the binary's mtime in unix seconds, so a rebuilt dev binary with
the same version counts as newer. Any other method answers
`{"id":n,"error":{"code":"unknown-method","message":"…"}}` until step 6 adds
the API; malformed lines answer `invalid`.

## Version handoff

Versions compare as `(major, minor, patch, build)`. When a client's `hello`
is newer than the daemon, the daemon:

1. stops accepting and removes `serve.sock`,
2. answers `{"result":{…,"handoff":true}}`,
3. closes idle connections and lets in-flight requests finish (up to 5 s),
4. exits, releasing the lock.

The client then spawns its own daemon, which waits for the lock and takes
over. Older and equal clients just proceed. (Step 6: subscribers get a
reconnect hint before the close.)

## Idle exit

The daemon exits 10 minutes after its last *holder* left; a new holder
cancels the countdown. Holders today: connected clients. Later steps add
running `dispatcher`/`inbox` instances and in-flight control requests
(step 5), port forwards (step 8) and `--follow` subscribers (step 18).

`--keep-alive` disables the idle exit. A `serve.keep-alive` setting in a
global config is planned but there's no global config yet; `devsandbox serve
install` (step 10, systemd user unit / LaunchAgent) will imply keep-alive.

## Log

`serve.log` gets one timestamped line (`[<unix secs>] devsandbox serve: …`)
at start (socket, pid, version, keep-alive), at a handoff request and at
exit. It's appended to, never rotated.
