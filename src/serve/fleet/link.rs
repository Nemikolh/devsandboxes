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
//!   are refused (but for `TRANSFER`, below).
//! - [`answerer`], the end that dials (the host bridge's role): skips to the
//!   sentinel, exchanges `Hello`s advertising the cap, answers each
//!   `Open { channel }` with the connection its callback makes, refuses
//!   every other channel.
//!
//! The machines' agent relay is one (`machines::agent::SPEC`: its opener is
//! `commands::agent_relay`, its answerer `machines::agent::serve`); the
//! fleet link another (`fleet::LINK`).
//!
//! A link may also carry `TRANSFER` streams both ways ([`opener_with`],
//! [`answerer_with`]; the fleet link does): the opener binds a second socket
//! whose connections become `TRANSFER` streams to the peer, the answerer
//! opens them toward the opener through an [`Outlet`], and each end hands
//! the peer's to its `transfer::Receivers`. Both ends then advertise and
//! require `caps::TRANSFER`.
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
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::devsbd::mux::{self, Conn, Mux};
use crate::devsbd::proto::{self, caps, channel};
use crate::serve::machines::ssh::{self, SkipError};

use super::transfer::Receivers;

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

/// The opener's `TRANSFER` side ([`opener_with`]).
pub struct OpenerTransfer {
    /// Where to bind the socket whose connections become `TRANSFER` streams
    /// to the peer (bound like the link's own socket: newest wins).
    pub socket: PathBuf,
    /// Who takes the `TRANSFER` streams the peer opens.
    pub receivers: Arc<Receivers>,
}

/// The answerer's `TRANSFER` side ([`answerer_with`]).
pub struct AnswererTransfer {
    /// Who takes the `TRANSFER` streams the opener opens.
    pub receivers: Arc<Receivers>,
    /// Set to the link while it is up, to open `TRANSFER` streams toward
    /// the opener.
    pub outlet: Arc<Outlet>,
}

/// The answerer's way to open `TRANSFER` streams toward the opener while
/// its link is up (`Mux::open_host`: high-bit ids, which the opener admits
/// through its `on_host_open`).
#[derive(Default)]
pub struct Outlet {
    mux: Mutex<Option<Arc<Mux>>>,
}

impl Outlet {
    /// A new `TRANSFER` stream to the opener (for `transfer::send`); an
    /// error when no link is up.
    #[allow(dead_code)] // first caller: fetch-back (fleet step 5)
    pub fn open(&self) -> Result<Conn> {
        let mux = self.mux.lock().unwrap().clone().context("no link is up")?;
        let (_, conn) = mux.open_host(channel::TRANSFER, |_| {}).context("cannot open a transfer")?;
        Ok(conn)
    }

    fn set(&self, mux: Option<Arc<Mux>>) {
        *self.mux.lock().unwrap() = mux;
    }
}

/// [`opener_with`], carrying no `TRANSFER`.
pub fn opener(
    spec: &Spec,
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    socket: &Path,
    on_ready: impl FnOnce(&proto::Peer) -> Result<()>,
) -> Result<()> {
    opener_with(spec, reader, writer, socket, None, on_ready)
}

