//! A connection to a machine's daemon (docs/machines.md). For now the
//! handshake only; step 3 grows it into the held link per machine.
//!
//! The hello says version `0.0.0`, build 0: older than any daemon, so a
//! machine connection never hands off the remote daemon (only that host's
//! own clients upgrade it). What has to match is the `protocol`.

use std::fmt;
use std::time::Duration;

use anyhow::Context;

use super::ssh::{Duplex, Peer};
use crate::serve::client::{self, Conn, Hello};
use crate::serve::proto::{self, HelloResult, Version};

/// How long a closed connection waits for ssh's stderr to say why.
const STDERR_WAIT: Duration = Duration::from_secs(2);

/// A machine connection past the hello. `conn.daemon` is the remote's
/// hello reply. Dropping it closes the socket, then kills the ssh.
pub struct Link {
    pub conn: Conn,
    pub peer: Peer,
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
    let reply = match client::hello(&mut stream, &version, &client_name()) {
        Ok(Hello::Ready(reply)) => reply,
        Ok(Hello::Closed) => return Err(LinkError::Closed { machine: machine.into(), stderr: peer.stderr_tail(STDERR_WAIT) }),
        // Build 0 is older than any daemon: a handoff means something's off.
        Ok(Hello::Handoff) => return Err(other(anyhow::anyhow!("the remote daemon asked a machine connection to take over"))),
        Err(e) => {
            // A login shell printing to stdout, say: ssh's stderr may still help.
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
    Ok(Link { conn, peer })
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

    /// A fake peer: reads the hello, checks it, answers `reply` (a line).
    fn fake_peer(reply: &'static str) -> (Duplex, std::thread::JoinHandle<Value>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            let mut r = BufReader::new(theirs.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            (&theirs).write_all(reply.as_bytes()).unwrap();
            serde_json::from_str(&line).unwrap()
        });
        (Duplex::socket(ours), peer)
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
        // The dial as tests inject it (F14).
        let dial: ssh::Dial = Arc::new(move |_, _| Ok(Duplex::socket(stream.try_clone()?)));
        let entry = super::super::Entry { name: "m".into(), target: "m".into(), roots: vec![], command: None };
        let mut link = handshake("m", dial(&entry, None).unwrap(), Duration::from_secs(5)).unwrap();
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
        let err = handshake("box", ssh::spawn(&argv, None).unwrap(), Duration::from_secs(5)).err().unwrap();
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
        let script = "read l; echo '{\"id\":0,\"result\":{\"version\":\"0.6.0\",\"build\":1,\"protocol\":1}}'; \
                      read l; echo '{\"id\":1,\"result\":[]}'; exec cat >/dev/null";
        let argv: Vec<std::ffi::OsString> = ["sh", "-c", script].into_iter().map(Into::into).collect();
        let mut link = handshake("box", ssh::spawn(&argv, None).unwrap(), Duration::from_secs(5)).unwrap();
        let r = link.conn.call("roots.list", Value::Null).unwrap();
        assert_eq!(r.result, Some(json!([])));
        link.peer.close();
    }
}
