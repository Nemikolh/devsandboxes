//! Fleet: machines as one pool of capacity (docs/architecture/daemon/fleet.md,
//! plan docs/plans/active/fleet.md). So far its link primitive and both
//! sides of a worker's link:
//!
//! - `link`: the two ends of a host-to-host frame-mux link, generic over
//!   its sentinel line and channel ([`link::Spec`]), each end serving its
//!   own table of channels ([`link::Serves`]) and opening the peer's; the
//!   machines' agent relay runs on it, and [`LINK`] is the fleet link's
//!   spec (the `API` channel).
//! - `offer`: `fleet.offer`, which registers an offered worker in
//!   `machines.toml`, and the [`offer::dial`] that reaches one through its
//!   `fleet-link` socket (`commands::fleet_link` is that opener).
//! - `lend`: `lend.toml` (whether and to which hub this host lends) and the
//!   fleet root a hub reaches.
//! - `lender`: `lend.get` / `lend.set`, the daemon's one writer of
//!   `lend.toml`, applied live to the lending thread.
//! - `worker`: the worker's lending thread, the answerer of [`LINK`] over
//!   its ssh to the hub.
//! - `lent`: what a hub may do over that link (the daemon's `Worker`
//!   connection table).
//! - `pty`: the worker's end of a `PTY` stream: a session attach in a
//!   lent instance, through its helper's `session.attach`.
//! - `pty_client`: the hub's end of one: a tty `exec` on an offered
//!   machine, on this process's terminal.
//! - `tunnel`: the worker's end of a `TUNNEL` stream: one port forward's
//!   bridge into a lent instance (the hub's end is
//!   `commands::tunnel::remote_opener`).
//! - `transfer`: one bulk transfer on the link's `TRANSFER` streams (either
//!   way) and each end's receivers by kind.
//! - `update`: the build each end says in its `Hello`, and how a worker
//!   follows its hub's (download, install, forced handoff).
//! - `code`: a config root and its repos moved onto a worker (the root as
//!   a tar, each folder's branch as a git bundle from the hub's repo
//!   cache), and the worker's receivers for them.
//! - `place`: the pure placement of new work over candidates filled from
//!   the daemon's snapshots (soft caps, locality, pins).
//! - `placed`: the daemon's `run` verb placed: candidates, the decision,
//!   the request forwarded to the worker.

pub mod code;
pub mod lend;
pub mod lender;
pub mod lent;
pub mod link;
pub mod offer;
pub mod place;
pub mod placed;
pub mod pty;
pub mod pty_client;
pub mod transfer;
pub mod tunnel;
pub mod update;
pub mod worker;

use crate::devsbd::proto::channel;

/// The fleet link: a worker's daemon's API, one flowing `API` stream per
/// connection to the opener's socket. Its sentinel is `devsandbox-fleet 1`.
/// Each `API` stream starts with the API relay's own sentinel line
/// (`relay::write_sentinel`), written by the worker's answer, so a stream
/// reads as an ssh relay does to `machines::link::handshake`. Both ends
/// also serve `TRANSFER` streams ([`transfer`], `link::Serves::transfers`).
/// Its `Hello` hash is this build
/// ([`update::HELLO`]), which a worker follows (`worker`).
pub const LINK: link::Spec = link::Spec {
    name: "fleet link",
    serves: "API",
    sentinel: "devsandbox-fleet",
    sentinel_version: 1,
    channel: channel::API,
    hello: update::HELLO,
};
