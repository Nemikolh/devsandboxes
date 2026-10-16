//! `devsandbox serve`: the per-user host daemon
//! (docs/architecture/daemon/role.md). Unix-only for now; `main.rs` gates the
//! whole module.
//!
//! The socket and its permissions, the start lock, lazy start from clients,
//! version handoff and the idle exit (step 4); the bridges, with the outbox
//! drain, popups and control ops, and the startup autostart pass (step 5).
//! The API (step 6, docs/reference/api.md): `api` methods over the same wire,
//! `subscribe` notifications.
//!
//! - `api`: the method table and handlers (thin calls into `inbox::ops`,
//!   `snapshot`), testable without a socket
//! - `endpoint`: the `local_endpoint` abstraction (paths, bind/connect/accept)
//! - `proto`: the JSON-lines wire bits (`hello`, ids, errors)
//! - `idle`: the pure idle countdown over a holder snapshot
//! - `daemon`: the accept loop, lock, handoff and idle exit
//! - `host`: the live host side: the one container poll (bridges, forwards,
//!   watched roots' snapshots) and the autostart pass
//! - `snapshots`: the watched roots' snapshot cache and the poll's cadence
//! - `forwards`: the port forwards (ad-hoc and configured), on their own thread
//! - `forward_store`: `forwards.toml`, the ad-hoc forwards a successor restores
//! - `client`: connect-or-lazy-start for commands
//! - `machines`: remote hosts over ssh (docs/architecture/daemon/machines.md):
//!   the registry (`machines.toml`), the ssh argv and dial, the link
//!   handshake, the `machines.*` methods
//! - `relay`: `devsandbox api --stdio`, stdin/stdout ↔ the socket (step 9)
//! - `install`: `serve install|uninstall`, the systemd user unit / LaunchAgent
//!   (step 10)
//! - `verbs`: `run`/`rebuild`/`start`/`stop`/`rm` as requests, each a
//!   `devsandbox --in-process` subprocess streamed back to its client
//! - `verbs_client`: the CLI's side of those five: prompts, streaming,
//!   Ctrl-C, exit codes, the in-process fallback

pub mod api;
pub mod client;
pub mod daemon;
pub mod endpoint;
pub mod forward_store;
pub mod forwards;
mod host;
pub mod idle;
pub mod install;
pub mod machines;
pub mod proto;
pub mod relay;
pub mod snapshots;
pub mod verbs;
pub mod verbs_client;
#[cfg(test)]
pub(crate) mod dts;
