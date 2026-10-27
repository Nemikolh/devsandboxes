//! A connection to a machine's daemon (docs/architecture/daemon/machines.md):
//! the handshake.
//! `machines.probe`/`add` use one briefly; `service` holds one per machine.
//!
//! Before the hello comes the relay's sentinel line (`relay::write_sentinel`):
//! whatever the remote login shell printed ahead of it is skipped, and the
//! first such line kept for the log.
//!
//! The hello says version `0.0.0`, build 0: older than any daemon, so a
//! machine connection never hands off the remote daemon (only that host's
//! own clients upgrade it). What has to match is the `protocol`.

use std::fmt;
use std::time::Duration;

use anyhow::Context;

use super::ssh::{self, Duplex, Peer, SkipError};
use crate::serve::client::{self, Conn, Hello};
use crate::serve::proto::{self, HelloResult, Version};
use crate::serve::relay::{SENTINEL, SENTINEL_VERSION};

/// How long a closed connection waits for ssh's stderr to say why.
const STDERR_WAIT: Duration = Duration::from_secs(2);

/// How much a link reads looking for the sentinel: the login shell's noise
/// and the line itself together.
const SENTINEL_LIMIT: usize = 64 * 1024;

/// A machine connection past the hello. `conn.daemon` is the remote's
/// hello reply. Dropping it closes the socket, then kills the ssh.
pub struct Link {
    pub conn: Conn,
    /// Held for its drop (kills the ssh).
    #[allow(dead_code)] // read by tests
    pub peer: Peer,
    /// The first line the remote login shell printed before the sentinel,
    /// cut short; `None` when it printed nothing.
    pub noise: Option<String>,
}

impl Link {
    pub fn remote(&self) -> &HelloResult {
        &self.conn.daemon
    }
}

/// Why [`handshake`] failed.
#[derive(Debug)]
pub enum LinkError {
    /// The remote speaks another protocol: one side needs an upgrade.
    Protocol { machine: String, remote_version: String, remote_protocol: u32 },
    /// The connection ended before the hello's reply: ssh failed, or the
    /// remote has no (working) devsandbox. `stderr` is ssh's last words.
    Closed { machine: String, stderr: String },
    Other { machine: String, error: anyhow::Error },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Protocol { machine, remote_version, remote_protocol } => write!(
                f,
                "{machine} runs devsandbox {remote_version} (protocol {remote_protocol}); this one is {} \
                 (protocol {}): upgrade one of them",
                Version::current().semver,
                proto::PROTOCOL
            ),
            LinkError::Closed { machine, stderr } if stderr.is_empty() => {
                write!(f, "{machine}: the connection closed before devsandbox answered (ssh said nothing)")
            }
            LinkError::Closed { machine, stderr } => write!(f, "{machine}: {stderr}"),
            LinkError::Other { machine, error } => write!(f, "{machine}: {error:#}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// Say hello over `duplex` to machine `machine` (its name, for errors),
/// waiting up to `timeout` for the reply (ssh's connect and auth included).
/// An equal `protocol` is a [`Link`]; anything else a [`LinkError`].
pub fn handshake(machine: &str, duplex: Duplex, timeout: Duration) -> Result<Link, LinkError> {
    let Duplex { mut stream, peer } = duplex;
    let other = |error: anyhow::Error| LinkError::Other { machine: machine.into(), error };
    let version = Version { semver: "0.0.0".into(), build: 0 };
    stream.set_read_timeout(Some(timeout)).context("cannot set a read timeout").map_err(other)?;
    let mut noise = None;
    match ssh::skip_lines(&mut stream, SENTINEL_LIMIT, &mut noise, sentinel) {
        Ok(()) => {}
        Err(SkipError::Eof) => return Err(LinkError::Closed { machine: machine.into(), stderr: peer.stderr_tail(STDERR_WAIT) }),
        Err(SkipError::TooLong) => {
            let printed = noise.as_deref().unwrap_or_default();
            return Err(other(anyhow::anyhow!(
                "no `{SENTINEL}` line in the first {SENTINEL_LIMIT} bytes; the login shell printed: {printed}"
            )));
        }
        Err(SkipError::Io(e)) => {
            let mut e = anyhow::Error::new(e).context(format!("waiting for the relay's `{SENTINEL}` line"));
            if let Some(printed) = &noise {
                e = e.context(format!("the login shell printed: {printed}"));
            }
            let tail = peer.stderr_tail(Duration::ZERO);
            return Err(other(if tail.is_empty() { e } else { e.context(tail) }));
        }
        Err(SkipError::Refused(e)) => return Err(other(e)),
    }
    let reply = match client::hello(&mut stream, &version, &client_name()) {
        Ok(Hello::Ready(reply)) => reply,
        Ok(Hello::Closed) => return Err(LinkError::Closed { machine: machine.into(), stderr: peer.stderr_tail(STDERR_WAIT) }),
        // Build 0 is older than any daemon: a handoff means something's off.
        Ok(Hello::Handoff) => return Err(other(anyhow::anyhow!("the remote daemon asked a machine connection to take over"))),
        Err(e) => {
            // A reply that doesn't parse (the relay's daemon misbehaving):
            // ssh's stderr may still help.
            let tail = peer.stderr_tail(Duration::ZERO);
            let e = if tail.is_empty() { e } else { e.context(tail) };
            return Err(other(e));
        }
    };
    if reply.protocol != proto::PROTOCOL {
        return Err(LinkError::Protocol {
            machine: machine.into(),
            remote_version: reply.version,
            remote_protocol: reply.protocol,
        });
    }
    stream.set_read_timeout(None).context("cannot clear the read timeout").map_err(other)?;
    let conn = Conn::new(stream, reply).map_err(other)?;
    Ok(Link { conn, peer, noise })
}

/// The matcher of the sentinel line: exactly `devsandbox-api 1` (a trailing
/// `\r` tolerated; its leading newline makes it start a line). Another
/// version is an error, anything else noise.
fn sentinel(line: &[u8]) -> anyhow::Result<Option<()>> {
    let line = String::from_utf8_lossy(line);
    let Some(version) = line.trim_end_matches('\r').strip_prefix(SENTINEL).and_then(|r| r.strip_prefix(' ')) else {
        return Ok(None);
    };
    if version != SENTINEL_VERSION.to_string() {
        anyhow::bail!(
            "the relay there speaks `{SENTINEL} {version}`, this devsandbox {SENTINEL_VERSION}: update devsandbox on the older side"
        );
    }
    Ok(Some(()))
}

/// `machine:<this host's name>`: how the remote's `serve.log` names us.
pub fn client_name() -> String {
    format!("machine:{}", hostname())
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates
    // within it or truncates (we cut at the first NUL either way).
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let name = if ok { String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(buf.len())]).into_owned() } else { String::new() };
    if name.is_empty() { "unknown".into() } else { name }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::time::Instant;

