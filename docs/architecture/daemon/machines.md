# Machines: remote hosts over ssh

A machine is a remote host running devsandbox, registered with `devsandbox
machine add`. The local daemon links to each machine's daemon over
`ssh <target> devsandbox api --stdio`, merges its rows into watched
snapshots, and forwards requests for its instances. Read this before
touching anything that takes a `machine` param or a `m/name`.

Code: `src/serve/machines/mod.rs` (`Entry`, `path`, `parse`, `update`,
`check_name`, `Where`, `parse_name`, `default_name`), `ssh.rs`
(`api_argv`, `tty_argv`, `tty_argv_with`, `cli_argv`, `agent_argv`,
`parse_target`, `Dial`, `Duplex`, `ssh_dial`, `ssh_agent_dial`), `link.rs`
(`LinkError`, `client_name`), `service.rs` (`Machines`, `Handle`, `Want`,
`Timing`, `Hooks`), `agent.rs`
(`serve`, `End`, `SENTINEL`), `api.rs`
(`machines.*`, `Ctx`), `forward.rs` (`QUICK`, `FANOUT_WAIT`, `verb`, the
federated joins), `cli.rs` (`plan`, `Plan`, `machine_use`, `route`,
`localize`), `src/serve/snapshots.rs` (`Remotes`, `LinkState`, `merge`),
`src/serve/relay.rs` (`--session-env`), `src/commands/machine.rs`,
`src/commands/agent_relay.rs` (`run`, `Socket`),
`src/serve/endpoint.rs` (`agent_socket`, `agent_sockets`),
`src/tui/remote.rs`. Params: [api.md](../../reference/api.md#machines).

## Registry: `machines.toml`

`<data-dir>/devsandbox/machines.toml`, `[machine.<name>]` with `target`
(`me@host`, an ssh-config alias, or `ssh://[user@]host[:port]`), `roots`
(remote config roots, as the remote canonicalized them), an optional
`command` (remote devsandbox as shell text, default `devsandbox`; set by
hand, `machine add` has no flag for it) and `agent` (whether this host's
ssh-agent is meant for the machine, see [Agent](#agent); off by default,
written only when on). Only the daemon writes it
(`machines.add`/`set`/`rm`): locked read-modify-write on `machines.lock`, temp
file + rename, since two daemons overlap during a handoff. A file that
doesn't parse is logged and changes nothing. The CLI reads it only to know
whether any machine exists, so with none every command resolves exactly as
without machines and never starts a daemon it didn't need.

Names: `[A-Za-z0-9._-]+`, not starting with `.` or `-` (`check_name`).
Default: the target's host (`default_name`). `parse_name` splits a CLI
`<name>`: `m/x` → `Where::Machine`, `@/x` → `Where::Local`, bare →
`Where::Any`. Container names can't hold `/` or `@`, so this never
collides with an instance name.

## ssh

One argv builder for the daemon, the CLI and the dashboard. Plain ssh, no
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

`Tail` (a bounded ring of a stream's last bytes, `Tail::collect`) keeps
ssh's stderr for `spawn`'s `Peer` and the remote forward opener alike.

`Dial` (`Fn(&Entry) -> Duplex`) is injectable: `ssh_dial` spawns
`api_argv`, `ssh_agent_dial` `agent_argv`, both with `link::client_name`
as the client. A `Duplex` is always a unix socket pumped to the ssh
child's pipes, so `client::Conn` works on it unchanged and tests dial a
second test daemon's socket (or run the agent relay's far end
in-process). `--session-env` makes the remote relay replace each verb
request's `params.env` with its own login-shell env, `SSH_AUTH_SOCK`
pointed at this host's agent relay socket there while it's up
([Agent](#agent)).

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
connects only while the machine is wanted. Connected, it
subscribes to `instances`, watches each of the machine's roots while a
local watcher wants machines (not for a link lingering on calls), and
stores pushed snapshots in `Remotes` (memory only, kept while the link is
down).
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
  answers itself, binding the forward on this host with one ssh per bridge
  ([forwards.md](forwards.md#ad-hoc-forwards-and-forwardstoml)).

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
array carries each machine's `name`, `target`, `state`, `error`, `version`,
`command`, `runtime`, `agent`. A machine's push or state change re-publishes those
watchers' roots from the last local params. Details:
[host-poll.md](host-poll.md#snapshots).

## Agent

A machine's agent relay carries this host's ssh-agent to the machine over
its own ssh (`ssh::agent_argv`, no `-A`), on the helper's frame protocol
([protocol.md](../helper/protocol.md)) with the roles of a container's
relay ([ssh-agent.md](../helper/ssh-agent.md)) swapped onto the two hosts:

- **The machine's end**, the hidden `devsandbox agent-relay --client <c>`
  (`commands::agent_relay`), plays the in-container daemon. It binds
  `endpoint::agent_socket`: `<socket dir>/agent-<client>.sock`, 0600 in
  the 0700 socket dir, the client cut to `[A-Za-z0-9._-]` and 40 bytes (a
  longer one ends in a hash of the whole name) so the path stays under
  macOS's 104 bytes. It binds a temp name and renames it over that path,
  so the newest relay for a client wins. Then it writes the sentinel line
  `\n` + `devsandbox-agent 1\n` (`agent::write_sentinel`), exchanges
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
them (`Ctl::kill_relay`): `machine set <name> --agent off` kills it at
once (in-flight agent streams cut, later `Open`s refused) without
restarting the link; the want ending kills it within a tick; `rm` and
daemon exit kill it and join the thread.

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
  on, see below). `logs` /
  `inspect` with a bare `devsandbox-…` container name stay local. `port`
  on a machine's instance runs here: `route` returns `Routed::Port` (the
  machine's `Entry`, the match's root or else the machine's first, the
  instance as the machine names it) and `commands::port::port_remote`
  forwards over one ssh per port (`ssh::tunnel_argv` without `BatchMode`,
  `tunnel::remote_opener`; [forwards](forwards.md)). `vscode` on a
  machine's instance is refused.
- **`--machine m`** is for nameless commands (`machine_use`): `ps`,
  `stats` run once over ssh; root-bound ones (`ls`, `status`, `gc`,
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
  the command runs without the agent. With `agent` off ssh is `exec`ed.

## `devsandbox machine add|ls|set|rm`

A client of `machines.*` only, never in-process (`src/commands/machine.rs`).
`add` without `--config` calls `machines.probe` (the remote's
`roots.list`: every `config_dir` in its `state.toml` plus
`~/.devsandboxes`) and lets the user multi-select on a TTY; off a TTY it
takes the one usable candidate, else errors listing them. `machines.add`
then checks every given root on the machine (`roots.list {paths}`: `~`
expanded with the remote home, canonicalized, config loaded). `add
--agent` saves the machine with `agent` on; adding a name that exists is
refused. `set <name> --agent on|off` (`machines.set`; `--agent` is
required) changes it on a registered machine and prints `<name>: agent
on|off`. `ls` shows it in an `AGENT` column. `rm` forgets the machine.

## Dashboard

`src/tui/remote.rs`: rows are keyed `machine/name` (bare for local),
routed back to a request's `machine` + remote `root`; terminal tabs use
`tty_argv` (no hold of their own: the dashboard's `machines: true` watch
keeps the machine, and its agent relay, up); a machine without rows shows `box: connecting` or `box:
unreachable: <why>` in the header (`pending_note`), the name as `box
(agent)` when its `agent` is on. `p` / `:port` on a remote row sends
`forwards.add` with `machine` and the row's root (the forward binds here,
listed with the rest on the Ports tab: `forwards.list {machines: true}`);
VS Code on a remote row answers "isn't supported yet". The Machines tab,
listed while a machine is registered, shows every registered machine from
the snapshot's `machines` with its link state (and error) and `agent`;
`a` flips the selected one's `agent` with `machines.set` (no prompt; a
down machine's too, it's a registry write), and the row follows with the
next push, which the reload triggers
([tabs-and-actions.md](../dashboard/tabs-and-actions.md) *Tabs*).

Limitations: [status.md](../../status.md#daemon)
