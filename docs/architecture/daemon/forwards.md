# Port forwarding

Host TCP ports tunnelled into an instance (or one of its services) through
the in-container helper, without publishing container ports. Two owners
share one engine: the daemon (Ports-tab ad-hoc forwards and configured
`forwardPorts`) and the foreground `devsandbox port` command. Read this
before touching forwards, their ports or their persistence.

Code: `src/devsbd/forward.rs` (`Forward`, `ForwardSpec`, `Opener`,
`Opened`, `ProcessLookup`, `local_opener`, `HostPort`, `PREFER_SPAN`,
`ForwardState`, `bind_listener`), `src/commands/port.rs`
(`plan`, `Planned`, `resolver`, `resolver_in`, `parse_port_spec`,
`listening_procs`, `port`, `port_remote`), `src/commands/tunnel.rs`
(`Header`, `read_header`, `resolve`, `tunnel`, `Request`, `Via`,
`remote_opener`), `src/devsbd/bridge.rs` (`attach`, `Transport`),
`src/serve/forwards.rs` (`Registry`, `Handle`, `Root`, `Owner`,
`Configured`, `AddRequest`, `Event`, `restore`, `candidates`, `via`),
`src/serve/forward_store.rs` (`path`, `load`, `update`, `Entry`),
`src/serve/api.rs` (`Forwarding`, `forwards_*`), `src/state.rs`
(`Instance.forwarded_ports`, `State.global_forwarded_ports`),
`src/config.rs` (`ForwardPort`). Wire shapes:
[api.md](../../reference/api.md#forwards).

## The engine: `Forward`

`Forward::start` binds a host listener on the caller's thread, then runs
everything else on background threads: a supervisor keeping one
`devsbd bridge` (`exec -i … devsbd bridge`) alive, respawned on death, an
accept loop, and a relay per connection. Each accepted connection becomes
a host-initiated `Connect` stream over the bridge's mux (flow-controlled,
half-close aware; frames: [protocol.md](../helper/protocol.md)); the helper inside dials the
target. Where the bridge comes from is the forward's `ForwardSpec.open`, an
`Opener` the supervisor calls on every (re)spawn: it returns the spawned
`Bridge` plus an `Opened` (route label, dial host and port, and the
`ProcessLookup` saying where the listening process is found), and the
engine waits for the handshake itself. `local_opener` resolves the route
and `exec`s a bridge into its container (`bridge::spawn_for`); since it
re-resolves each time, a container restart or a `service rebuild` heals
without restarting the forward. An opener gets the forward's `stop` flag:
one that blocks must bail once it's set, so dropping a `Forward` stays
prompt.
Nothing goes to stderr: state is `ForwardState` (`Connecting`, `Active`,
`Error`) plus notes. Unix-only, like the mux.

