//! This end of a machine's agent relay (docs/architecture/daemon/machines.md,
//! *Agent*): serves the ssh-agent streams the machine's
//! `devsandbox agent-relay` (`commands::agent_relay`) opens, from the local
//! agent.
//!
//! The relay's stdio carries the helper's frame protocol
//! (docs/architecture/helper/protocol.md). Roles as in a container: the far
//! end plays the in-container daemon (accepts agent clients on its socket and
//! sends one `Open { channel: SSH_AGENT }` each), [`serve`] the host bridge
//! (`bridge.rs` `open_stream`): it connects each `Open` to the agent, or
//! refuses it with `Close`. Transport-agnostic: any byte stream, an ssh
//! `Duplex` in production.
//!
//! Before its `Hello` the far end writes a [`SENTINEL`] line, so what the
//! remote login shell printed first is skipped, as on the API link.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::ssh::{self, SkipError};
use crate::devsbd::mux::{self, Mux};
use crate::devsbd::proto::{self, caps, channel};

/// The line `agent-relay` writes before its frames:
/// `devsandbox-agent <SENTINEL_VERSION>`.
pub const SENTINEL: &str = "devsandbox-agent";

/// The sentinel's format version; a reader refuses any other.
pub const SENTINEL_VERSION: u32 = 1;

/// How much [`serve`] reads looking for the sentinel: the login shell's
/// noise and the line itself together.
const SENTINEL_LIMIT: usize = 64 * 1024;

/// What both ends say in their `Hello` (informational: the frame protocol's
/// `VERSION` is what has to match).
pub const HELLO_HASH: &str = env!("CARGO_PKG_VERSION");

/// The sentinel line, flushed. Its leading newline ends a motd printed
/// without a final one, so the sentinel always starts a line.
pub fn write_sentinel(out: &mut impl Write) -> io::Result<()> {
    write!(out, "\n{SENTINEL} {SENTINEL_VERSION}\n")?;
    out.flush()
}

/// The sentinel matcher for `ssh::skip_lines`: exactly
/// `devsandbox-agent 1` (a trailing `\r` tolerated); another version is an
/// error, anything else noise.
fn sentinel(line: &[u8]) -> Result<Option<()>, String> {
    let line = String::from_utf8_lossy(line);
    let Some(version) = line.trim_end_matches('\r').strip_prefix(SENTINEL).and_then(|r| r.strip_prefix(' ')) else {
        return Ok(None);
    };
    if version != SENTINEL_VERSION.to_string() {
        return Err(format!(
            "the agent relay there speaks `{SENTINEL} {version}`, this devsandbox {SENTINEL_VERSION}: update devsandbox on the older side"
        ));
    }
    Ok(Some(()))
}

/// How [`serve`] ended.
#[derive(Debug)]
pub enum End {
    /// The relay never came up: no sentinel, or the `Hello`s failed.
    Handshake(String),
    /// No frame for `mux::KEEPALIVE_TIMEOUT`: `on_dead` ran.
    Dead,
    /// The stream ended (the far end exited, the transport closed).
    Closed,
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            End::Handshake(why) => write!(f, "agent relay handshake failed: {why}"),
            End::Dead => write!(f, "agent relay silent for {}s", mux::KEEPALIVE_TIMEOUT.as_secs()),
            End::Closed => write!(f, "agent relay closed"),
        }
    }
}

/// Serve one agent relay over `reader`/`writer` until it ends: skip to the
/// sentinel, exchange `Hello`s advertising `caps::SSH_AGENT`, then answer
/// each `Open { SSH_AGENT }` with a connection to `agent()` when `allowed()`
/// says so at that moment, refusing it otherwise (and every other channel).
/// Keepalive pings go out every `mux::KEEPALIVE_INTERVAL`; after
/// `mux::KEEPALIVE_TIMEOUT` without a frame `on_dead` runs once: it must
/// end `reader` (kill the ssh) so this returns.
pub fn serve(
    mut reader: impl Read,
    mut writer: impl Write + Send + 'static,
    allowed: impl Fn() -> bool,
    agent: impl Fn() -> Option<PathBuf>,
    on_dead: impl FnOnce() + Send + 'static,
) -> End {
    let mut noise = None;
    match ssh::skip_lines(&mut reader, SENTINEL_LIMIT, &mut noise, sentinel) {
        Ok(()) => {}
        Err(e) => {
            let why = match e {
                SkipError::Eof => "closed before its first line".to_string(),
                SkipError::TooLong => format!("no `{SENTINEL}` line in the first {SENTINEL_LIMIT} bytes"),
                SkipError::Io(e) => e.to_string(),
                SkipError::Refused(why) => why,
            };
            return End::Handshake(match noise {
                Some(printed) => format!("{why}; the login shell printed: {printed}"),
                None => why,
            });
        }
    }
    if let Err(e) = proto::handshake(&mut reader, &mut writer, HELLO_HASH, caps::SSH_AGENT) {
        return End::Handshake(e.to_string());
    }
    let mux = Mux::new(writer);
    let dead = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dead);
    mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
        flag.store(true, Ordering::Relaxed);
        on_dead();
    });
    mux.serve(reader, |_, ch| {
        if ch != channel::SSH_AGENT || !allowed() {
            return None;
        }
        UnixStream::connect(agent()?).ok().map(Into::into)
    });
    if dead.load(Ordering::Relaxed) { End::Dead } else { End::Closed }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        write_sentinel(&mut wire).unwrap();
        let mut r = &wire[..];
        let mut noise = None;
        assert!(ssh::skip_lines(&mut r, SENTINEL_LIMIT, &mut noise, sentinel).is_ok());
        assert!(r.is_empty(), "nothing past the sentinel consumed");
        assert_eq!(noise.as_deref(), Some("motd line"));

        let end = serve(&b"just a motd\n"[..], Vec::new(), || true, || None, || {});
        let End::Handshake(why) = end else { panic!("{end:?}") };
        assert!(why.contains("closed before") && why.contains("just a motd"), "{why}");
    }
}
