# The host daemon: `devsandbox serve`

One per-user, per-host background process that owns the live host side of
devsandbox, so it keeps working with the dashboard and the terminal closed.
Read this first for anything in `src/serve/`; the siblings go deeper.

Code: `src/serve/mod.rs` (module list), `endpoint.rs` (`socket_dir`,
`Listener`, `connect`), `daemon.rs` (`serve`, `run`, `acquire_lock`,
`Shared`, `Exit`), `idle.rs` (`Holders`, `decide`, `IDLE_TIMEOUT`),
`client.rs` (`connect`, `ensure_running`, `spawn_detached`), `install.rs`
(`install_cli`, `uninstall_cli`, `lazy_start`, `plan`), `proto.rs`
(`Version`, `PROTOCOL`), `src/daemon_config.rs`, `src/exe.rs`
(`own_build_stamp`).

## What it owns

- **The one container poll** (`host.rs`): bridges to every running
  instance's helper (ssh-agent relay, outbox drain into `inbox.json`,
  desktop popups, in-container control ops), watched roots' snapshots,
  the startup `autostart = true` pass. → [host-poll.md](host-poll.md)
- **Verbs as requests**: `run`/`rebuild`/`start`/`stop`/`rm`/
  `services.rebuild`, each a `devsandbox --in-process` subprocess.
  → [verbs.md](verbs.md)
- **Port forwards**: every Ports-tab forward, ad-hoc and configured.
  → [forwards.md](forwards.md)
- **Machine links**: remote hosts over ssh. → [machines.md](machines.md)
- **The API** on its socket (JSON lines), used by the CLI, the dashboard
  and the npm package. → [client-server.md](client-server.md),
  [api.md](../../reference/api.md)

## Files are the truth

State lives on disk, next to `state.toml` in `<data-dir>/devsandbox/`
(`$XDG_DATA_HOME`, else `~/.local/share`): `state.toml` (instances),
`inbox.json`, `forwards.toml`, `machines.toml`, `daemon.config.toml`,
`logs/verbs/`. The daemon keeps only caches (snapshots, links, bridges) and
writes through the same locked read-modify-writes as the CLI. So any
command still works without a daemon: verbs fall back to running in the
CLI's process, Inbox mutations run locally, and a new daemon picks up
where the last one left off.

The user's own files are the client's to write: the example config
(`run::offer_example_config`) is offered and written by the CLI, never the
daemon. One exception: `config.migrate` (`config_identity::ops::migrate`)
rewrites a config root's config file in the daemon, because the dashboard
and machines have no other way to write it (a machine's config is
rewritten by that machine's daemon). It writes through `migrate_file`'s
`.bak` + tmp + rename, and only for a request naming the file's current
hash.

## Socket, lock, log

| file | where |
|---|---|
| `serve.sock` (0600) | socket dir: `$XDG_RUNTIME_DIR/devsandbox/`, else the state dir |
| `serve.lock` | socket dir |
| `serve.log` | state dir: `$XDG_STATE_HOME/devsandbox/`, else `~/.local/state/devsandbox/` |

Empty or relative XDG values are ignored. The socket dir is created (or
reset) 0700. The path carries no config root (macOS caps socket paths at
104 bytes): one daemon per user, config roots are request params. **Access
control is the file permissions**: whoever can connect can exec into every
container, like `docker.sock`. No TCP listener.

`serve.log` is appended to, never rotated: start (socket, pid, version,
keep-alive), handoff/shutdown, runtime lost/back, forward status lines,
exit, autostart notes.

## Lazy start

`devsandbox serve [--keep-alive] [--socket-dir]` runs in the foreground,
but it's plumbing: `client::connect` sends a `hello`, and when nothing
answers it starts one and polls the socket up to `START_TIMEOUT` (5 s).
Without a unit, `spawn_detached`: `setsid`, cwd `/`, stdin `/dev/null`,
output appended to `serve.log`. With a unit installed it goes through the
service manager (*Boot start*).

The daemon holds an exclusive lock on `serve.lock` for its life
(`acquire_lock`). A second daemon that finds it held and the socket
answering exits 0 (`Exit::AlreadyRunning`), so of two racing starts the
loser's client reaches the winner. A holder that doesn't answer yet
(starting, draining) is waited for up to `LOCK_WAIT` (10 s). With the
lock taken, a stale `serve.sock` is replaced.

Who starts one: the dashboard, the verbs' CLI side, `run`/`rebuild`/`start`
after they ran (`ensure_running`, a failure is one `warning:` line),
commands relaying the ssh-agent (`ensure_bridge`), `devsandbox api
--stdio`, `devsandbox machine …` and anything routed to a machine. Never
an `--in-process` process (`client::set_in_process`): the daemon's own
verb subprocesses, the CLI's fallback, the dispatcher's child ops. Never
`devsandbox inbox` (uses a running one only) or `devsandbox port`.

The daemon inherits its starter's environment (`DEVSANDBOX_RUNTIME`,
`SSH_AUTH_SOCK`, `DISPLAY`, `XDG_*`); a verb runs with the env sent in
its request instead ([verbs.md](verbs.md)).

