//! This end of a machine's agent relay (docs/architecture/daemon/machines.md,
//! *Agent*): serves the ssh-agent streams the machine's
//! `devsandbox agent-relay` (`commands::agent_relay`) opens, from the local
//! agent.
//!
//! The relay is a `fleet::link` of [`SPEC`] (docs/architecture/daemon/fleet.md):
//! the far end, its opener, plays the in-container daemon (accepts agent
//! clients on its socket and sends one `Open { channel: SSH_AGENT }` each),
//! [`serve`], its answerer, the host bridge (`bridge.rs` `open_stream`): it
//! connects each `Open` to the agent, or refuses it with `Close`.
//! Transport-agnostic: any byte stream, an ssh `Duplex` in production.
//!
//! Before its `Hello` the far end writes the sentinel line
//! `devsandbox-agent 1`, so what the remote login shell printed first is
//! skipped, as on the API link.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::devsbd::proto::{caps, channel};
use crate::serve::fleet::link::{self, Spec};

pub use crate::serve::fleet::link::End;

/// The agent relay as a link: `SSH_AGENT` streams, sentinel
/// `devsandbox-agent 1`.
pub const SPEC: Spec = Spec {
    name: "agent relay",
    serves: "ssh-agent",
    sentinel: "devsandbox-agent",
    sentinel_version: 1,
    channel: channel::SSH_AGENT,
    cap: caps::SSH_AGENT,
    hello: env!("CARGO_PKG_VERSION"),
};

/// Serve one agent relay over `reader`/`writer` until it ends
/// (`link::answerer` of [`SPEC`]): each `Open { SSH_AGENT }` gets a
/// connection to `agent()` when `allowed()` says so at that moment, else
/// it's refused (as is every other channel). `on_ready` runs once the
/// `Hello`s are through (the far end's socket is bound by then); `on_dead`
/// once the keepalive hears nothing: it must end `reader` (kill the ssh) so
/// this returns.
pub fn serve(
    reader: impl Read,
    writer: impl Write + Send + 'static,
    allowed: impl Fn() -> bool,
    agent: impl Fn() -> Option<PathBuf>,
    on_ready: impl FnOnce(),
    on_dead: impl FnOnce() + Send + 'static,
) -> End {
    let open = || {
        if !allowed() {
            return None;
        }
        UnixStream::connect(agent()?).ok().map(Into::into)
    };
    link::answerer(&SPEC, reader, writer, open, on_ready, on_dead)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::machines::ssh;

    fn sentinel(line: &[u8]) -> Result<Option<()>, String> {
        link::match_sentinel(&SPEC, line)
    }

    #[test]
    fn the_sentinel_matcher_takes_only_our_line() {
        assert_eq!(sentinel(b"devsandbox-agent 1"), Ok(Some(())));
        assert_eq!(sentinel(b"devsandbox-agent 1\r"), Ok(Some(())));
        assert_eq!(sentinel(b"welcome to box"), Ok(None));
        assert_eq!(sentinel(b"devsandbox-api 1"), Ok(None));
        assert!(sentinel(b"devsandbox-agent 2").unwrap_err().contains("update devsandbox"));
    }

    #[test]
    fn noise_before_the_sentinel_is_skipped_and_none_is_a_handshake_failure() {
        let mut wire = b"motd line\nno newline".to_vec();
        link::write_sentinel(&SPEC, &mut wire).unwrap();
        let mut r = &wire[..];
        let mut noise = None;
        assert!(ssh::skip_lines(&mut r, link::SENTINEL_LIMIT, &mut noise, sentinel).is_ok());
        assert!(r.is_empty(), "nothing past the sentinel consumed");
        assert_eq!(noise.as_deref(), Some("motd line"));

        let end = serve(&b"just a motd\n"[..], Vec::new(), || true, || None, || {}, || {});
        let End::Handshake(why) = end else { panic!("{end:?}") };
        assert!(why.contains("closed before") && why.contains("just a motd"), "{why}");
    }
}
