//! Fleet: machines as one pool of capacity (docs/architecture/daemon/fleet.md,
//! plan docs/plans/active/fleet.md). So far only its link primitive:
//!
//! - `link`: the two ends of a host-to-host frame-mux link carrying one
//!   channel, generic over its sentinel line and channel ([`link::Spec`]);
//!   the machines' agent relay runs on it, and [`LINK`] is the fleet
//!   link's spec (the `API` channel).

pub mod link;

use crate::devsbd::proto::{caps, channel};

/// The fleet link: a worker's daemon's API, one flowing `API` stream per
/// connection to the opener's socket. Its sentinel is `devsandbox-fleet 1`.
#[allow(dead_code)] // `fleet-link` and the worker's lending thread (fleet plan 1b/1c)
pub const LINK: link::Spec = link::Spec {
    name: "fleet link",
    serves: "API",
    sentinel: "devsandbox-fleet",
    sentinel_version: 1,
    channel: channel::API,
    cap: caps::API,
    hello: env!("CARGO_PKG_VERSION"),
};