Every forward owns its own bridge, separate from the daemon's per-instance
bridge ([host-poll.md](host-poll.md#bridges)): a forwarded instance has two
bridge `exec`s, more with more forwards.

## Routes

`port::plan` (pure) turns `(instance?, service?)` plus the running
instances into ordered `Planned` candidates (the port only enters the
`Route`); `port::resolver` reloads config and state on every
call and tries them in order:

- **Instance**: tunnel into the instance's container, dial
  `127.0.0.1:<port>`. The instance must be running.
- **Service via an instance**: tunnel into a running instance that
  references the service, dial `<service>:<port>` over the shared network.
  Isolated service: the named instance (required). Global service: the
  named instance if it references it, else the first running referencing
  instance of the config root.
- **Injection fallback**, always last for a service: install the helper in
  the service container and dial `127.0.0.1:<port>` there (needs `/bin/sh`,
  root `exec`, writable `/run`).

The engine shows which process listens (`node (pid 412)`, `format_procs`
in `src/devsbd.rs`), per the opener's `ProcessLookup`. When the port lives
in the dialed container (instance and injection routes: `Helper`) it asks
the forward's own bridge: `Bridge::listening`,
a `PortQuery` the helper answers from `/proc`
([protocol.md](../helper/protocol.md#port-queries)), waited at most
`LOOKUP_TIMEOUT` (2 s). A via-instance route (the instance's helper can't
see the service container: `Probe`) and a helper without `PORT_INFO` fall
back to `ForwardSpec.probe`, which is `listening_procs`: a quiet `lsof -F`
`exec` in the route's `process_container`. Any failure shows nothing; a helper
that doesn't answer in time is not retried through `lsof`
(`helper_answer`). Probed on the supervisor thread at most every
`PROBE_REFRESH` (10 s) while the forward is `Active`.

## Remote bridges

A forward into a machine's instance (machines.md) is always asked for from
this side (`devsandbox port`, `forwards.add` with `machine`) and owned
here: a machine's `forwardPorts` are never mirrored locally, since a port
exposed there doesn't mean it's wanted here. Each bridge is one `TUNNEL`
stream on the machine's link ([fleet.md](fleet.md#tunnel)), whatever kind
of machine, so it goes through the machine's trust table there; a dead
stream is just a dead bridge: the supervisor opens a new one after its
retry gap, like any other, until the forward stops.
`commands::tunnel::remote_opener(machine, via)` asks `via()` on every
spawn (so a machine removed from the registry is the bridge's error) how
to reach the machine: `Via::Fleet {socket, request}`. It connects to the
link's `fleet-<name>.tunnel.sock` in the socket dir
(`endpoint::fleet_tunnel_socket`; absent: "its fleet link isn't up
(…)"), writes the `tunnel::Request` line (`{root, instance?, service?,
port}`), then reads the header within `HEADER_WAIT` (10 s), `stop`
polled (on stop or timeout the stream is shut down).

The far side resolves everything on its own host (`tunnel::resolve`: the
name with `resolve_instance_noninteractive`, then `port::resolver` there:
its config, state, runtime, helper install), over what its trust admits,
writes one header line, then runs `bridge::bridge_command` (`<runtime>
exec -i -u root <container> devsbd bridge`, the argv every local bridge
spawns) with the stream as its stdio. The header (`Header::line`) is
`devsandbox-tunnel 1 <json>`, the json either `{label, host, port, hash,
helper}` (`helper`: the port lives in the dialed container) or `{error}`
(the resolve failed).

This side reads the header (`read_header`) on a thread, one byte at a time
so nothing of the bridge's stream past the newline is taken, skipping
lines before the magic, bounded at 64 KiB. Then `bridge::attach` runs the
usual handshake and mux on the rest of the stream, labelled
`<machine>/<label>` (the route label shown too), taking the stream itself
as its transport (`bridge::Transport::Stream`, its stderr empty): killing
the bridge shuts the stream down, and the stream ending (the link or the
machine gone) ends the mux, so the bridge is done and the supervisor opens
another. `attach` also gets the machine, so a far helper older than this
devsandbox's protocol reads "update devsandbox on <machine> (and restart
the instance there)" (`mismatch_message`) rather than the local "restart
the instance". The lookup is `ProcessLookup::Helper` when the header says
`helper` (the far helper answers `Bridge::listening` over the same
stream), else `None`; a remote forward's `ForwardSpec.probe` finds
nothing. Errors: the header's `error`, else `<machine>: <why>` (`the
tunnel ended before its header` when the link refused the stream).

`Via::Ssh(argv)` (`open_remote`: an ssh running the hidden `devsandbox -C
<root> tunnel …` on the machine, `ssh::tunnel_argv`, its stderr kept in an
`ssh::Tail` for `no_header`'s errors) is the older form; no forward builds
it.

## Host ports

`HostPort::Fixed(p)` binds `p` or fails. `HostPort::Prefer(p)` tries `p`
upward over `PREFER_SPAN` (100) ports, skipping only `AddrInUse`, then an
OS-assigned port. Spec `<port>` is `Prefer(port)`, `<host>:<port>` is
`Fixed(host)` (`parse_port_spec`). Bind address defaults to `127.0.0.1`.

## Daemon forwards: `Registry`

One `serve-forwards` thread owns every daemon forward, so they outlive any
dashboard. It blocks on a command channel with a `POLL` (500 ms) timeout:
`forwards.list|add|rm` requests (each with a reply channel, waited up to
`REPLY_TIMEOUT`, 10 s), the host poll's running containers, and the
periodic status poll. All config/state work and binding happens on this
thread, never a connection's. Forwards are grouped per canonical config
root (`Root`), forwards into machines kept apart (`Remote`, by id: no local
config behind them); ids are daemon-wide. `forwards.list` shows the remote
ones only with `machines`. Changes go out as `forwards.changed`,
status lines as `forwards.status` and to `serve.log`. Every live forward is
an idle holder ([role.md](role.md#holders-and-idle-exit)). Dropping the
registry closes every listener and kills every bridge.

### Ad-hoc forwards and `forwards.toml`

`forwards.add` (the dashboard's `p` / `:port`) starts one and appends it to
`<data-dir>/devsandbox/forwards.toml` with the host port actually bound;
`forwards.rm` removes it. A daemon exiting leaves the file alone. At
start the registry restores each entry first thing: on its saved port
(`Fixed`, retried for `RESTORE_PORT_WAIT`, 1 s, while `AddrInUse`), else
`Prefer` from it, saving the new port. Entries whose instance is gone from
`state.toml` are dropped; a stopped instance's forward comes back and heals
when it runs. Writes are read-modify-writes under an exclusive lock on
`forwards.lock`, temp file + rename (`serve::locked_file::update`), since
two daemons overlap during a handoff.

`forwards.add` with `machine` (`Daemon::add_remote`) checks the machine is
in `machines.toml` (the path `Registry::spawn` gets), then binds a
`Forward` whose opener is `tunnel::remote_opener`; its `via` closure
re-reads `machines.toml` on every spawn (so a
removed machine is the bridge's error) and reaches it through its
link's tunnel socket in the socket dir `Registry::spawn` gets
(`forwards::via`). No probe fallback. Its entry carries
`machine`, `dir` the root there as sent (never canonicalized here) and
`instance` the name there. `restorable` keeps such entries without the
local state check; restore drops the ones of machines no longer registered
(logged) and starts the rest on their saved ports like local ones. Each
sync (host poll) first runs `drop_unregistered`: after `devsandbox machine
rm`, that machine's forwards stop and leave the store (logged; the rows'
change goes out as `forwards.changed`). `serve.log` names them
`box/web:3000 on 127.0.0.1:3000` (`describe`).

### Configured forwards (`forwardPorts`)

A sandbox's `forwardPorts` entries (`[host:][service:]port`) are owned by
the registry. Each host-poll pass hands it this host's running containers;
for every config root recorded in `state.toml` it loads config and state,
starts missing forwards and stops those whose owner stopped or whose entry
is gone. `Owner`: an instance entry or an isolated service's belongs to
the instance; a global service's belongs to the config root (one forward,
routed with no instance named). `forwards.rm` on one suppresses it until
its owner stops and runs again; a failed start is reported once.

Host port: always `127.0.0.1`. `candidates`: the saved port, then the
entry's host port (else the target port) upward over `PREFER_SPAN`,
skipping ports saved for any other forward so a stopped instance keeps its
own; each tried `Fixed`. The chosen port is saved in `state.toml`
(`Instance.forwarded_ports`, kept across `rebuild`, gone with `rm`; or
`State.global_forwarded_ports[project]`), keyed by the entry's canonical
form; each sync prunes ports of entries no longer declared. `forwardPorts`
is excluded from `config_hash`: editing it is never drift.

## Foreground `devsandbox port`

`devsandbox port [<instance>] [--service <svc>] [--address <ip>]
<[host:]port>...` runs forwards in its own process, without the daemon:
it waits up to 20 s in total for every forward's first outcome, exits
non-zero on any error or timeout, else prints the mappings and blocks until Ctrl-C. Its forwards
are invisible to the daemon, the Ports tab and API clients.

The instance may be a machine's (`box/api`, or a bare name that resolves
there), as may a global service (`--machine box --service redis`):
`machines::cli::route` hands it to `port::port_remote`, the same flow
(`forward_until_interrupted`, shared with `port::port`) with
[remote bridges](#remote-bridges) through the machine's link tunnel
socket in this user's socket dir, and no probe fallback. Only the
bind is local; the name resolves on the machine.

Limitations: [status.md](../../status.md#daemon)
