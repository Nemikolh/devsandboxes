# Fleet

Machines as one pool of capacity (the plan:
[fleet](../../plans/active/fleet.md)). What exists so far is the link
primitive the fleet's links are built on (the machines'
[agent relay](machines.md#agent) runs on it too) and the hub's side of a
worker's link: the offer, the offered registry entry and its dial.

Code: `src/serve/fleet/mod.rs` (`LINK`), `src/serve/fleet/link.rs`
(`Spec`, `opener`, `answerer`, `End`, `write_sentinel`,
`match_sentinel`), `src/serve/fleet/offer.rs` (`call`, `Ctx`, `accept`,
`dial`), `src/commands/fleet_link.rs` (`fleet_link`, `run`),
`src/serve/endpoint.rs` (`fleet_socket`), `src/daemon_config.rs`
(`ServeConfig::hub`), `src/devsbd/proto.rs` (`channel::API`, `caps::API`,
`channel::flows`).

## Link

A link is one byte pipe between two hosts (an ssh's stdin/stdout in
production) carrying the helper's [frame protocol](../helper/protocol.md),
with the roles of a container's link moved onto the two hosts. A connection
to a socket on one host becomes a stream on the mux, answered on the other
host by a connection made there. A `Spec` says what a link carries: its
sentinel line (`\n<sentinel> <version>\n`), its one channel, the cap that
says the far end serves it, and the `Hello` hash (informational).

- **The opener** (`link::opener`, the in-container daemon's role) gets a
  reader, a writer and a socket path. It binds a temp name next to the path
  (0600) and renames it over the path, so the newest opener for a path
  wins; then it writes the sentinel, exchanges `Hello`s (advertising
  nothing) and requires the peer's `Hello` caps to include the spec's
  cap. Its `on_ready` callback runs next, with the peer's `Hello` (its
  `hash` is the far end's spec `hello`, a semver); an error there ends
  the opener with it. Each accepted connection becomes one `Open { channel }` stream
  (`Mux::attach`, ids below 2^31); on a flowing channel
  (`channel::flows`) the stream is flow-controlled. `Open`s from the peer
  are refused. It returns `Ok` when the reader ends or the writer fails,
  an error when the keepalive (`mux::KEEPALIVE_INTERVAL` /
  `KEEPALIVE_TIMEOUT`) hears nothing or the listener fails. Exiting
  unlinks the socket only while it is still its own (same device and
  inode), never a newer opener's.
- **The answerer** (`link::answerer`, the host bridge's role) gets a
  reader, a writer and an `open` callback. It skips what the login shell
  printed up to the sentinel (`ssh::skip_lines`, at most `SENTINEL_LIMIT`
  bytes; another sentinel version is a handshake failure saying which side
  to update), exchanges `Hello`s advertising the cap, and answers each
  `Open` on the spec's channel with `open()`'s connection (`None` refuses
  it with `Close`, as is every other channel). It returns an `End`: the
  handshake failed (with the first line of noise), the keepalive ran
  `on_dead` (which must end the reader, e.g. kill the ssh), or the stream
  closed.

Two specs exist:

| spec | sentinel | channel | cap | opener / answerer |
|---|---|---|---|---|
| `machines::agent::SPEC` | `devsandbox-agent 1` | `SSH_AGENT` (legacy) | `caps::SSH_AGENT` | `commands::agent_relay::run` / `machines::agent::serve` |
| `fleet::LINK` | `devsandbox-fleet 1` | `API` (flowing) | `caps::API` | `commands::fleet_link::run` (on the hub) / the worker's lending thread (plan step 1c) |

On `fleet::LINK` each connection to the opener's socket is one connection
to whatever daemon socket the answerer's `open` connects, so a
`client::Conn` on the opener's host says its hello to, and calls, a daemon
on the answerer's host.

**The sentinel rule.** An `API` stream starts with the API relay's own
sentinel line, `\n` + `devsandbox-api 1\n` (`relay::write_sentinel`): the
answerer's `open` hands back a daemon connection that writes that line
before the daemon's bytes. So a stream reads exactly like an ssh machine's
relay, and `machines::link::handshake` (which skips to that line, then
says its hello) works on it unchanged.

## Hub side

A worker dials its hub: its daemon holds `ssh -T -o BatchMode=yes <hub>
devsandbox fleet-link --worker <name>` (plan step 1c). The hub never dials
the worker; everything it does there goes over that link's `API` streams.

- **`devsandbox fleet-link --worker <name>`** (hidden,
  `commands::fleet_link`) is the opener of `fleet::LINK` on the hub. The
  name must pass `machines::check_name`. It binds
  `endpoint::fleet_socket`: `<socket dir>/fleet-<name>.sock` (0600 in the
  0700 socket dir, the name cut as an agent relay socket's client is), so
  the newest link for a worker wins. Its `on_ready` connects to this host's
  daemon (`client::connect`, which starts one like any client does when
  none runs) and sends `fleet.offer {worker, version}`, `version` the
  worker's `Hello`. A refused offer ends the link with `offer refused:
  <why>` (on its stderr, which the worker's ssh keeps). It holds that
  daemon connection for the link's life, so a linked worker is a `clients`
  holder of the hub's daemon. `serve::machines::cli::machine_use` refuses
  `--machine` for it.
- **`fleet.offer`** (`fleet::offer::call`, internal: not in the npm
  typings) is accepted only by a hub: `[serve] hub = true` in
  `daemon.config.toml` (`ServeConfig::hub`, read once at start into
  `daemon::Options::hub`); anything else is `denied`. `accept` (pure)
  adds an offered entry for a new name, keeps an existing offered one, and
  refuses (`denied`) a name an ssh machine has. A new entry reloads the
  machine links (`service::Handle::reload`), which start a link thread for
  it; the offer is logged with the worker's version.
- **An offered entry** is `[machine.<name>]` with `offered = true` and no
  `target` in `machines.toml` (`machines::Kind::Offered`,
  [Registry](machines.md#registry-machinestoml)). Its roots are empty.
- **The dial** (`fleet::offer::dial`, wrapped around whatever reaches ssh
  machines when `daemon::run` builds its `Dial`): for an offered entry, a
  connection to its `fleet-<name>.sock` in the daemon's socket dir as a
  plain `ssh::Duplex::socket`; down, `its fleet link isn't up (…)`. Each
  connection is one `API` stream to the worker's daemon, so the held link
  (`service::Machines`), the quick calls and `forward::verb` (one stream
  per verb, a hang-up the stream's close) run on it as on an ssh machine.
- **What needs ssh** to the machine is refused for an offered one: every
  `machines::ssh` argv goes through `argv_with`, which takes
  `Entry::ssh_target`, an error for an offered entry
  (`machines::offered_refusal`: exec, terminals, forwards and CLI re-runs
  there need fleet step 4). So the CLI's re-runs (`exec`, `logs`,
  `inspect`, `done`, …, `--machine` per-root commands), the dashboard's
  terminal tabs, remote forwards and the agent relay all fail with that
  message; `machines.set` refuses `agent` on for one (`invalid`). The
  dashboard's logs and inspect views go through the `containers.*` quick
  calls and work.
