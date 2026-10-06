# `devsandbox serve`: the host daemon

A per-user, per-host background process that owns the live host side of
devsandbox (bridges, popups, control ops, port forwards, the API) so it
keeps working with the dashboard closed. Design and roadmap:
`docs/inbox-redesign.md`, "Host daemon". Unix only; the command doesn't
exist on Windows.

This page covers what exists today (steps 4-10 of that plan): the socket,
start, handoff, idle exit, the bridges (the ssh-agent relay included), the
port forwards, the startup autostart pass and boot start. The
API on the socket has its own page, `docs/api.md`. Marked below is what
later steps add.

Code: `src/serve/` (`endpoint.rs` paths + bind/connect/accept, `daemon.rs`,
`host.rs` bridges + autostart, `forwards.rs` port forwards, `api.rs`
methods, `client.rs`, `relay.rs` the `api --stdio` relay, `install.rs`
`serve install|uninstall`, `idle.rs`, `proto.rs`).

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
stderr, the command still succeeds; they hang up right away), and every
command that relays the ssh-agent into a relay-mode instance (`exec`,
`start`'s `postStartCommand`, `run`'s lifecycle chain) for its bridge (see
*Bridges*), and `devsandbox api --stdio` (the relay external clients and
the npm package spawn; it stays connected while it runs, see `docs/api.md`,
*Relay*). `inbox` follows in step 19 (the foreground `port` command
doesn't use the daemon).

The daemon inherits the environment of the process that started it: the
runtime choice (`DEVSANDBOX_RUNTIME`), `SSH_AUTH_SOCK` (the agent its bridges
relay until a client reports one, see *Bridges*), `DISPLAY` /
`DBUS_SESSION_BUS_ADDRESS` (popups), `XDG_*`. A daemon the service manager
starts has the manager's environment plus what `serve install` captured
instead (see *Boot start*).

## Bridges

The daemon keeps one bridge per running instance with a helper, the way the
dashboard did until now (`devsbd::bridge::Bridges`): it lists the runtime's
running containers every 5 s and reconciles. Each bridge relays the host
ssh-agent (when there is one), drains the container's outbox (`devsbd
notify`, `devsbd thread put|rm`) into `inbox.toml`, shows desktop popups
(rate-limited per instance), and serves the dispatcher's control ops
(`ensure`, `exec`, `run …`, `events`, `thread ls`). Before each (re)spawn
it reinstalls a stale helper, so a running dispatcher gets new `devsbd`
verbs once a newer daemon runs. The dashboard no longer bridges, and neither
do CLI commands.

**ssh-agent through the daemon.** A command that injects `SSH_AUTH_SOCK`
into a relay-mode instance (`exec`, `start`'s `postStartCommand`, `run`'s
lifecycle chain, a dashboard terminal) sends `bridges.ensure` (`docs/api.md`)
with its own `$SSH_AUTH_SOCK`. The daemon puts that agent first in its
candidates (up to 8 reported agents, newest first, then its inherited
`$SSH_AUTH_SOCK`), wakes the poll for that container (a dead bridge there is
respawned at once, retry gap or not) and answers without waiting for the
bridge, unless asked to `wait`: then once the bridge's handshake is decided
(`run`'s and `start`'s lifecycle commands wait, up to 12 s, before they
start). Each agent stream connects to the first candidate that accepts a
connection; a bridge spawned while no candidate was live is replaced once
one is. So the relay follows whichever session last ran a command, and
survives the agent the daemon was started with going away. A CLI command
keeps its connection open while it runs, so it's a holder; a relay that
isn't there is one `note:` line (after an `exec`, before a lifecycle chain).

With the runtime unreachable, the last list stands (bridges and holders are
kept, not torn down on a blip) and `serve.log` gets one line; another when
it answers again.

`events --wait` wakes as soon as the daemon itself writes the store (the
notify sink, an ack, a connected dashboard's clicks through the API), and
re-checks the file every 500 ms for writes by other processes (a dashboard
without a daemon connection, `devsandbox rm`).

Each message the sink stores that earns a status line also goes to `inbox`
subscribers as `inbox.shown`: that's the dashboard's status line.

## Forwards

The daemon owns every Ports-tab forward (`docs/port-forwarding.md`), on its
own `serve-forwards` thread: ad-hoc ones added through `forwards.add` (the
dashboard's `p` / `:port`), and every running instance's configured
`forwardPorts`, for every config root recorded in `state.toml`. The
configured ones follow the 5 s container poll: started on the host port
saved in `state.toml` when their instance runs, stopped when it stops, so
they work with no dashboard open. Each forward keeps its own bridge (an
`exec -i … devsbd bridge` of its own, self-healing), separate from the
instance's daemon bridge; sharing that bridge's mux is a later
optimization. Status lines (a configured forward started or failed, a
connection note) go to `forwards` subscribers as `forwards.status` and to
`serve.log`. The foreground `devsandbox port` command still forwards in its
own process, without the daemon.

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
dashboard, a running `exec` that relays the agent, an `api --stdio`
relay), running instances that declare `dispatcher` (as of the last
poll; `inbox = true` joins in step 11), in-flight control requests, every
live port forward (ad-hoc or configured, whatever its state), and the
startup autostart pass while it runs. So with a dispatcher running the
daemon never idles, and by design neither does it while an instance with
`forwardPorts` runs, or an ad-hoc forward exists (stop it in the Ports tab
with `d`). A later step adds `--follow` subscribers (step 18).

Exiting (idle or handoff) kills every bridge and closes every forward
before the start lock is released, so a successor's bridges never overlap
and it can bind the configured forwards' saved host ports again; it
restarts them on its first poll. Ad-hoc forwards are lost on a handoff
(the dashboard's Ports tab shows them gone; add them again). A running
autostart pass is waited for.

`--keep-alive` disables the idle exit; so does the per-user global config
`<data-dir>/devsandbox/daemon.config.toml` (next to `state.toml`;
`$XDG_DATA_HOME` or `~/.local/share`):

```toml
[serve]
keep-alive = true
```

The effective value is the flag OR the file, read once at daemon start
(the start log line says `keep-alive` when on). A missing file is the
defaults; unknown keys are ignored; a file that doesn't parse is logged as
a warning to `serve.log` and the defaults are used. The unit
`devsandbox serve install` writes passes `--keep-alive` (*Boot start*).

Exiting on a `shutdown` request (`docs/api.md`) is the same drain, with
`closing` `"reason":"shutdown"`.

## Boot start

Opt-in: `devsandbox serve install` hands the daemon to the user's service
manager, so it runs from login without any devsandbox command. That's what
dispatcher / inbox containers the runtime restarts by itself
(`autostart = "runtime"`) need: a container can't start a host process, so
without it they wait (exit 75, retrying) until the first `devsandbox`
command. It's a *user* service: it starts when the user's session (systemd
user manager / launchd `gui` domain) does, i.e. at login; on Linux,
`loginctl enable-linger` makes that boot, and `serve install` doesn't do it
for you.

| | unit file | manager calls |
|---|---|---|
| Linux | `$XDG_CONFIG_HOME/systemd/user/devsandbox.service`, else `~/.config/systemd/user/` | `systemctl --user daemon-reload`, `enable devsandbox.service`, `restart devsandbox.service` |
| macOS | `~/Library/LaunchAgents/dev.devsandbox.serve.plist` (label `dev.devsandbox.serve`) | `launchctl bootout gui/<uid>/dev.devsandbox.serve` (failure ignored), `launchctl bootstrap gui/<uid> <plist>` (retried up to 3 times) |

Other unix systems get an error. The unit runs the installing binary's
absolute, symlink-resolved path with `serve --keep-alive --socket-dir
<dir>`, the socket dir resolved at install time, and appends stdout and
stderr to `serve.log` (systemd `StandardOutput=append:`, launchd
`StandardOutPath`).

**Captured environment.** systemd and launchd start services with a
minimal `PATH`, so the unit carries, as set (and non-empty) when you ran
`install`: `PATH`, `DEVSANDBOX_RUNTIME`, `DOCKER_HOST`, `DOCKER_CONTEXT`,
`CONTAINER_HOST`, `XDG_DATA_HOME`, `XDG_STATE_HOME` (the last two keep the
managed daemon on the same `state.toml`, `inbox.toml` and `serve.log` as
your shell's commands). Not `SSH_AUTH_SOCK`: it rotates, and clients report
theirs (`bridges.ensure`, *Bridges*). Not `DISPLAY` either: popups from a
managed daemon need the manager to have it (`systemctl --user
import-environment DISPLAY`). **Run `serve install` again** after changing
`PATH`, the runtime or its context, or the binary's location (a package
manager upgrade that moves the resolved path, e.g. Homebrew's versioned
`Cellar` dir, breaks the unit until you do).

**Install steps.** Write the unit (creating its dir and the `serve.log`
dir). If a daemon answers on the socket, managed or started on demand, send
it `shutdown` and wait (up to 10 s) until nothing answers, so the manager's
daemon can bind (an older daemon already hands off on the hello; one that
answers `shutdown` with an error is reported: stop it, run `install`
again). Then the manager calls above, then wait (up to 5 s)
for a daemon to answer and print where the unit is. Not answering in time,
or a manager call failing, is an error naming the unit file it wrote.
Running `install` again rewrites the unit and restarts the daemon.

**Restart policy.** `Restart=on-failure` (`RestartSec=5`) /
`KeepAlive {SuccessfulExit false}` + `RunAtLoad`: a crash restarts it, a
clean exit doesn't. Deliberately: a version handoff exits 0, and an
`always` policy would respawn the stale binary against the newer daemon in
a loop. So would a manager-started daemon finding another one already
answering (it exits 0 at once, *Start*).

**Handoff with a managed daemon.** When a newer CLI connects (an upgrade, a
rebuilt dev binary), the managed daemon hands off as usual and exits 0; the
newer client starts its own daemon *on demand*: unmanaged, without
`--keep-alive`, so it idles out like any other. The unit stays stopped
(systemd shows it inactive, not failed) until the next login or
`devsandbox serve install`, which also points the unit at the new binary.
Run `install` after every upgrade.

**Uninstall.** `devsandbox serve uninstall`: on Linux `systemctl --user
disable --now devsandbox.service`, remove the file, `daemon-reload` (a
failing `systemctl` is a warning; the file is removed anyway); on macOS
`launchctl bootout gui/<uid>/dev.devsandbox.serve`, remove the plist. The
manager stops its daemon with a signal, not the drain: clients see the
connection drop without a `closing` notification, and reconnect starting
one on demand. A daemon started on demand isn't touched. No unit file:
one line saying it's not installed, exit 0.

## Log

`serve.log` gets one timestamped line (`[<unix secs>] devsandbox serve: …`)
at start (socket, pid, version, keep-alive), at a handoff or shutdown request, when the
runtime stops or starts answering, at each forwards status line
(`forwards: …`), and at exit; plus the autostart pass's own notes
(untimestamped). It's appended to, never rotated.
