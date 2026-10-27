//! The link primitive (docs/architecture/daemon/fleet.md): one byte pipe
//! between two hosts (an ssh's stdio in production) carrying the helper's
//! frame protocol (docs/architecture/helper/protocol.md), where each
//! connection to a socket on one host becomes a stream to a connection the
//! other host makes. What a link carries is its [`Spec`]: the sentinel line,
//! the channel and the cap that says the far end serves it.
//!
//! - [`opener`], the end with the socket (the in-container daemon's role):
//!   binds it, writes the sentinel, exchanges `Hello`s, and turns each
//!   accepted connection into one `Open { channel }` stream (flowing when
//!   `channel::flows` says so, `Mux::attach` picks); `Open`s from the peer
//!   are refused.
//! - [`answerer`], the end that dials (the host bridge's role): skips to the
//!   sentinel, exchanges `Hello`s advertising the cap, answers each
//!   `Open { channel }` with the connection its callback makes, refuses
//!   every other channel.
//!
//! The machines' agent relay is one (`machines::agent::SPEC`: its opener is
//! `commands::agent_relay`, its answerer `machines::agent::serve`); the
//! fleet link another (`fleet::LINK`).
//!
//! The newest opener for a socket wins: it binds a temp name and renames it
//! over the path. An opener exits, unlinking the socket if it is still its
//! own, when its reader ends, its writer fails, or the keepalive hears
//! nothing for `mux::KEEPALIVE_TIMEOUT`, so a partitioned link can't leave a
//! socket that accepts and hangs.

use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::devsbd::mux::{self, Conn, Mux};
use crate::devsbd::proto;
use crate::serve::machines::ssh::{self, SkipError};

/// What one kind of link carries.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// What errors call the link: `agent relay`, `fleet link`.
    pub name: &'static str,
    /// What the answerer serves, for the opener's refusal of a peer
    /// without `cap`: `ssh-agent`, `API`.
    pub serves: &'static str,
    /// The sentinel line's word; the line is `\n<sentinel> <version>\n`.
    pub sentinel: &'static str,
    /// The sentinel's format version; a reader refuses any other.
    pub sentinel_version: u32,
    /// The one channel the opener opens and the answerer accepts.
    pub channel: u8,
    /// The cap the answerer advertises and the opener requires.
    pub cap: u32,
    /// What both ends say in their `Hello` (informational: the frame
    /// protocol's `VERSION` is what has to match).
    pub hello: &'static str,
}

/// How much [`answerer`] reads looking for the sentinel: the login shell's
/// noise and the line itself together.
pub const SENTINEL_LIMIT: usize = 64 * 1024;

/// How often the opener's accept loop looks up from the listener to see
/// whether the link has ended.
const POLL: Duration = Duration::from_millis(200);

/// `spec`'s sentinel line, flushed. Its leading newline ends a motd printed
/// without a final one, so the sentinel always starts a line.
pub fn write_sentinel(spec: &Spec, out: &mut impl Write) -> io::Result<()> {
    write!(out, "\n{} {}\n", spec.sentinel, spec.sentinel_version)?;
    out.flush()
}

/// The sentinel matcher for `ssh::skip_lines`: exactly `spec`'s line (a
/// trailing `\r` tolerated); another version is an error, anything else
/// noise.
pub fn match_sentinel(spec: &Spec, line: &[u8]) -> Result<Option<()>, String> {
    let line = String::from_utf8_lossy(line);
    let Some(version) = line.trim_end_matches('\r').strip_prefix(spec.sentinel).and_then(|r| r.strip_prefix(' ')) else {
        return Ok(None);
    };
    if version != spec.sentinel_version.to_string() {
        return Err(format!(
            "the {} there speaks `{} {version}`, this devsandbox {}: update devsandbox on the older side",
            spec.name, spec.sentinel, spec.sentinel_version
        ));
    }
    Ok(Some(()))
}

/// Why the opener ended.
enum OpenerEnd {
    /// The peer's stream ended (or carried a bad frame).
    Closed,
    /// The keepalive heard nothing.
    Dead,
    /// Writing to the peer failed: it's gone too, seen from the other side.
    Write,
    /// The listener failed.
    Accept(io::Error),
}