/// The opener over `reader`/`writer` with its socket at `socket` (temp name
/// in the same dir): bind, sentinel and `Hello`s (the peer must advertise
/// `spec.cap`), then accept and relay until it ends (see the module doc).
/// With `transfer`, its socket is bound too, `caps::TRANSFER` advertised and
/// required, and the peer's high-bit `Open { TRANSFER }`s go to its
/// receivers. `on_ready` runs once the `Hello`s are through, with the
/// peer's (its `hash` is what the far end's spec says, e.g. its semver),
/// before the first accept; its error ends the opener (the sockets
/// unlinked) with that error. `Ok` when the peer went away (the reader
/// ended or the writer broke, whichever was seen first).
pub fn opener_with(
    spec: &Spec,
    mut reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    socket: &Path,
    transfer: Option<OpenerTransfer>,
    on_ready: impl FnOnce(&proto::Peer) -> Result<()>,
) -> Result<()> {
    // Bound before the `Hello`s, so once this end has answered, the socket
    // is there.
    let socket = Socket::bind(socket)?;
    let transfer = match transfer {
        Some(t) => Some((Socket::bind(&t.socket)?, t.receivers)),
        None => None,
    };
    let (tx, ended) = mpsc::channel();
    let mut writer = Reporting { inner: writer, failed: tx.clone() };
    write_sentinel(spec, &mut writer).context("cannot write to stdout")?;
    let ours = if transfer.is_some() { caps::TRANSFER } else { 0 };
    let peer = proto::handshake(&mut reader, &mut writer, spec.hello, ours)
        .with_context(|| format!("{} handshake failed", spec.name))?;
    if peer.caps & spec.cap == 0 {
        bail!("the peer serves no {}", spec.serves);
    }
    if transfer.is_some() && peer.caps & caps::TRANSFER == 0 {
        bail!("the peer takes no transfers");
    }
    on_ready(&peer)?;
    let mux = Mux::new(writer);
    if let Some((_, receivers)) = &transfer {
        let receivers = Arc::clone(receivers);
        mux.on_host_open(Arc::new(move |_, ch| if ch == channel::TRANSFER { receivers.accept() } else { None }));
    }
    let dead = tx.clone();
    mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
        let _ = dead.send(OpenerEnd::Dead);
    });
    {
        let mux = Arc::clone(&mux);
        std::thread::spawn(move || {
            // Low-bit opens from the peer are refused (high-bit ones go to
            // `on_host_open`); `serve` refuses `Connect`.
            mux.serve(reader, |_, _| None);
            let _ = tx.send(OpenerEnd::Closed);
        });
    }
    let mut listeners = vec![(&socket.listener, spec.channel)];
    if let Some((t, _)) = &transfer {
        listeners.push((&t.listener, channel::TRANSFER));
    }
    let polled: Vec<&UnixListener> = listeners.iter().map(|(l, _)| *l).collect();
    let mut next = 1;
    let end = loop {
        if let Ok(end) = ended.try_recv() {
            break end;
        }
        match crate::serve::endpoint::accept_any(&polled, POLL) {
            // A failed `Open` is a write error, which `Reporting` reports.
            Ok(Some((i, conn))) => {
                let _ = mux.attach(next_stream_id(&mut next), conn, Some(listeners[i].1));
            }
            Ok(None) => {}
            Err(e) => break OpenerEnd::Accept(e),
        }
    };
    drop((polled, listeners));
    drop(socket);
    drop(transfer);
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
    reader: impl Read,
    writer: impl Write + Send + 'static,
    open: impl Fn() -> Option<Conn>,
    on_ready: impl FnOnce(),
    on_dead: impl FnOnce() + Send + 'static,
) -> End {
    answerer_with(spec, reader, writer, open, None, on_ready, on_dead)
}

