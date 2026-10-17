//! `devsandbox agent-relay --client <c>` (hidden): the machine-side end of a
//! machine's agent relay (docs/architecture/daemon/machines.md, *Agent*).
//! The linked laptop's daemon runs it over its own ssh (no `-A`) and serves
//! the streams it opens from the laptop's agent
//! (`serve::machines::agent::serve`).
//!
//! It plays the in-container daemon's part of the helper's frame protocol
//! (`devsbd/src/daemon.rs` `accept_agent_clients`): it listens on
//! `endpoint::agent_socket` (0600, in the 0700 socket dir), and each
//! connection there becomes one `Open { channel: SSH_AGENT }` stream on the
//! mux over stdout. Processes on this host find the socket through
//! `bridge::host_agent`, which tries the socket dir's `agent-*.sock`.
//!
//! The newest relay for a client wins: it binds a temp name and renames it
//! over the socket. It exits, unlinking the socket if it is still its own,
//! when stdin ends, stdout fails, or the keepalive hears nothing for
//! `mux::KEEPALIVE_TIMEOUT`, so a partitioned link can't leave a socket that
//! accepts and hangs.

use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::devsbd::mux::{self, Mux};
use crate::devsbd::proto::{self, caps, channel};
use crate::serve::endpoint;
use crate::serve::machines::agent::{write_sentinel, HELLO_HASH};

/// How often the accept loop looks up from the listener to see whether the
/// relay has ended.
const POLL: Duration = Duration::from_millis(200);

/// The CLI entry: the socket dir (created 0700 if missing), then [`run`] on
/// stdin/stdout.
pub fn agent_relay(client: &str) -> Result<()> {
    let dir = endpoint::socket_dir()?;
    endpoint::ensure_private_dir(&dir)?;
    run(io::stdin(), io::stdout(), &dir, client)
}

/// Why the relay ended.
enum End {
    /// The peer's stream ended (or carried a bad frame).
    Closed,
    /// The keepalive heard nothing.
    Dead,
    /// Writing to the peer failed: it's gone too, seen from the other side.
    Write,
    /// The listener failed.
    Accept(io::Error),
}

/// The relay over `reader`/`writer` with its socket in `dir`: bind, sentinel
/// and `Hello`s (the peer must advertise `caps::SSH_AGENT`), then accept and
/// relay until it ends (see the module doc). `Ok` when the peer went away
/// (stdin ended or stdout broke, whichever was seen first).
pub fn run(
    mut reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    dir: &Path,
    client: &str,
) -> Result<()> {
    // Bound before the `Hello`s, so once this end has answered, the socket
    // is there.
    let socket = Socket::bind(dir, client)?;
    let (tx, ended) = mpsc::channel();
    let mut writer = Reporting { inner: writer, failed: tx.clone() };
    write_sentinel(&mut writer).context("cannot write to stdout")?;
    let peer = proto::handshake(&mut reader, &mut writer, HELLO_HASH, 0).context("agent relay handshake failed")?;
    if peer.caps & caps::SSH_AGENT == 0 {
        bail!("the peer serves no ssh-agent");
    }
    let mux = Mux::new(writer);
    let dead = tx.clone();
    mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
        let _ = dead.send(End::Dead);
    });
    {
        let mux = std::sync::Arc::clone(&mux);
        std::thread::spawn(move || {
            // Opens from the peer are refused; `serve` refuses `Connect`.
            mux.serve(reader, |_, _| None);
            let _ = tx.send(End::Closed);
        });
    }
    let mut next = 1;
    let end = loop {
        if let Ok(end) = ended.try_recv() {
            break end;
        }
        match endpoint::accept_timeout(&socket.listener, POLL) {
            // A failed `Open` is a write error, which `Reporting` reports.
            Ok(Some(conn)) => {
                let _ = mux.attach(next_stream_id(&mut next), conn, Some(channel::SSH_AGENT));
            }
            Ok(None) => {}
            Err(e) => break End::Accept(e),
        }
    };
    drop(socket);
    mux.close_all();
    match end {
        End::Closed | End::Write => Ok(()),
        End::Dead => bail!("no frame from the peer for {}s", mux::KEEPALIVE_TIMEOUT.as_secs()),
        End::Accept(e) => Err(e).context("agent relay socket"),
    }
}