/// The opener over `reader`/`writer` with its socket at `socket` (temp name
/// in the same dir): bind, sentinel and `Hello`s (the peer must advertise
/// `spec.cap`), then accept and relay until it ends (see the module doc).
/// `on_ready` runs once the `Hello`s are through, with the peer's (its
/// `hash` is what the far end's spec says, e.g. its semver), before the
/// first accept; its error ends the opener (the socket unlinked) with that
/// error. `Ok` when the peer went away (the reader ended or the writer
/// broke, whichever was seen first).
pub fn opener(
    spec: &Spec,
    mut reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    socket: &Path,
    on_ready: impl FnOnce(&proto::Peer) -> Result<()>,
) -> Result<()> {
    // Bound before the `Hello`s, so once this end has answered, the socket
    // is there.
    let socket = Socket::bind(socket)?;
    let (tx, ended) = mpsc::channel();
    let mut writer = Reporting { inner: writer, failed: tx.clone() };
    write_sentinel(spec, &mut writer).context("cannot write to stdout")?;
    let peer = proto::handshake(&mut reader, &mut writer, spec.hello, 0)
        .with_context(|| format!("{} handshake failed", spec.name))?;
    if peer.caps & spec.cap == 0 {
        bail!("the peer serves no {}", spec.serves);
    }
    on_ready(&peer)?;
    let mux = Mux::new(writer);
    let dead = tx.clone();
    mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
        let _ = dead.send(OpenerEnd::Dead);
    });
    {
        let mux = Arc::clone(&mux);
        std::thread::spawn(move || {
            // Opens from the peer are refused; `serve` refuses `Connect`.
            mux.serve(reader, |_, _| None);
            let _ = tx.send(OpenerEnd::Closed);
        });
    }
    let mut next = 1;
    let end = loop {
        if let Ok(end) = ended.try_recv() {
            break end;
        }
        match crate::serve::endpoint::accept_timeout(&socket.listener, POLL) {
            // A failed `Open` is a write error, which `Reporting` reports.
            Ok(Some(conn)) => {
                let _ = mux.attach(next_stream_id(&mut next), conn, Some(spec.channel));
            }
            Ok(None) => {}
            Err(e) => break OpenerEnd::Accept(e),
        }
    };
    drop(socket);
    mux.close_all();
    match end {
        OpenerEnd::Closed | OpenerEnd::Write => Ok(()),
        OpenerEnd::Dead => bail!("no frame from the peer for {}s", mux::KEEPALIVE_TIMEOUT.as_secs()),
        OpenerEnd::Accept(e) => Err(e).with_context(|| format!("{} socket", spec.name)),
    }
}

/// How [`answerer`] ended.
#[derive(Debug)]
pub enum End {
    /// The link never came up: no sentinel, or the `Hello`s failed.
    Handshake(String),
    /// No frame for `mux::KEEPALIVE_TIMEOUT`: `on_dead` ran.
    Dead,
    /// The stream ended (the far end exited, the transport closed).
    Closed,
}

/// The answerer over `reader`/`writer` until the link ends: skip to the
/// sentinel, exchange `Hello`s advertising `spec.cap`, then answer each
/// `Open { spec.channel }` with `open()`'s connection (`None` refuses it
/// with `Close`, as is every other channel). `open` runs on the mux's read
/// thread: it must be quick (a local connect). `on_ready` runs once the
/// `Hello`s are through (the opener's socket is bound by then). Keepalive
/// pings go out every `mux::KEEPALIVE_INTERVAL`; after
/// `mux::KEEPALIVE_TIMEOUT` without a frame `on_dead` runs once: it must
/// end `reader` (kill the ssh) so this returns.
pub fn answerer(
    spec: &Spec,
    mut reader: impl Read,
    mut writer: impl Write + Send + 'static,
    open: impl Fn() -> Option<Conn>,
    on_ready: impl FnOnce(),
    on_dead: impl FnOnce() + Send + 'static,
) -> End {
    let mut noise = None;
    if let Err(e) = ssh::skip_lines(&mut reader, SENTINEL_LIMIT, &mut noise, |l: &[u8]| match_sentinel(spec, l)) {
        let why = match e {
            SkipError::Eof => "closed before its first line".to_string(),
            SkipError::TooLong => format!("no `{}` line in the first {SENTINEL_LIMIT} bytes", spec.sentinel),
            SkipError::Io(e) => e.to_string(),
            SkipError::Refused(why) => why,
        };
        return End::Handshake(match noise {
            Some(printed) => format!("{why}; the login shell printed: {printed}"),
            None => why,
        });
    }
    if let Err(e) = proto::handshake(&mut reader, &mut writer, spec.hello, spec.cap) {
        return End::Handshake(e.to_string());
    }
    on_ready();
    let mux = Mux::new(writer);
    let dead = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dead);
    mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
        flag.store(true, Ordering::Relaxed);
        on_dead();
    });
    mux.serve(reader, |_, ch| if ch == spec.channel { open() } else { None });
    if dead.load(Ordering::Relaxed) { End::Dead } else { End::Closed }
}

