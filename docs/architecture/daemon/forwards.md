# Port forwarding

Host TCP ports tunnelled into an instance (or one of its services) through
the in-container helper, without publishing container ports. Two owners
share one engine: the daemon (Ports-tab ad-hoc forwards and configured
`forwardPorts`) and the foreground `devsandbox port` command. Read this
before touching forwards, their ports or their persistence.

Code: `src/devsbd/forward.rs` (`Forward`, `ForwardSpec`, `HostPort`,
`PREFER_SPAN`, `ForwardState`, `bind_listener`), `src/commands/port.rs`
(`plan`, `Planned`, `resolver`, `parse_port_spec`, `listening_procs`,
`port`), `src/serve/forwards.rs` (`Registry`, `Handle`, `Root`, `Owner`,
`Configured`, `AddRequest`, `Event`, `restore`, `candidates`),
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
target. The route is re-resolved on every bridge (re)spawn, so a container
restart or a `service rebuild` heals without restarting the forward.
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

`listening_procs` shows which process listens (`node (pid 412)`) via a
quiet `lsof -F` `exec`; any failure shows nothing. Probed at most every
`PROBE_REFRESH` (10 s) while the forward is `Active`.

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
root (`Root`); ids are daemon-wide. Changes go out as `forwards.changed`,
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
`forwards.lock`, temp file + rename, since two daemons overlap during a
handoff.

### Configured forwards (`forwardPorts`)

A sandbox's `forwardPorts` entries (`[host:][service:]port`) are owned by
the registry. Each host-poll pass hands it the running containers; for
every config root recorded in `state.toml` it loads config and state,
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

Limitations: [status.md](../../status.md#daemon)
