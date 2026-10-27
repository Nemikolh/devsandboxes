# Fleet

Machines as one pool of capacity (the plan:
[fleet](../../plans/active/fleet.md)). What exists so far is the link
primitive the fleet's links are built on; the machines'
[agent relay](machines.md#agent) already runs on it.

Code: `src/serve/fleet/mod.rs` (`LINK`), `src/serve/fleet/link.rs`
(`Spec`, `opener`, `answerer`, `End`, `write_sentinel`,
`match_sentinel`), `src/devsbd/proto.rs` (`channel::API`, `caps::API`,
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
  cap. Each accepted connection becomes one `Open { channel }` stream
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
| `fleet::LINK` | `devsandbox-fleet 1` | `API` (flowing) | `caps::API` | — |

On `fleet::LINK` each connection to the opener's socket is one connection
to whatever daemon socket the answerer's `open` connects, so a
`client::Conn` on the opener's host says its hello to, and calls, a daemon
on the answerer's host.