    use serde_json::{Value, json};

    use super::*;
    use crate::serve::daemon::Exit;
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::endpoint;
    use crate::serve::machines::ssh;

    /// A fake peer: writes `before` (the login shell's noise and the
    /// sentinel), reads the hello, answers `reply` (a line), returns the hello.
    fn fake_peer_after(before: String, reply: &'static str) -> (Duplex, std::thread::JoinHandle<Value>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            (&theirs).write_all(before.as_bytes()).unwrap();
            let mut r = BufReader::new(theirs.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            (&theirs).write_all(reply.as_bytes()).unwrap();
            serde_json::from_str(&line).unwrap()
        });
        (Duplex::socket(ours), peer)
    }

    /// [`fake_peer_after`] a quiet login shell.
    fn fake_peer(reply: &'static str) -> (Duplex, std::thread::JoinHandle<Value>) {
        fake_peer_after(sentinel_line(), reply)
    }

    fn sentinel_line() -> String {
        let mut out = Vec::new();
        crate::serve::relay::write_sentinel(&mut out, true).unwrap();
        String::from_utf8(out).unwrap()
    }

    const READY: &str = "{\"id\":0,\"result\":{\"version\":\"0.6.0\",\"build\":1,\"protocol\":1}}\n";

    #[test]
    fn the_login_shells_noise_is_skipped_and_kept() {
        let before = format!("Welcome to box!\r\n\nLast login: today\n{}", sentinel_line());
        let (duplex, peer) = fake_peer_after(before, READY);
        let link = handshake("m", duplex, Duration::from_secs(5)).unwrap();
        assert_eq!(peer.join().unwrap()["method"], "hello");
        assert_eq!(link.noise.as_deref(), Some("Welcome to box!"));

        // A motd without a final newline: the sentinel's own ends it.
        let (duplex, peer) = fake_peer_after(format!("motd{}", sentinel_line()), READY);
        let link = handshake("m", duplex, Duration::from_secs(5)).unwrap();
        peer.join().unwrap();
        assert_eq!(link.noise.as_deref(), Some("motd"));

        // CRLF on the sentinel; a quiet shell leaves no noise.
        let (duplex, peer) = fake_peer_after(sentinel_line().replace('\n', "\r\n"), READY);
        let link = handshake("m", duplex, Duration::from_secs(5)).unwrap();
        peer.join().unwrap();
        assert_eq!(link.noise, None);
    }

    #[test]
    fn no_sentinel_is_closed_or_too_long() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (&theirs).write_all(b"motd\n{\"id\":0}\n").unwrap();
        drop(theirs);
        let err = handshake("box", Duplex::socket(ours), Duration::from_secs(5)).err().unwrap();
        assert!(matches!(err, LinkError::Closed { .. }), "{err:?}");

        let (ours, theirs) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            let line = format!("{}\n", "x".repeat(1000));
            // Stops once the link gives up and drops its end.
            while (&theirs).write_all(line.as_bytes()).is_ok() {}
        });
        let err = handshake("box", Duplex::socket(ours), Duration::from_secs(5)).err().unwrap();
        writer.join().unwrap();
        assert!(matches!(err, LinkError::Other { .. }), "{err:?}");
        let want = format!("box: no `devsandbox-api` line in the first 65536 bytes; the login shell printed: {}", "x".repeat(200));
        assert_eq!(err.to_string(), want);
    }

    #[test]
    fn another_sentinel_version_says_update() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (&theirs).write_all(b"\ndevsandbox-api 2\n").unwrap();
        let err = handshake("box", Duplex::socket(ours), Duration::from_secs(5)).err().unwrap();
        assert!(err.to_string().ends_with("update devsandbox on the older side"), "{err}");
    }

    #[test]
    fn a_silent_relay_times_out_with_the_noise() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (&theirs).write_all(b"motd\n").unwrap();
        let err = handshake("box", Duplex::socket(ours), Duration::from_millis(200)).err().unwrap();
        let msg = err.to_string();
        assert!(matches!(err, LinkError::Other { .. }), "{err:?}");
        assert!(msg.contains("the login shell printed: motd") && msg.contains("`devsandbox-api` line"), "{msg}");
        drop(theirs);
    }

    #[test]
    fn hello_against_a_test_daemon() {
        let dir = scratch("link");
        let daemon = spawn_daemon(&dir, opts(1, 500));
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match endpoint::connect(&dir) {
                Ok(s) => break s,
                Err(e) if Instant::now() > deadline => panic!("{e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        // The dial as tests inject it.
        let dial: ssh::Dial = Arc::new(move |_| ssh::test_support::relayed(stream.try_clone()?));
        let entry = super::super::Entry::ssh("m", "m");
        let mut link = handshake("m", dial(&entry).unwrap(), Duration::from_secs(5)).unwrap();
        assert_eq!((link.remote().protocol, link.remote().version.as_str(), link.remote().handoff), (proto::PROTOCOL, "0.6.0", false));
        let r = link.conn.call("nope", Value::Null).unwrap();
        assert_eq!(r.error.unwrap().code, "unknown-method");
        drop((link, dial));
        // Build 0 never hands the daemon off: it idles out as usual.
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hello_says_version_zero_and_its_host() {
        let (duplex, peer) = fake_peer("{\"id\":0,\"result\":{\"version\":\"0.6.0\",\"build\":1,\"protocol\":1}}\n");
        let link = handshake("m", duplex, Duration::from_secs(5)).unwrap();
        let hello = peer.join().unwrap();
        assert_eq!(hello["method"], "hello");
        assert_eq!((hello["params"]["version"].clone(), hello["params"]["build"].clone()), (json!("0.0.0"), json!(0)));
        let client = hello["params"]["client"].as_str().unwrap();
        assert!(client.starts_with("machine:") && client.len() > "machine:".len(), "{client}");
        assert_eq!(link.remote().build, 1);
    }

    #[test]
    fn another_protocol_names_both_versions() {
        let (duplex, peer) = fake_peer("{\"id\":0,\"result\":{\"version\":\"0.7.0\",\"build\":1,\"protocol\":2}}\n");
        let err = handshake("my-server", duplex, Duration::from_secs(5)).err().unwrap();
        peer.join().unwrap();
        assert!(matches!(err, LinkError::Protocol { remote_protocol: 2, .. }), "{err:?}");
        let want = format!(
            "my-server runs devsandbox 0.7.0 (protocol 2); this one is {} (protocol 1): upgrade one of them",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(err.to_string(), want);
    }

    #[test]
    fn an_ssh_that_fails_says_why() {
        let argv: Vec<std::ffi::OsString> =
            ["sh", "-c", "echo 'me@box: Permission denied (publickey).' >&2; exit 255"].into_iter().map(Into::into).collect();
        let err = handshake("box", ssh::spawn(&argv).unwrap(), Duration::from_secs(5)).err().unwrap();
        assert!(matches!(err, LinkError::Closed { .. }), "{err:?}");
        assert_eq!(err.to_string(), "box: me@box: Permission denied (publickey).");

        // A silent EOF still says what happened.
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let err = handshake("box", Duplex::socket(ours), Duration::from_secs(5)).err().unwrap();
        assert!(err.to_string().contains("closed before devsandbox answered"), "{err}");
    }

    #[test]
    fn a_spawned_peer_links() {
        // The pumps carry the hello both ways, and the conn's calls after it.
        let script = "echo motd; printf '\\ndevsandbox-api 1\\n'; read l; echo '{\"id\":0,\"result\":{\"version\":\"0.6.0\",\"build\":1,\"protocol\":1}}'; \
                      read l; echo '{\"id\":1,\"result\":[]}'; exec cat >/dev/null";
        let argv: Vec<std::ffi::OsString> = ["sh", "-c", script].into_iter().map(Into::into).collect();
        let mut link = handshake("box", ssh::spawn(&argv).unwrap(), Duration::from_secs(5)).unwrap();
        assert_eq!(link.noise.as_deref(), Some("motd"));
        let r = link.conn.call("roots.list", Value::Null).unwrap();
        assert_eq!(r.result, Some(json!([])));
        link.peer.close();
    }
}