/// The next stream id the opener allocates: 1..2^31, wrapping back to 1,
/// the in-container daemon's id space (the high bit is the peer's).
fn next_stream_id(next: &mut u32) -> u32 {
    let id = *next;
    *next = if id >= (1 << 31) - 1 { 1 } else { id + 1 };
    id
}

/// The writer to the peer, telling the opener when a write fails (the peer
/// is gone).
struct Reporting<W> {
    inner: W,
    failed: mpsc::Sender<OpenerEnd>,
}

impl<W> Reporting<W> {
    fn check<T>(&self, r: io::Result<T>) -> io::Result<T> {
        if r.is_err() {
            let _ = self.failed.send(OpenerEnd::Write);
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

/// The opener's listening socket. Dropping it unlinks the socket file unless
/// a newer opener has replaced it since (another inode there).
struct Socket {
    listener: UnixListener,
    path: PathBuf,
    id: (u64, u64),
}

impl Socket {
    /// Bind a temp name next to `path` (0600), then rename it over `path`: a
    /// live socket there is another opener's, and the newest wins; a dead one
    /// is a leftover.
    fn bind(path: &Path) -> Result<Socket> {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = path.parent().unwrap_or(Path::new("."));
        // Short: the temp path must fit a socket address too.
        let tmp = dir.join(format!(".link-{}-{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&tmp);
        let listener = UnixListener::bind(&tmp).with_context(|| format!("cannot bind {}", tmp.display()))?;
        let placed = (|| {
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            let meta = std::fs::symlink_metadata(&tmp)?;
            listener.set_nonblocking(true)?;
            std::fs::rename(&tmp, path)?;
            Ok::<_, io::Error>((meta.dev(), meta.ino()))
        })();
        match placed {
            Ok(id) => Ok(Socket { listener, path: path.to_path_buf(), id }),
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
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    use serde_json::Value;

    use super::*;
    use crate::devsbd::proto::{caps, channel, Frame};
    use crate::serve::client::{self, Hello};
    use crate::serve::daemon::Exit;
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::endpoint;
    use crate::serve::fleet::LINK;
    use crate::serve::proto::Version;

    fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// An opener and an answerer of [`LINK`] back to back over two pipes
    /// (stdio, as over ssh), the answerer serving `open`. Returns once the
    /// opener's socket accepts. The pipes can't be shut from outside, so
    /// both ends outlive the test (their keepalives are 15 s apart).
    fn link(socket: &Path, open: impl Fn() -> Option<Conn> + Send + 'static) {
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        let path = socket.to_path_buf();
        std::thread::spawn(move || opener(&LINK, o_r, o_w, &path, |_| Ok(())));
        std::thread::spawn(move || answerer(&LINK, a_r, a_w, open, || {}, || {}));
        wait_for(|| UnixStream::connect(socket).is_ok(), "the opener's socket accepts");
    }

    #[test]
    fn a_conn_through_the_link_reaches_the_daemon() {
        let dir = scratch("fleet-link");
        std::fs::create_dir_all(&dir).unwrap();
        let daemon = spawn_daemon(&dir, opts(1, 1000));
        let socket = dir.join("fleet-w.sock");
        let daemon_dir = dir.clone();
        link(&socket, move || endpoint::connect(&daemon_dir).ok().map(Into::into));
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // The daemon may still be binding: the link's `Open` is refused until it does.
        let (stream, hello) = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut stream = UnixStream::connect(&socket).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                match client::hello(&mut stream, &Version { semver: "0.0.0".into(), build: 0 }, "test") {
                    Ok(Hello::Ready(reply)) => break (stream, reply),
                    other => assert!(Instant::now() < deadline, "no hello through the link: {other:?}"),
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        assert_eq!((hello.version.as_str(), hello.protocol), ("0.6.0", crate::serve::proto::PROTOCOL));
        stream.set_read_timeout(None).unwrap();
        let mut conn = client::Conn::new(stream, hello).unwrap();
        let r = conn.call("nope", Value::Null).unwrap();
        assert_eq!(r.error.unwrap().code, "unknown-method");
        drop(conn);
        // The link's stream closed with the client: the daemon idles out.
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_answerer_refuses_other_channels() {
        // Play the opener by hand: sentinel, `Hello`, then `Open`s.
        let (a_r, mut o_w) = std::io::pipe().unwrap();
        let (mut o_r, a_w) = std::io::pipe().unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let theirs = std::sync::Mutex::new(Some(theirs));
        std::thread::spawn(move || {
            answerer(&LINK, a_r, a_w, move || theirs.lock().unwrap().take().map(Into::into), || {}, || {})
        });
        write_sentinel(&LINK, &mut o_w).unwrap();
        let peer = proto::handshake(&mut o_r, &mut o_w, "test", 0).unwrap();
        assert_eq!(peer.caps, caps::API);

        for ch in [channel::SSH_AGENT, channel::RPC] {
            proto::write_frame(&mut o_w, &Frame::Open { stream: 1, channel: ch }).unwrap();
            match proto::read_frame(&mut o_r).unwrap() {
                Some(Frame::Close { stream: 1, .. }) => {}
                other => panic!("channel {ch}: {other:?}"),
            }
        }
        // `API` is answered: the stream is flowing, so a `Window` credit
        // comes back once data is consumed; data reaches the callback's end.
        proto::write_frame(&mut o_w, &Frame::Open { stream: 3, channel: channel::API }).unwrap();
        proto::write_frame(&mut o_w, &Frame::Data { stream: 3, bytes: b"hi".to_vec() }).unwrap();
        let mut ours = ours;
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = [0; 2];
        ours.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi");
        ours.write_all(b"yo").unwrap();
        loop {
            match proto::read_frame(&mut o_r).unwrap() {
                Some(Frame::Data { stream: 3, bytes }) => {
                    assert_eq!(bytes, b"yo");
                    break;
                }
                Some(Frame::Window { .. } | Frame::Ping(_) | Frame::Pong(_)) => {}
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_newer_opener_replaces_the_socket_and_keeps_it() {
        let dir = scratch("fleet-newer");
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("fleet-w.sock");
        // Each answerer connects to its own echo, tagged so the test sees which link answered.
        let tagged = |tag: &'static [u8]| {
            move || {
                let (ours, theirs) = UnixStream::pair().ok()?;
                std::thread::spawn(move || {
                    let mut ours = ours;
                    let _ = ours.write_all(tag);
                });
                Some(theirs.into())
            }
        };
        link(&socket, tagged(b"old"));
        let old_ino = std::fs::metadata(&socket).unwrap().ino();
        link(&socket, tagged(b"new"));
        wait_for(|| std::fs::metadata(&socket).is_ok_and(|m| m.ino() != old_ino), "the newer socket in place");
        let mut c = UnixStream::connect(&socket).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = [0; 3];
        c.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"new");
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers, ["fleet-w.sock"], "no temp names left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sentinel_matcher_takes_only_its_line() {
        assert_eq!(match_sentinel(&LINK, b"devsandbox-fleet 1"), Ok(Some(())));
        assert_eq!(match_sentinel(&LINK, b"devsandbox-fleet 1\r"), Ok(Some(())));
        assert_eq!(match_sentinel(&LINK, b"devsandbox-agent 1"), Ok(None));
        let err = match_sentinel(&LINK, b"devsandbox-fleet 2").unwrap_err();
        assert!(err.starts_with("the fleet link there speaks `devsandbox-fleet 2`"), "{err}");
    }

    #[test]
    fn api_flows() {
        assert!(proto::channel::flows(channel::API));
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