/// [`answerer`], with `transfer`: `caps::TRANSFER` advertised, the opener's
/// `Open { TRANSFER }`s handed to its receivers, and its outlet set to this
/// link from the `Hello`s until it ends (when the opener advertised
/// `caps::TRANSFER`).
pub fn answerer_with(
    spec: &Spec,
    mut reader: impl Read,
    mut writer: impl Write + Send + 'static,
    open: impl Fn() -> Option<Conn>,
    transfer: Option<&AnswererTransfer>,
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
    let ours = spec.cap | if transfer.is_some() { caps::TRANSFER } else { 0 };
    let peer = match proto::handshake(&mut reader, &mut writer, spec.hello, ours) {
        Ok(peer) => peer,
        Err(e) => return End::Handshake(e.to_string()),
    };
    on_ready();
    let mux = Mux::new(writer);
    let outlet = transfer.filter(|_| peer.caps & caps::TRANSFER != 0).map(|t| &t.outlet);
    if let Some(outlet) = outlet {
        outlet.set(Some(Arc::clone(&mux)));
    }
    let dead = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dead);
    mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
        flag.store(true, Ordering::Relaxed);
        on_dead();
    });
    mux.serve(reader, |_, ch| match transfer {
        _ if ch == spec.channel => open(),
        Some(t) if ch == channel::TRANSFER => t.receivers.accept(),
        _ => None,
    });
    if let Some(outlet) = outlet {
        outlet.set(None);
    }
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

    /// A file each way over a real link: the hub (opener) sends to the
    /// worker (answerer) through its transfer socket, the worker to the hub
    /// through its outlet; each end's receivers by kind.
    #[test]
    fn transfers_cross_the_link_both_ways() {
        use crate::serve::fleet::transfer::{self, Header, Received};
        let dir = scratch("fleet-transfer");
        let (hub_dir, worker_dir) = (dir.join("hub"), dir.join("worker"));
        std::fs::create_dir_all(&hub_dir).unwrap();
        let receivers = |into: PathBuf, tx: mpsc::Sender<Received>| {
            let tx = std::sync::Mutex::new(tx);
            Arc::new(Receivers::new(|_| {}).with(
                "root-tar",
                1 << 20,
                move |h: &Header| Ok(into.join(&h.name)),
                move |r| tx.lock().unwrap().send(r).unwrap(),
            ))
        };
        let (hub_tx, hub_rx) = mpsc::channel();
        let (worker_tx, worker_rx) = mpsc::channel();
        let socket = hub_dir.join("fleet-w.sock");
        let xfer = endpoint::fleet_transfer_socket(&hub_dir, "w");
        let ours = OpenerTransfer { socket: xfer.clone(), receivers: receivers(hub_dir.join("in"), hub_tx) };
        let theirs = Arc::new(AnswererTransfer { receivers: receivers(worker_dir.clone(), worker_tx), outlet: Default::default() });
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        let path = socket.clone();
        std::thread::spawn(move || opener_with(&LINK, o_r, o_w, &path, Some(ours), |_| Ok(())));
        let answering = Arc::clone(&theirs);
        std::thread::spawn(move || answerer_with(&LINK, a_r, a_w, || None, Some(&answering), || {}, || {}));
        wait_for(|| UnixStream::connect(&xfer).is_ok(), "the transfer socket accepts");

        // Hub → worker: bigger than a window, so credit has to flow.
        let body: Vec<u8> = (0..600_000u32).map(|i| i as u8).collect();
        let (size, sha256) = transfer::digest(&body[..]).unwrap();
        let header = Header { kind: "root-tar".into(), name: "cfg.tar".into(), size, sha256 };
        transfer::send(UnixStream::connect(&xfer).unwrap(), &header, &body[..]).unwrap();
        let got = worker_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got.path, worker_dir.join("cfg.tar"));
        assert_eq!(std::fs::read(&got.path).unwrap(), body);
        // A kind the worker doesn't take is refused over the link.
        let other = Header { kind: "binary".into(), ..header.clone() };
        let err = transfer::send(UnixStream::connect(&xfer).unwrap(), &other, &body[..]).unwrap_err();
        assert_eq!(format!("{err:#}"), "transfer refused: nothing here receives `binary` transfers");
        // A bad body's answer still arrives, the whole body through the mux.
        let bad = Header { sha256: "0".repeat(64), ..header.clone() };
        let err = transfer::send(UnixStream::connect(&xfer).unwrap(), &bad, &body[..]).unwrap_err();
        assert!(format!("{err:#}").starts_with("transfer refused: the body's sha256 is"), "{err:#}");
        let mut long = body.clone();
        long.extend_from_slice(&[0; 1000]);
        let err = transfer::send(UnixStream::connect(&xfer).unwrap(), &header, &long[..]).unwrap_err();
        assert!(format!("{err:#}").contains("the body is longer than its 600000 bytes"), "{err:#}");
        assert_eq!(std::fs::read_dir(&worker_dir).unwrap().count(), 1, "only cfg.tar, no temp files");

        // Worker → hub, once the outlet is set (the `Hello`s are through).
        wait_for(|| theirs.outlet.mux.lock().unwrap().is_some(), "the outlet is up");
        transfer::send(theirs.outlet.open().unwrap(), &header, &body[..]).unwrap();
        let got = hub_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got.path, hub_dir.join("in/cfg.tar"));
        assert_eq!(std::fs::read(&got.path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transfer_flows() {
        assert!(proto::channel::flows(channel::TRANSFER));
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