## Version handoff

`Version` is the semver plus the binary's mtime (`exe::own_build_stamp`,
read at process start so an upgrade replacing the file doesn't change it),
compared as `(major, minor, patch, build)`: a rebuilt dev binary counts as
newer. When a `hello` is newer than the daemon, the daemon stops accepting
and removes `serve.sock`, answers `handoff: true`, drains, and exits. The
client then starts its own, which waits for the lock. Equal or older
clients proceed, but for a **forced** hello (`HelloParams::force`), which
hands off whatever its version: honored only on a local (`Full`)
connection whose peer runs as the daemon's uid
(`endpoint::peer_is_own_user`), never on a fleet hub's `Worker` one. A
fleet worker sends it after installing its hub's version, older or newer
([fleet](fleet.md#versions),
[ADR 0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md)).

## Holders and idle exit

The daemon exits `IDLE_TIMEOUT` (10 min) after its last holder left
(`idle::decide`, pure). `Holders` (filled in `Shared::holders`):

- `clients`: open connections (dashboard, `api --stdio` relay, a command
  holding a bridge);
- `forwards`: every live forward, ad-hoc or configured, whatever its state;
- `live_instances`: running instances declaring `dispatcher` or
  `inbox = true` (`host::holds_host`), as of the last poll;
- `control`: in-flight control requests (`devsbd events --follow` streams
  included);
- `verbs`: verb subprocesses not reaped (detached and adopted ones too)
  plus verbs forwarded to a machine while their ssh runs;
- `autostart`: the startup pass while it runs;
- `links`: the links this host dials, one per registered machine
  (`Kind::Ssh` entries of `machines::service::Handle::entries`): a
  member's daemon stays up while it's in a fleet, or its machines would
  lose it as an inbound neighbor. A machine dialing this host holds it
  through its `fleet-link`'s `links.inbound` connection (`clients`). `--keep-alive` or `[serve] keep-alive = true` in
`daemon.config.toml` (read once at start; a bad file is logged, defaults
used) disables the idle exit, as does `[serve] hub = true` (a
[fleet hub](fleet.md#hub-mode)).

## Drain

Idle exit, handoff and `shutdown` share one exit path (`daemon::run`):
listener and socket go, subscribers get `closing` (`reason`: `handoff`,
`shutdown`, `idle`), connections drain up to `DRAIN_TIMEOUT` (5 s), then the
machine links stop (ssh killed), then the bridges (`Host` dropped), then
the forwards, and the start lock last, so a successor never overlaps
bridges or forward ports. Local verbs in flight don't hold the drain and aren't
cancelled; the successor adopts them ([verbs.md](verbs.md)). A
non-`detach` verb forwarded to a machine is the exception: killing its ssh
cancels it on the remote ([machines.md](machines.md)). Ad-hoc forwards
come back from `forwards.toml` ([forwards.md](forwards.md)).

## Boot start: `serve install` / `uninstall`

Opt-in: hands the daemon to the user's service manager so it runs from
login (what `autostart = "runtime"` containers need: a container can't
start a host process).

| | unit | manager calls |
|---|---|---|
| Linux | `$XDG_CONFIG_HOME/systemd/user/devsandbox.service` (else `~/.config/…`) | `systemctl --user daemon-reload`, `enable`, `restart` |
| macOS | `~/Library/LaunchAgents/dev.devsandbox.serve.plist` | `launchctl bootout` (failure ignored), `bootstrap` (`BOOTSTRAP_ATTEMPTS` = 3) |

Other unix systems get an error. The unit runs the installing binary's
absolute, symlink-resolved path with `serve --keep-alive --socket-dir
<dir>`, output appended to `serve.log`, and carries `CAPTURED_ENV` as set
at install time: `PATH`, `DEVSANDBOX_RUNTIME`, `DOCKER_HOST`,
`DOCKER_CONTEXT`, `CONTAINER_HOST`, `XDG_DATA_HOME`, `XDG_STATE_HOME`. Not
`SSH_AUTH_SOCK` (clients report theirs via `bridges.ensure`), not
`DISPLAY`.

`install`: write the unit; if a daemon answers, send it `shutdown` and wait
up to `STOP_WAIT` (10 s); the manager calls; wait up to `START_WAIT` (5 s)
for an answer. Restart policy `Restart=on-failure` / `KeepAlive
{SuccessfulExit false}`: a handoff exits 0 and must not respawn the stale
binary.

With a unit installed, every lazy start goes through `install::plan`: a
handoff always rewrites the unit from the client's binary and env (newest
binary wins, dev builds included) then restarts it; a plain start leaves a
working unit alone and only rewrites one whose binary is gone. Any manager
failure is one `serve.log` line and a detached start instead.

`uninstall`: systemd `disable --now` + remove + `daemon-reload`; launchd
`bootout` + remove. The manager stops the daemon by signal, so clients see
no `closing`. `tests/service_manager.rs` runs against the real managers
only with `DEVSANDBOX_SERVICE_MANAGER_E2E=1`.

## Platform

Unix only: see [platform-support.md](platform-support.md).

Limitations: [status.md](../../status.md#daemon)
