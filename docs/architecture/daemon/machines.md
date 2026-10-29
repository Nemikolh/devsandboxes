# Machines: remote hosts over ssh

A machine is a remote host running devsandbox, registered with `devsandbox
machine add`. The local daemon links to each machine's daemon over
`ssh <target> devsandbox api --stdio`, merges its rows into watched
snapshots, and forwards requests for its instances. Read this before
touching anything that takes a `machine` param or a `m/name`.

Code: `src/serve/machines/mod.rs` (`Entry`, `Kind`, `MachineKind`,
`RawEntry`, `offered_refusal`, `path`, `parse`, `update`,
`check_name`, `Where`, `parse_name`, `default_name`), `ssh.rs`
(`api_argv`, `tty_argv`, `tty_argv_with`, `cli_argv`, `agent_argv`,
`script_argv`, `parse_target`, `Dial`, `Duplex`, `ssh_dial`, `ssh_agent_dial`), `link.rs`
(`LinkError`, `client_name`), `service.rs` (`Machines`, `Handle`, `Want`,
`Timing`, `Hooks`, `Hub`), `saved.rs` (`remotes.json`), `agent.rs`
(`serve`, `End`, `SPEC`), `api.rs`
(`machines.*`, `Ctx`), `forward.rs` (`QUICK`, `FANOUT_WAIT`, `verb`, the
federated joins), `cli.rs` (`plan`, `Plan`, `machine_use`, `route`,
`localize`, `Reconnect`, `Dials`), `cli/offered.rs` (`use_of`, `Use`,
`Refused`, `refusal`, `named`, `per_root`, `config_roots`), `install.rs` (`CHECK`, `INSTALL`, `check`, `install`,
`parse_check`, `decide`, `platform_matches`), `src/serve/snapshots.rs` (`Remotes`, `LinkState`, `merge`),
`src/serve/relay.rs` (`--session-env`), `src/commands/machine.rs`,
`src/commands/agent_relay.rs` (`run`, `Socket`),
`src/serve/endpoint.rs` (`agent_socket`, `agent_sockets`),
`src/tui/remote.rs`. Params: [api.md](../../reference/api.md#machines).

## Registry: `machines.toml`

`<data-dir>/devsandbox/machines.toml`, `[machine.<name>]` with its kind
(`Entry::kind`, read and written through `RawEntry`): `target` (`me@host`,
an ssh-config alias, or `ssh://[user@]host[:port]`) for a machine this host
sshes to (`Kind::Ssh`), or `offered = true` and no `target` for a fleet
worker that dialed this host (`Kind::Offered`, written by `fleet.offer`:
[fleet](fleet.md#hub-side)); exactly one of the two. Then `roots`
(remote config roots, as the remote canonicalized them; an offered
machine's one root is the fleet root its offer names), an optional
`command` (remote devsandbox as shell text, default `devsandbox`; no
`machine add` flag: `machine add` sets it to `~/.local/bin/devsandbox`
when it installs there off the login `PATH` (see
[`machine add`](#devsandbox-machine-addlssetrmtrustroots)) through `machines.add`'s
`command` param, any other location is set by hand; `machines.probe`
dials with the same param) and `agent` (whether this host's
ssh-agent is meant for the machine, see [Agent](#agent); off by default,
written only when on). Only the daemon writes it
(`machines.add`/`set`/`rm`, `fleet.offer`): locked read-modify-write on `machines.lock`, temp
file + rename (`serve::locked_file::update`), since two daemons overlap during a handoff. A file that
doesn't parse is logged and changes nothing. The CLI reads it only to know
whether any machine exists, so with none every command resolves exactly as
without machines and never starts a daemon it didn't need.

Names: `[A-Za-z0-9._-]+`, not starting with `.` or `-` (`check_name`).
Default: the target's host (`default_name`). `parse_name` splits a CLI
`<name>`: `m/x` → `Where::Machine`, `@/x` → `Where::Local`, bare →
`Where::Any`. Container names can't hold `/` or `@`, so this never
collides with an instance name.

## Inbound neighbors

A machine whose link to this host was dialed from its end (`machine add
<this host>` run there, a [peer link](fleet.md#peer-links)) is an inbound
neighbor here while that link is up: never in `machines.toml`, memory
only (`machines::inbound::Neighbors`, on `machines::api::Ctx`). Its
`fleet-link --peer <name>` announces it (`links.inbound`) on a connection
it holds; the neighbor lasts while that connection does (`Guard`, the
newest announce of a name winning, so an older connection's close never
removes a newer one's). `machines.list` lists the neighbors after the
registry's entries, as rows of `kind` `inbound` (`MachineKind::Inbound`):
no `target`, `command` or `capacity`, `agent` off, `state` always
`connected`, `version` the link `Hello` it said (build, platform,
`hub|member`), `roots` its own config roots and this host's fleet root
there, as its daemon answered `roots.list` over the link once announced
(`daemon::inbound_roots`; empty until then). `devsandbox machine ls`
shows `inbound` as the TARGET. A name is a registry entry's or a
neighbor's, never both: `links.inbound` refuses a name `machines.toml`
has (any kind), `machines.add` and `fleet.offer` a live neighbor's
(`inbound::taken`). `Kind::from_wire` refuses `inbound`: a neighbor
isn't something to dial.

## ssh

One argv builder for the daemon, the CLI and the dashboard; every argv
goes through `argv_with`, which takes `Entry::ssh_target`, so none is
built for an offered machine (`offered_refusal`, a backstop: each path to
one goes over its fleet link or refuses first in its own words). Plain ssh, no
ControlMaster of its own: `~/.ssh/config` decides. The remote command runs
through the login shell (`exec "$SHELL" -lc '…'`, each arg quoted for it),
so the remote `PATH` is the user's. No argv passes `-A`: this host's
ssh-agent reaches a machine only over its [agent relay](#agent), only with
the machine's `agent` on.

- `api_argv`: `ssh -T -o BatchMode=yes [-p N] -- <host> … api --stdio
  --client <c> --session-env` (the daemon has no TTY: ssh must fail, not
  prompt).
- `tty_argv` / `tty_argv_with`: `-t`, no `BatchMode` (terminal tabs,
  interactive re-runs; `tty_argv_with` runs `<runtime> exec -it` for
  service tabs).
- `cli_argv`: `-t` on a terminal, else `-T` (piped bytes stay clean).
- `tunnel_argv`: `ssh -T [-o BatchMode=yes] [-p N] -- <host> … -C <root>
  tunnel [--instance=<name>] [--service=<svc>] --port <n>`, a remote
  forward's bridge ([forwards.md](forwards.md#remote-bridges));
  `BatchMode` when `batch` (the daemon). The `=` forms keep a name
  starting with `-` a value.
- `agent_argv`: `ssh -T -o BatchMode=yes [-p N] -- <host> … agent-relay
  --client <c>`, a machine's agent relay ([Agent](#agent)).
- `script_argv`: `ssh -T [-p N] -- <host> sh -c <script> sh <args…>`, a
  POSIX script run for a person at the CLI ([Install](#install)); no
  `BatchMode`. A script is one line: the login shell may be csh, which
  refuses a newline inside quotes.

`Tail` (a bounded ring of a stream's last bytes, `Tail::collect`) keeps
ssh's stderr for `spawn`'s `Peer` and the remote forward opener alike.

`Dial` (`Fn(&Entry) -> Duplex`) is injectable: `ssh_dial` spawns
`api_argv`, `ssh_agent_dial` `agent_argv`, both with `link::client_name`
as the client. The daemon wraps its API dial in `fleet::offer::dial`,
which dials an offered machine at its fleet link's socket instead. A `Duplex` is always a unix socket pumped to the ssh
child's pipes, so `client::Conn` works on it unchanged and tests dial a
second test daemon's socket (or run the agent relay's far end
in-process). `--session-env` makes the remote relay replace each verb
request's `params.env` with its own login-shell env, `SSH_AUTH_SOCK`
pointed at this host's agent relay socket there while it's up
([Agent](#agent)).

`--session-env` also makes the remote relay attach its connection before
it writes the sentinel or relays a byte (`relay::attach`): one
`link.attach {peer}` request of its own, `peer` being `--client` without
its `machine:` prefix (this host's name, `link::client_name`), its answer
read byte by byte up to its newline and not relayed (no `channel`: the
relay's form; `link.attach {peer, channel}` is a fleet member's stream,
[fleet](fleet.md#the-trust-table)). From then on the
remote daemon serves that connection with the machine's trust table
([fleet](fleet.md#the-trust-table)): under its `trust = "worker"` (the
default) this host reaches the machine's own roots only (those its
instances name, plus its `member.toml` `roots`, which `machine add`
records there), never `machines.*`, `member.*`, `shutdown` or an edit of
those configs; under `full`, everything but `member.*`, `link.*` and `links.*`. The
held link and every forwarded verb's connection dial `api_argv`, so both
go through it. A refused attach (another user's socket, a daemon without
`link.attach`) ends the relay with exit 1 and the reason on stderr,
which the link reports as ssh's stderr tail (`LinkError::Closed`); the
daemon hanging up first is exit 75. The other argvs (`tty_argv`,
`cli_argv`, `tunnel_argv`, `agent_argv`) are ssh shells on the machine:
the trust table isn't in their path.

## Install

`src/serve/machines/install.rs` checks which devsandbox a machine runs and
copies this binary there, for `machine add`
([`devsandbox machine add|ls|set|rm|trust|roots`](#devsandbox-machine-addlssetrmtrustroots)). The
CLI runs it, not the daemon: the daemon's ssh
is `BatchMode` with nobody to ask, and the bytes are the CLI's own binary.
Both scripts go over `script_argv` and print their report after a marker
line, past whatever the login shell printed first.

- `CHECK` (`check`): `$1` is the program (empty: `devsandbox`), resolved
  with `command -v`; empty and not on the login `PATH`, it falls back to
  `$HOME/.local/bin/devsandbox` (`Remote::off_path`), since a login shell
  often misses that dir (added in `.bashrc`). It reports `uname -s`,
  `uname -m`, the path and the output of its `--version`. `parse_check` makes it a `Remote`, whose
  `version` is `None` when `--version` fails (a build from before it
  existed).
- `platform_matches`: `uname` against this binary's
  `std::env::consts::{OS, ARCH}` (`Linux`↔`linux`, `Darwin`↔`macos`;
  `x86_64`|`amd64`↔`x86_64`, `aarch64`|`arm64`↔`aarch64`); copying
  itself can't work anywhere else.
- `decide` (pure): the same semver is `Plan::Nothing`; a devsandbox found
  with another or no version is updated in place (`Dest::Found`); a
  missing one goes to `$HOME/.local/bin/devsandbox` (`Dest::Default`).
- `INSTALL` (`install`): `$1` is the destination (empty: the default).
  ssh's stdin, the binary, goes into a `mktemp` file next to it,
  `chmod 755`, then its `--version` must run (a binary linked against a
  libc the machine lacks fails here), then `mv -f` over the destination:
  atomic, no "text file busy" over a running binary. Any failure removes
  the temp file and leaves the destination as it was. It prints the
  installed path, expanded.

A failed ssh or script is an error carrying the tail of its stderr.

## Link handshake

`link.rs`: the hello says version `0.0.0`, build 0, so a link never hands
off the remote daemon (only that host's own clients upgrade it). The
`protocol` must match (`LinkError`); differing semvers within one protocol
are a warning at add and shown in `machine ls`.

Before the hello, `handshake` reads up to the relay's sentinel: the relay
(`--session-env`) writes `\n` + `devsandbox-api 1\n` once it reaches the
remote daemon (`relay::write_sentinel`), and the link skips every line
before it (`ssh::skip_lines`, the same byte-at-a-time skipper as a remote
forward's tunnel header, so nothing past the sentinel is consumed),
bounded at 64 KiB, with the hello's read timeout. The leading newline ends
a motd printed without a final one, so the sentinel always starts a line
and is matched as a whole line (a trailing `\r` tolerated); another version
is an error naming the older side. The first skipped line (cut at 200
chars) is kept on `Link::noise`; a held link logs it to `serve.log` once
per connect (`machine <name>: login shell printed: …`). EOF before the
sentinel is `LinkError::Closed` with ssh's stderr tail; the bound or a
timeout is `LinkError::Other` with that first line.

## Held links: `Machines`

The daemon runs a `Machines` service for its whole life: one
`serve-machine-<name>` thread per registered machine, plus a
`serve-machine-<name>-agent` one for its [agent relay](#agent), started,
stopped or restarted when `add`/`set`/`rm` change the registry
(`Handle::reload`: a change to a machine's entry restarts its link,
except a change of `agent` alone, which flips the live link's flag).
A machine is wanted while a local connection watches with `machines:
true`, a forwarded call is queued, or a `Want` holds it, and for
`Timing::linger` (60 s) after (both threads share the last want, a
`Want`'s drop counting as one). `Handle::want` returns a `Want`: a guard
per holder (a forwarded verb for its run, a connection's
`machines.hold` for the connection's life), counted per machine name so a
hold outlives a restarted link; `Want::agent_ready` waits, bounded, for
the agent relay's handshake (`false` at once with `agent` off). The link
connects only while the machine is wanted; on a fleet hub
(`service::Hub`) an offered machine is always wanted
([fleet](fleet.md#hub-mode)). Connected, it
subscribes to `instances`, watches each of the machine's roots while a
local watcher wants machines (not for a link lingering on calls), and
stores pushed snapshots in `Remotes` (kept while the link is down; in
memory only, but on a hub, which also saves them to `remotes.json`).
`LinkState`: `idle`, `connecting`, `connected`, `unreachable` (with the
reason); retries back off from 1 s doubling to 30 s while wanted.
Handshake timeout 20 s. A link isn't an idle holder of the local daemon;
the clients that want it are. On the machine it's a client like any other.

## Forwarded requests

A request carrying `machine` (not registered: `not-found`) has it stripped
and goes to that machine, but for the ones served here:

- **Quick calls** (`forward::QUICK`: `instances.list|cancel|procs|signal|
  rename|done|buildLog|resolve`, `containers.logs|inspect`,
  `sandboxes.list|get`, `services.get`) go over the held link, one in
  flight per machine, `Timing::call` (15 s) each. A timeout drops the link
  (its late answer would be read as the next call's); a call arriving
  during the backoff retries the connect at once, and calls queued when a
  connect fails get `failed` (`<machine>: unreachable: …`). The remote's answer, error code and data
  included, is the answer.
- **Verbs** get a fresh dial per request, so a build never holds up the
  link. The requester's `SSH_AUTH_SOCK` (an absolute path in `env`) is
  reported to this daemon (`bridge::report_agent`), so the agent relay
  serves the live agent of whoever asked even when the daemon started
  without one. The verb holds its machine wanted (a `Want`) for its run;
  with `agent` on it waits up to 5 s (`forward::AGENT_WAIT`) for the
  relay's handshake before dialing, so a `run` that clones over ssh there
  doesn't race it, and goes anyway (logged) if it isn't up.
  `output`/`progress` are relayed with `request` rewritten to the local
  id. While its ssh runs it's a `verbs` holder here. A hang-up, or a
  local drain, closes the ssh, which the remote sees as a hang-up:
  cancelled there unless `detach`.
- **Federated** `instances.resolve` / `sandboxes.list` with `machines:
  true` ask this host and every machine in parallel, each machine given
  `FANOUT_WAIT` (5 s). Remote matches come back qualified (`box/web-2`)
  with an additive `found: [{name, machine, root}]`; a machine that doesn't
  answer adds nothing, and an empty result names the unreachable ones.
- **Served here** (`forward::HERE`: `forwards.add`): `machine` names where
  the forward reaches, not where it runs. The daemon keeps the param and
  answers itself, binding the forward on this host with one ssh per bridge,
  or on an offered fleet worker one `TUNNEL` stream per bridge over its
  fleet link ([forwards.md](forwards.md#ad-hoc-forwards-and-forwardstoml)).

- **`machines.hold {name, agent?}`** (`machines::api::hold`, answered in
  `daemon.rs` `hold_machine`) is per connection: the `Want` is kept in the
  connection's `ConnState` (one per machine; holding again replaces it)
  and dropped when the connection's thread ends, so a holder that exits
  or dies releases the machine (then the linger). `agent`, the caller's
  absolute `SSH_AUTH_SOCK`, is reported like a verb's; with the machine's
  `agent` on the answer waits up to `forward::AGENT_WAIT` for the relay.
  It answers `{ok, agent}`, `agent` whether the relay is up. The CLI's
  re-runs use it ([The CLI](#the-cli-clirs)).

## Federated snapshots

`instances.watch {dir, machines: true}` (the dashboard always sends it)
gets every push merged with each machine's rows (`snapshots::merge`): rows
gain `machine` and keep their remote `root`, and a top-level `machines`
array carries each machine's `name`, `kind` (`ssh` | `offered`), `target`
(`null` for an offered one), `state`, `error`, `version`, `command`,
`runtime`, `agent`, `capacity` (its host's
[capacity report](fleet.md#capacity), from its last push). A machine's push or state change re-publishes those
watchers' roots from the last local params. Details:
[host-poll.md](host-poll.md#snapshots).

## Agent

A machine's agent relay carries this host's ssh-agent to the machine over
its own ssh (`ssh::agent_argv`, no `-A`), on the helper's frame protocol
([protocol.md](../helper/protocol.md)) with the roles of a container's
relay ([ssh-agent.md](../helper/ssh-agent.md)) swapped onto the two hosts.
It is a [fleet link](fleet.md#link) of `agent::SPEC`: `fleet::link::opener` and
`fleet::link::answerer` hold the mechanics below.

- **The machine's end**, the hidden `devsandbox agent-relay --client <c>`
  (`commands::agent_relay`), plays the in-container daemon. It binds
  `endpoint::agent_socket`: `<socket dir>/agent-<client>.sock`, 0600 in
  the 0700 socket dir, the client cut to `[A-Za-z0-9._-]` and 40 bytes (a
  longer one ends in a hash of the whole name) so the path stays under
  macOS's 104 bytes. It binds a temp name and renames it over that path,
  so the newest relay for a client wins. Then it writes the sentinel line
  `\n` + `devsandbox-agent 1\n` (`fleet::link::write_sentinel`), exchanges
  `Hello`s on its stdin/stdout and requires the peer to advertise
  `caps::SSH_AGENT`. Each connection to its socket becomes one
  `Open { channel: SSH_AGENT }` stream on the mux over stdout (ids in the
  daemon's space, below 2^31); `Open`s from the peer are refused. It
  exits when stdin ends or stdout fails (`Ok`) or when the keepalive
  (`mux::KEEPALIVE_INTERVAL`/`KEEPALIVE_TIMEOUT`) hears nothing, so a
  partitioned link can't leave a socket that accepts and hangs. Exiting
  unlinks the socket only while it is still its own (same device and
  inode), never a newer relay's.
- **This end**, `agent::serve` over any byte stream, plays the host
  bridge: it skips the login shell's output up to the sentinel
  (`ssh::skip_lines`, as the [link](#link-handshake) does), sends its
  `Hello` advertising `caps::SSH_AGENT`, and answers each
  `Open { SSH_AGENT }` with a connection to its `agent()` (production:
  `bridge::host_agent`) when `allowed()` says yes at that moment, else a
  `Close`, which the machine's end turns into a closed client socket. It
  returns an `End`: the handshake failed, the keepalive ran `on_dead`
  (which must end the stream, e.g. kill the ssh), or the stream closed.

On the machine nothing registers the socket: `bridge::host_agent` tries
the live `agent-*.sock` files in the socket dir (`endpoint::agent_sockets`,
newest mtime first) after the agents clients reported and before the
process's own `$SSH_AUTH_SOCK`, so the daemon's bridges and any command
run there find it, and an exited relay drops out by the liveness check.

Each machine's `agent` flag (`Entry::agent`, off by default) is set by
`machine add --agent`, `machine set <name> --agent on|off` or `a` on the
dashboard's Machines tab (`machines.add` / `machines.set`), listed by
`machines.list` / `get` and the federated snapshot's `machines`, shown
in the Machines tab's AGENT column, and marked `(agent)` in the
dashboard's header note.

The local daemon runs the relay: the machine's agent thread
(`service.rs` `agent_loop`) spawns `ssh::agent_argv` through the agent
`Dial` while the flag is on and the machine is wanted
([Held links](#held-links-machines), linger included). What keeps it
wanted: a dashboard (it always watches with `machines: true`, which
covers its terminal tabs), a forwarded verb for its run, and a CLI
command re-run there over ssh, through its `machines.hold`. It serves it
with `agent::serve`, its `allowed()` reading the live flag. A failed
relay (ssh, handshake, keepalive) is logged (`machine <name>: agent
relay: <why>`, with ssh's stderr tail) and retried with the link's
backoff. The relay's socket and ssh are kept where the service reaches
them (`Ctl::kill_relay`): turning `agent` off (`machine set <name>
--agent off`, or `a` on the dashboard's Machines tab) kills it at
once (in-flight agent streams cut, later `Open`s refused) without
restarting the link; the want ending kills it within a tick; `rm` and
daemon exit kill it and join the thread.

An offered fleet worker has no relay of its own (`spawn_link` starts no
agent thread for it; `Want::agent_ready` is `false` at once): its
`agent` flag gates the `AGENT` streams of its fleet link instead
([fleet.md](fleet.md#agent)). The hub's `fleet-link` serves those
streams from the agent this daemon answers `fleet.agent` with (its own
`bridge::host_agent` while the flag is on, asked every 2 s), and the
worker binds `agent-fleet-<hub>.sock` in its socket dir, found by
`bridge::host_agent` like a relay's socket, only while its probes say
the hub serves.

The machine's API relay (`--session-env`, `relay::SessionEnv`) sets each
verb's `SSH_AUTH_SOCK` to `endpoint::agent_socket` for its own
`--client` (the same `link::client_name` both ssh's pass) when that
socket accepts a connection at the time of the request line, so host git
there (worktree clones) signs with this host's agent; otherwise the login
env's own `SSH_AUTH_SOCK` (the machine's agent, if any) stays.

## The CLI: `cli.rs`

`route` (I/O) runs `plan` (pure) in `main` before a verb goes to
`verbs_client` or any command runs locally:

- Under `--in-process`: nothing fans out; `--machine` is an error.
- **Verbs** (`run`, `rebuild`, `start`, `stop`, `rm`) keep their name;
  `verbs_client` resolves it across machines (`instances.resolve` /
  `sandboxes.list` with `machines: true`), picking on a TTY when ambiguous,
  and sends the request with the match's `machine` and root.
- **Other named commands** (`exec`, `logs`, `inspect`, `done`, `undone`,
  `rename`, …) are resolved through the daemon; one on a machine is re-run
  as `ssh [-t] <target> devsandbox <args>` (`exec` of ssh; with `agent`
  on, see below; `exec --service <name>` re-runs with `--service=<name>`,
  once). `logs` /
  `inspect` with a bare `devsandbox-…` container name stay local. `port`
  on a machine's instance runs here: `route` returns `Routed::Port` (the
  machine's `Entry`, the match's root or else the machine's first, the
  instance as the machine names it) and `commands::port::port_remote`
  forwards over one ssh per port (`ssh::tunnel_argv` without `BatchMode`,
  `tunnel::remote_opener`; [forwards](forwards.md)), or on an offered
  fleet worker one `TUNNEL` stream per bridge over its fleet link
  ([fleet](fleet.md#tunnel)). `vscode` on a
  machine's instance is refused.
- **`--machine m`** is for nameless commands (`machine_use`; an offered
  worker's: below): `ps`, `stats` run once over ssh; root-bound ones (`ls`, `status`, `gc`,
  `service ls`) run once per registered root with `-C <root>` and a
  `==> m:<root>` line (`--json` with several roots is an error); `stop
  --all`, `rebuild --all`, `start --all`, `service rebuild` go as verbs;
  `port --service <svc>` naming no instance (`MachineUse::Port`) forwards
  the machine's global service from its first registered root, as above.
  On a named command it's an error (qualify the name instead); refused for
  `inbox`, `vscode`, `serve`, `api`, `machine`, the dashboard.
- Without a local daemon, a qualified remote name and `--machine` fail
  ("needs devsandbox serve"); a bare name resolves locally.
- **`agent` on** (`hold_params` decides from the `machines.get` entry):
  the command keeps its daemon connection and sends `machines.hold {name,
  agent}` (`agent`: this process's `SSH_AUTH_SOCK`, if absolute) first,
  so the relay is up for ssh. `--machine`'s per-root runs, spawned
  already, keep that connection across all of them. A single re-run's ssh
  is spawned and waited for instead of `exec`ed (an
  `exec` would drop the connection, and OpenSSH closes inherited fds), and
  the command exits with ssh's status (`Routed::Exit`). While it runs this
  process ignores SIGINT and SIGQUIT (`spawn_ssh_owning_signals`): with
  `-t` ssh owns the terminal, and with `-T` a Ctrl-C reaches the whole
  process group; ssh gets the dispositions back as they were before
  (a `pre_exec`, since an ignored signal survives `exec`). A hold that
  fails (a daemon without the method, any error) prints a `warning:` and
  the command runs without the agent. With `agent` off ssh is `exec`ed,
  except for a tty `exec` (below).
- **A tty `exec` reconnects** (`reconnect_session`: no `--attach` /
  `--sessions` / `--service`, and a tty by the remote `exec`'s rule, `shell_flags`, with
  this process on a terminal standing for the remote stdin). The id is this
  process's (`--session` as given, else `exec::mint_session_id`), and the
  ssh is always spawned (`reconnect_loop`, through
  `spawn_ssh_owning_signals`, a hold kept across every dial): the first
  dial runs `exec --reserve-255 --session=<id> …` (create-or-attach),
  each re-dial `exec --reserve-255 <name> --attach=<id>` (attach only: a
  reconnect never starts a second program; `session_dials`). `Reconnect`
  (pure) decides after each dial: 255 is ssh's own failure, since
  `--reserve-255` (hidden) makes the remote `exec` report a program's 255
  as 254 (`exec::reported_code`); after a dial that ran
  `CONNECTED_AFTER` (5 s) it is an outage: `connection lost,
  reconnecting… (Ctrl-C to stop)` on stderr, then `reconnecting… (attempt
  N)` per further try, re-dialed after `backoff` (1, 2, 4, 8, then 10 s,
  no limit on tries). A 255 before any dial got that far is ssh failing to
  connect: the loop ends with it. Any other code ends the loop with it;
  `NO_SESSION_EXIT` (250) also prints that the session is gone (`gone_note`);
  the helper's 251 (attached elsewhere) and 252 (`VERSION_EXIT`: started
  by another devsbd, still running) end it too, the helper's own note on
  stderr.
  Without the helper on the remote instance `--session` is a plain exec,
  so a re-dial's `--attach` fails there (exit 1, its message on stderr)
  and ends the loop. SIGINT is only ignored while ssh runs, so Ctrl-C
  during a wait ends the process.
- **On an offered fleet worker** (no ssh to it, [fleet](fleet.md#pty))
  the dials are `PTY` streams over its fleet link from this process
  (`fleet::pty_client::dial`), in the same loop: `dials` picks `Dials::Pty`
  for an offered entry (`Dials::Ssh`, the argvs above, otherwise) and
  `run_dials` hands `reconnect_loop` the dial closure. The first dial
  creates the session (or attaches) running the command (none: the login
  shell), each re-dial attaches only, at the instance's root there (the
  match's, else the machine's one root); a dial's end becomes the exit
  code an ssh dial would give (`pty_client::exit_of`: 255 only for a lost
  link), so `Reconnect` is unchanged. `--sessions` is a `sessions.list`
  quick call with `machine` (the lines printed, or `no sessions in
  \`m/x\`` on stderr); a tty `--attach [ID]` is one dial attaching only
  (no id: `exec::choose_detached` over that list), no loop, as over ssh
  (`offered_session_exec`). An `exec` without a tty is one piped `PTY`
  dial ([fleet](fleet.md#pty), *Pipe mode*) and never re-dialed:
  `cli::pipe_mode` (pure) gives its argv and `-i` as a local `exec`
  decides them (the command as typed with `-i` as given; none, the login
  shell with `exec::shell_flags`' `-i`), and `resolve_and_route` dials it
  with the machine held for the agent. A session `exec` without a tty
  (`--attach` off a terminal, `--session` without `-t`) is refused
  (`offered::Refused::SessionNoTty`). `exec --service <name>` is one
  service `PTY` dial (`cli::service_mode`: the command as typed, none the
  worker's login shell; [fleet](fleet.md#pty), *Service terminals*), never
  re-dialed (no session), refused without a terminal. A fallback terminal
  ([fleet](fleet.md#pty), *Tty fallback*) is dialed as a session, so a
  lost link re-dials its attach, which the worker answers with
  `no-session`: the loop ends with 250 and the worker's
  not-resumable message.
- **Other re-runs to an offered fleet worker** are API calls through this
  host's daemon with `machine` set (fleet plan decision 15: within the
  worker's trust, never a `devsandbox …` run there), printed by the local
  command's own render fns (`commands::logs::render_logs`,
  `inspect::render`, `ls::table` / `ls::render_json`, `status::render`,
  `done::status_line`, `rename::renamed_line`, `ps::render`,
  `stats::render`, `services::ls_render`), so the output reads the same; `cli::offered::use_of`
  (pure) picks the call, `offered::named` / `offered::per_root` /
  `offered::listing` make them over an injected call. `logs [-n N]` is
  `containers.logs` of the instance's container (its row's, from an
  `instances.list` at `depth: disk`: a rename keeps the container; a name
  that is no instance is taken as a container name); `logs --build` is
  `instances.buildLog` (up to `api::MAX_LINES` lines, a note on stderr
  when cut; `ansi` when stdout is a terminal); `logs --build --follow` is
  `instances.buildLog` pages with `offset` (`offered::follow_build_log`,
  by the row's `instance_id`, so a rename doesn't lose it): from the
  log's start, the next page at once while pages bring something, after
  `logs::FOLLOW_POLL` when one doesn't (`offered::follow_next`), until
  one is `complete`; printed as the local follow prints (as they are on a
  terminal, a pty log through `build_log::Strip` elsewhere), and the
  record gone meanwhile ends it, as locally; `inspect [--json]` is
  `containers.inspect` (without the local one's browser sandbox line:
  that comes from this host's record); `done` / `undone` are
  `instances.done`, worded from the row's `done` before; `rename` is
  `instances.rename`. `--machine m ls [--json]` and `status --json` list
  each config root its listing of the machine's one root (its fleet root)
  names: the roots its instance rows name (`offered::config_roots`: the
  placed configs', and under `worker` trust the worker's own roots'
  instances too; none: the fleet root itself), each an `instances.list` after its `==> m:<root>` line,
  `per_root`'s rules (`--json` with several roots is an error); `ls`
  renders the snapshot's sandbox rows (a config that doesn't load is the
  error, as locally). `--machine m service ls [--json]` takes the same
  roots and rules, each an `instances.list` at `depth: listing` (container
  statuses), and renders its service rows (`used_by` cut to the lent
  instances); the snapshot's `error` (the runtime's, resolve failures)
  goes to stderr, a config that doesn't load is the error. The empty
  line names `<root>/devsandboxes.toml`. `--machine m ps [--all] [--json]` is one
  `containers.list`, `stats [--json]` one `containers.stats`
  (`offered::listing`): the worker answers its whole runtime's rows cut to
  what its trust reaches (those instances' containers and their config
  roots' services, [fleet](fleet.md#the-trust-table)). `--machine m gc
  [--force]` (`offered::Use::Gc`) is handed on to `verbs_client`: the `gc`
  verb per config root (`config_roots`, as `ls`), each after its banner;
  the worker runs it under `full` trust only, its `worker` refusal is the
  error. Refused, naming the command (`offered::refusal`): `vscode` (not
  over a fleet link yet), and a session `exec` without a tty (a session
  over the link needs a terminal).

## `devsandbox machine add|ls|set|rm|trust|roots`

A client of `machines.*` (and, for `trust` and `roots`, `member.*`) only,
never in-process (`src/commands/machine.rs`).

`add` first checks the machine's devsandbox (`ensure_devsandbox`, over
[Install](#install)): `install::check` through the login `PATH`, then
`install::decide` against this binary's version. The same version goes on
to the probe. Otherwise, a machine whose `uname` fails
`install::platform_matches` gets a warning and goes on to the probe when
it has no devsandbox, and is left as it is when it has another version
(the daemon's version warning still applies). A devsandbox found only
through the `~/.local/bin` fallback (`off_path`) makes the machine's
`command` `LOCAL_COMMAND`, installed over or not. Then `consent`: `--auto-install` installs, a TTY asks `[y/N]` on
stderr, no TTY is an error pointing at `--auto-install` (`refusal`).
Declined, a missing devsandbox is an error and nothing is added; another
version carries on as it is. `install::install` copies `exe::devsandbox()`
there and a second `install::check` decides (`rechecked`): `devsandbox` on
the login `PATH` is now this version, done; installed to
`~/.local/bin` but the login `PATH` doesn't reach it first (macOS by
default, another devsandbox earlier on it), the installed path is checked
directly and the machine's `command` becomes `LOCAL_COMMAND`
(`~/.local/bin/devsandbox`); updated in place but still not what the
login `PATH` resolves, an error naming what it resolves to. That
`command` goes to both `machines.probe` and `machines.add`.

`add` without `--config` calls `machines.probe` (the remote's
`roots.list`: every `config_dir` in its `state.toml` plus
`~/.devsandboxes`) and lets the user multi-select on a TTY; off a TTY it
takes the one usable candidate, else errors listing them. `machines.add`
then checks every given root on the machine (`roots.list {paths}`: `~`
expanded with the remote home, canonicalized, config loaded). Between
the two, `add` records every chosen root, probed or typed, as the
machine's own (`record_roots`): `devsandbox machine roots add -- <roots…>`
run there over a one-off ssh with the machine's `command`
(`record_roots_argv`: `ssh::cli_argv` with `-T`, no `BatchMode`, each
root quoted so its `~` reaches the remote command unexpanded), stdout
dropped, stderr passed through. A probed root is recorded too: it may be
an own root only through an instance, and stops being one when that
instance goes. A failure (ssh, a non-zero exit, a devsandbox there without
the command) is a `warning:` line (`roots_warning`) naming the target and
the `machine roots add` to run there, since under the machine's `worker`
trust the hub may be refused those roots
([fleet.md](fleet.md#the-trust-table)); the add goes on. `add
--agent` saves the machine with `agent` on; adding a name that exists is
refused. `set <name> --agent on|off` (`machines.set`; `--agent` is
required) changes it on a registered machine and prints `<name>: agent
on|off`; on is refused for an offered machine. `ls` shows it in an `AGENT`
column, and `offered` as an offered machine's `TARGET`; `LEND` and
`CAPACITY` are the machine's last [capacity report](fleet.md#capacity)
(`machines.list`'s `capacity`, `-` before one came), the over-cap note in
parentheses. `rm` forgets the machine.

`trust [worker|full]` is about this host, not a registered machine: a
client of `member.*` (this host's `member.toml`;
[fleet.md](fleet.md#trust)). So is `roots [add|rm] [<path>…]`
(`commands::machine::roots`), this host's picked roots (`member.toml`'s
`roots`): without a subcommand `member.get`, printing them one per line
(or a note when there are none); `add` resolves each path here
(`resolve_root`: a leading `~` / `~/` is `$HOME`, a relative path is
against the current directory), canonicalizes it and requires a directory
(`existing_dir`), then `member.set {roots_add}`; `rm` resolves the same
way without requiring it to exist and sends both the canonical form (when
it exists) and the literal one as `member.set {roots_rm}`, so a removed
directory can be dropped. Both print the roots after the change.
`--machine` and qualified names refuse `machine` as a whole
(`serve::machines::cli::machine_use`).

## Dashboard

`src/tui/remote.rs`: rows are keyed `machine/name` (bare for local),
routed back to a request's `machine` + remote `root`; terminal tabs use
`tty_argv` (no hold of their own: the dashboard's `machines: true` watch
keeps the machine, and its agent relay, up); on an offered fleet worker an
instance tab instead runs this host's `devsandbox exec … -- <m>/<name>`
(`on_machine`, the executable `ExecCli::exe`), whose tty `exec` dials
the worker's fleet link ([The CLI](#the-cli-clirs)), and a service tab
runs `devsandbox exec --service=<name> -- <m>/<instance>` through an
instance using it (`service_argv`, `service_instance`); a remote instance tab runs
its shell in a helper session it names (`session_exec_argv`) and, when its
dial (ssh, or the local CLI) exits 255, re-dials an attach to it into the same tab (`attach_argv`,
`tui::app::terminal::Redial`, the CLI's `Reconnect` rule;
[terminal.md](../dashboard/terminal.md) *Remote tabs reconnect*); a machine without rows shows `box: connecting` or `box:
unreachable: <why>` in the header (`pending_note`), the name as `box
(agent)` when its `agent` is on. `p` / `:port` on a remote row sends
`forwards.add` with `machine` and the row's root (the forward binds here,
listed with the rest on the Ports tab: `forwards.list {machines: true}`);
VS Code on a remote row answers "isn't supported yet". The Machines tab,
listed while a machine is registered, shows every registered machine from
the snapshot's `machines` with its target (dimmed `offered` for an offered
fleet worker),
link state (and error), `agent`, and its LEND and CAPACITY
([capacity](fleet.md#capacity));
`a` flips the selected one's `agent` with `machines.set` (no prompt; a
down machine's too, it's a registry write), and the row follows with the
next push, which the reload triggers
([tabs-and-actions.md](../dashboard/tabs-and-actions.md) *Tabs*).

Limitations: [status.md](../../status.md#daemon)
