//! `devsandbox serve`: the per-user host daemon (docs/serve.md,
//! docs/inbox-redesign.md "Host daemon"). Unix-only for now; `main.rs` gates
//! the whole module.
//!
//! The socket and its permissions, the start lock, lazy start from clients,
//! version handoff and the idle exit (step 4); the bridges, with the outbox
//! drain, popups and control ops, and the startup autostart pass (step 5).
//! The API (step 6, docs/api.md): `api` methods over the same wire,
//! `subscribe` notifications.
//!
//! - `api`: the method table and handlers (thin calls into `inbox::ops`,
//!   `snapshot`), testable without a socket
//! - `endpoint`: the `local_endpoint` abstraction (paths, bind/connect/accept)
//! - `proto`: the JSON-lines wire bits (`hello`, ids, errors)
//! - `idle`: the pure idle countdown over a holder snapshot
//! - `daemon`: the accept loop, lock, handoff and idle exit
//! - `host`: the live host side: the bridge poll and the autostart pass
//! - `forwards`: the port forwards (ad-hoc and configured), on their own thread
//! - `client`: connect-or-lazy-start for commands
//! - `relay`: `devsandbox api --stdio`, stdin/stdout ↔ the socket (step 9)
//! - `install`: `serve install|uninstall`, the systemd user unit / LaunchAgent
//!   (step 10)

pub mod api;
pub mod client;
pub mod daemon;
pub mod endpoint;
pub mod forwards;
mod host;
pub mod idle;
pub mod install;
pub mod proto;
pub mod relay;
#[cfg(test)]
pub(crate) mod dts;
