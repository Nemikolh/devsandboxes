//! `devsandbox serve`: the per-user host daemon (docs/serve.md,
//! docs/inbox-redesign.md "Host daemon"). Unix-only for now; `main.rs` gates
//! the whole module.
//!
//! This is the skeleton (step 4): the socket and its permissions, the start
//! lock, lazy start from clients, version handoff and the idle exit. Bridges,
//! the outbox drain and popups move in at step 5, the API methods at step 6.
//!
//! - `endpoint`: the `local_endpoint` abstraction (paths, bind/connect/accept)
//! - `proto`: the JSON-lines wire bits (`hello`, ids, errors)
//! - `idle`: the pure idle countdown over a holder snapshot
//! - `daemon`: the accept loop, lock, handoff and idle exit
//! - `client`: connect-or-lazy-start for commands

pub mod client;
pub mod daemon;
pub mod endpoint;
pub mod idle;
pub mod proto;
