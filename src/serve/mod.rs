//! `devsandbox serve`: the per-user host daemon (docs/serve.md,
//! docs/inbox-redesign.md "Host daemon"). Unix-only for now; `main.rs` gates
//! the whole module.
//!
//! The socket and its permissions, the start lock, lazy start from clients,
//! version handoff and the idle exit (step 4); the bridges, with the outbox
//! drain, popups and control ops, and the startup autostart pass (step 5).
//! The API methods come at step 6.
//!
//! - `endpoint`: the `local_endpoint` abstraction (paths, bind/connect/accept)
//! - `proto`: the JSON-lines wire bits (`hello`, ids, errors)
//! - `idle`: the pure idle countdown over a holder snapshot
//! - `daemon`: the accept loop, lock, handoff and idle exit
//! - `host`: the live host side: the bridge poll and the autostart pass
//! - `client`: connect-or-lazy-start for commands

pub mod client;
pub mod daemon;
pub mod endpoint;
mod host;
pub mod idle;
pub mod proto;
