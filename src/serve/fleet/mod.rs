//! Fleet: machines as one pool of capacity (docs/architecture/daemon/fleet.md,
//! plan docs/plans/active/fleet.md): the peer link between two machines
//! and what runs over it.
//!
//! - `link`: the two ends of a host-to-host frame-mux link, generic over
//!   its sentinel line and channel ([`link::Spec`]), each end serving its
//!   own table of channels ([`link::Serves`]) and opening the peer's;
//!   [`LINK`] is the fleet link's spec (the `API` channel).
//! - `transport`: how a link is dialed (ssh).
//! - `dialer`: the dialer's half of a peer link: how the daemon holds each
//!   ssh machine's link (`fleet-link --peer` there), and the
//!   [`dialer::dial`] that reaches every machine through its
//!   `fleet-<name>.sock`.
//! - `member`: `member.toml` (this host's name, trust, roots, `[allow]`,
//!   `[caps]`, `dev-builds`, `[[agent]]` grants) and `member.get` /
//!   `member.set`; `grants`: the agent grants (whom this host's agent
//!   reaches, by member key) and their writes.
//! - `identity`: this member's key (`member.key`, behind a `KeyStore`),
//!   public keys' text form and fingerprint, domain-separated signatures,
//!   signed statements and the rollback check.
//! - `membership`: the membership log (`membership.json`), a hash chain
//!   the authority signs, its heads compared, and the introductions held
//!   beside it; `membership_api`: the local-only `membership.get` /
//!   `adopt` / `sign` over it, and the joins `machine add` checks with it.
//! - `proposals`: membership changes proposed to the authority
//!   (`fleet.propose`), queued in `proposals.json` until the user approves
//!   or denies them there.
//! - `proof`: a peer link's proof of both members' keys after its
//!   `Hello`s, with the membership heads compared and synced.
//! - `graph`: the members' signed link reports and the paths over them
//!   (pure); `reports`: this host's own report built from its links and
//!   sent to the hub (`fleet.report`), the reports exchanged and pushed
//!   (`fleet.graph`), the graph the daemon holds (`graph.get`).
//! - `route`: a routed stream (`ROUTE`): its header, the origin's and the
//!   destination's proof and their session's keys, how a link end relays
//!   or answers one, and
//!   [`route::connect`], how a local client reaches a member;
//!   `routes`: the daemon's `route-<member>.sock` per member it has no
//!   link up to, each connection opened over the graph's paths in order.
//! - `session`: a routed stream's end-to-end session: the ephemeral key
//!   agreement, per-direction keys, sealed records and the pump between
//!   a plaintext stream and a sealed one.
//! - `lent`: the trust table, what a peer may do over a link (the
//!   daemon's `Peer` connection table, by `member.toml`'s `trust`).
//! - `pty`: the serving end of a `PTY` stream: a session attach in an
//!   instance here, through its helper's `session.attach`.
//! - `pty_client`: the asking end of one: a tty `exec` on a machine, on
//!   this process's terminal.
//! - `tunnel`: the serving end of a `TUNNEL` stream: one port forward's
//!   bridge into an instance here (the asking end is
//!   `commands::tunnel::remote_opener`).
//! - `agent`: `AGENT`: a relaying end's agent socket, bound while the
//!   granting member serves, each connection one stream to its agent;
//!   `agent_routes`: the daemon's `agent-route-<member>.sock` per member
//!   whose report grants this host agent and that doesn't dial it.
//! - `transfer`: one bulk transfer on the link's `TRANSFER` streams (either
//!   way) and each end's receivers by kind.
//! - `update`: the build each end says in its `Hello`, and how a member
//!   follows its hub's (download, install, forced handoff); `follow`: the
//!   wait for a hub's pushed dev build, and [`follow::Follow`].
//! - `code`: a config root and its repos moved onto a machine (the root as
//!   a tar, each folder's branch as a git bundle from this host's repo
//!   cache), and the receivers for them.
//! - `place`: the pure placement of new work over candidates filled from
//!   the daemon's snapshots (soft caps, locality, pins).
//! - `placed`: the daemon's `run` verb placed: candidates, the decision,
//!   the request forwarded to the machine.

pub mod agent;
pub mod agent_routes;
pub mod code;
pub mod dialer;
pub mod follow;
pub mod grants;
pub mod graph;
pub mod identity;
pub mod lent;
pub mod link;
pub mod member;
pub mod membership;
pub mod membership_api;
pub mod place;
pub mod placed;
pub mod placements;
pub mod proof;
pub mod proposals;
pub mod reports;
pub mod route;
pub mod routes;
pub mod session;
pub mod pty;
pub mod pty_client;
pub mod transfer;
pub mod transport;
pub mod tunnel;
pub mod update;

use crate::devsbd::proto::channel;

/// The fleet link: the other end's daemon API, one flowing `API` stream per
/// connection to this end's socket. Its sentinel is `devsandbox-fleet 2`
/// (2: a peer link proves both keys after the `Hello`s, `proof`).
/// An `API` stream carries no sentinel of its own: its first byte is the
/// daemon's (decision 27 of the fleet plan). Both ends
/// also serve `TRANSFER` streams ([`transfer`], `link::Serves::transfers`).
/// Its `Hello` hash is this build
/// ([`update::HELLO`]); a peer link's says its end's role and hub too
/// (`update::hello_of`).
pub const LINK: link::Spec = link::Spec {
    name: "fleet link",
    serves: "API",
    sentinel: "devsandbox-fleet",
    sentinel_version: 2,
    channel: channel::API,
    hello: update::HELLO,
};