/// The next stream id this end allocates: 1..2^31, wrapping back to 1, the
/// in-container daemon's id space (the high bit is the peer's).
fn next_stream_id(next: &mut u32) -> u32 {
    let id = *next;
    *next = if id >= (1 << 31) - 1 { 1 } else { id + 1 };
    id
}

/// The writer to the peer, telling the relay when a write fails (the peer is
/// gone).
struct Reporting<W> {
    inner: W,
    failed: mpsc::Sender<End>,
}

impl<W> Reporting<W> {
    fn check<T>(&self, r: io::Result<T>) -> io::Result<T> {
        if r.is_err() {
            let _ = self.failed.send(End::Write);
        }
        r
    }
}

impl<W: Write> Write for Reporting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let r = self.inner.write(buf);
        self.check(r)
    }

    fn flush(&mut self) -> io::Result<()> {
        let r = self.inner.flush();
        self.check(r)
    }
}

/// The relay's listening socket. Dropping it unlinks the socket file unless a
/// newer relay has replaced it since (another inode there).
struct Socket {
    listener: UnixListener,
    path: PathBuf,
    id: (u64, u64),
}

impl Socket {
    /// Bind a temp name in `dir` (0600), then rename it over the client's
    /// socket: a live one is another relay for this client, and the newest
    /// wins; a dead one is a leftover.
    fn bind(dir: &Path, client: &str) -> Result<Socket> {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = endpoint::agent_socket(dir, client);
        let tmp = dir.join(format!(".agent-{}-{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&tmp);
        let listener = UnixListener::bind(&tmp).with_context(|| format!("cannot bind {}", tmp.display()))?;
        let placed = (|| {
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            let meta = std::fs::symlink_metadata(&tmp)?;
            listener.set_nonblocking(true)?;
            std::fs::rename(&tmp, &path)?;
            Ok::<_, io::Error>((meta.dev(), meta.ino()))
        })();
        match placed {
            Ok(id) => Ok(Socket { listener, path, id }),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e).with_context(|| format!("cannot set up {}", path.display()))
            }
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let ours = std::fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.id);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::serve::machines::agent::{self, End as LocalEnd};
    use crate::serve::machines::ssh;

    /// A short scratch dir (socket paths stay well under 104 bytes).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-ar-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake ssh-agent: echoes whatever each connection sends.
    fn echo_agent(dir: &Path) -> PathBuf {
        let path = dir.join("fake-agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                std::thread::spawn(move || {
                    let mut out = conn.try_clone().unwrap();
                    let _ = io::copy(&mut conn, &mut out);
                });
            }
        });
        path
    }

    struct Relay {
        /// The local end's transport; shutting it down ends both sides.
        local: UnixStream,
        remote: std::thread::JoinHandle<Result<()>>,
        local_end: std::thread::JoinHandle<LocalEnd>,
        socket: PathBuf,
    }

    /// The remote core and the local end over a socket pair, the local one
    /// serving `agent` while `allowed`.
    fn relay(dir: &Path, client: &str, agent: PathBuf, allowed: Arc<AtomicBool>) -> Relay {
        let (remote_io, local_io) = UnixStream::pair().unwrap();
        let (r_in, r_out) = (remote_io.try_clone().unwrap(), remote_io);
        let dir_owned = dir.to_path_buf();
        let client_owned = client.to_string();
        let remote = std::thread::spawn(move || run(r_in, r_out, &dir_owned, &client_owned));
        let (l_in, l_out) = (local_io.try_clone().unwrap(), local_io.try_clone().unwrap());
        let local_end = std::thread::spawn(move || {
            agent::serve(l_in, l_out, move || allowed.load(Ordering::Relaxed), move || Some(agent.clone()), || {}, || {})
        });
        let socket = endpoint::agent_socket(dir, client);
        wait_for(|| UnixStream::connect(&socket).is_ok(), "the relay socket accepts");
        Relay { local: local_io, remote, local_end, socket }
    }

    fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn roundtrip(socket: &Path, msg: &[u8]) -> Vec<u8> {
        let mut c = UnixStream::connect(socket).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(msg).unwrap();
        let mut got = vec![0; msg.len()];
        c.read_exact(&mut got).unwrap();
        got
    }

    #[test]
    fn clients_of_the_remote_socket_reach_the_local_agent() {
        let dir = scratch("echo");
        let agent = echo_agent(&dir);
        let r = relay(&dir, "machine:lap top", agent, Arc::new(AtomicBool::new(true)));
        assert!(r.socket.ends_with("agent-machine_lap_top.sock"));
        let mode = std::fs::metadata(&r.socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(roundtrip(&r.socket, b"hello agent"), b"hello agent");

        // Two clients at once, interleaved.
        let mut a = UnixStream::connect(&r.socket).unwrap();
        let mut b = UnixStream::connect(&r.socket).unwrap();
        for c in [&a, &b] {
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        }
        a.write_all(b"from a").unwrap();
        b.write_all(b"from b").unwrap();
        let mut buf = [0; 6];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"from b");
        a.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"from a");

        // The local end going away ends the remote one, which unlinks its socket.
        r.local.shutdown(Shutdown::Both).unwrap();
        r.remote.join().unwrap().unwrap();
        assert!(!r.socket.exists(), "socket unlinked");
        assert!(matches!(r.local_end.join().unwrap(), LocalEnd::Closed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_open_closes_the_client_promptly() {
        let dir = scratch("deny");
        let agent = echo_agent(&dir);
        let allowed = Arc::new(AtomicBool::new(false));
        let r = relay(&dir, "machine:x", agent, Arc::clone(&allowed));
        let mut c = UnixStream::connect(&r.socket).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let _ = c.write_all(b"sign this");
        let start = Instant::now();
        let mut buf = [0; 16];
        let n = c.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "closed, not answered");
        assert!(start.elapsed() < Duration::from_secs(2), "closed promptly");
        // Allowed again: the next connection goes through.
        allowed.store(true, Ordering::Relaxed);
        assert_eq!(roundtrip(&r.socket, b"ok"), b"ok");
        r.local.shutdown(Shutdown::Both).unwrap();
        r.remote.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_relay_replaces_the_socket_and_keeps_it() {
        let dir = scratch("newer");
        let agent = echo_agent(&dir);
        let yes = || Arc::new(AtomicBool::new(true));
        let old = relay(&dir, "machine:x", agent.clone(), yes());
        let old_ino = std::fs::metadata(&old.socket).unwrap().ino();
        let new = relay(&dir, "machine:x", agent, yes());
        wait_for(|| std::fs::metadata(&new.socket).is_ok_and(|m| m.ino() != old_ino), "the newer socket in place");
        assert_eq!(roundtrip(&new.socket, b"new"), b"new");

        old.local.shutdown(Shutdown::Both).unwrap();
        old.remote.join().unwrap().unwrap();
        assert!(new.socket.exists(), "the older relay leaves the newer's socket alone");
        assert_eq!(roundtrip(&new.socket, b"still"), b"still");

        new.local.shutdown(Shutdown::Both).unwrap();
        new.remote.join().unwrap().unwrap();
        assert!(!new.socket.exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers, ["fake-agent.sock"], "no temp names left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_peer_without_an_agent_is_refused_and_the_socket_removed() {
        let dir = scratch("nocap");
        let (remote_io, mut local_io) = UnixStream::pair().unwrap();
        let (r_in, r_out) = (remote_io.try_clone().unwrap(), remote_io);
        let d = dir.clone();
        let remote = std::thread::spawn(move || run(r_in, r_out, &d, "machine:x"));
        let mut noise = None;
        ssh::skip_lines(&mut local_io, 1024, &mut noise, |l: &[u8]| {
            Ok::<_, ()>(l.starts_with(b"devsandbox-agent").then_some(()))
        })
        .unwrap();
        let mut w = local_io.try_clone().unwrap();
        proto::handshake(&mut local_io, &mut w, "test", 0).unwrap();
        let err = remote.join().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("no ssh-agent"), "{err:#}");
        assert!(!endpoint::agent_socket(&dir, "machine:x").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_ids_stay_in_the_daemon_space() {
        let mut next = 1;
        assert_eq!(next_stream_id(&mut next), 1);
        assert_eq!(next_stream_id(&mut next), 2);
        let mut top = (1 << 31) - 1;
        assert_eq!(next_stream_id(&mut top), (1 << 31) - 1);
        assert_eq!(next_stream_id(&mut top), 1);
    }
}
