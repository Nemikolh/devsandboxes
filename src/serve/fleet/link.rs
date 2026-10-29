//! The link primitive (docs/architecture/daemon/fleet.md): one byte pipe
//! between two hosts (an ssh's stdio in production, `fleet::transport`)
//! carrying the helper's frame protocol
//! (docs/architecture/helper/protocol.md), where each connection to a socket
//! on one host becomes a stream to a connection the other host makes. A
//! link's [`Spec`] is its sentinel line and the channel it stands for.
//!
//! Both ends are the same [`Endpoint`] once the `Hello`s are through: OUT,
//! the sockets it binds (each accepted connection one `Open { channel }`
//! stream, dropped unopened when the peer didn't advertise that channel)
//! and an [`Outlet`] its own code opens streams through; IN, its own table
//! ([`Serves`]: channel → handler) answering the peer's streams. The two
//! roles differ only in the handshake and the stream id space:
//!
//! - [`opener_with`], the end ssh runs (it writes the sentinel): binds its
//!   sockets, writes the sentinel, exchanges `Hello`s; it opens low-bit ids
//!   (`next_stream_id`, `Mux::attach`: flowing when `channel::flows` says
//!   so, legacy otherwise) and answers the peer's high-bit ones
//!   (`Mux::on_host_open`).
//! - [`answerer_with`], the end that dials: skips to the sentinel, binds its
//!   sockets, exchanges `Hello`s; it opens high-bit ids (`Mux::open_host`:
//!   flowing channels only, every link channel flows; a socket's connection
//!   spliced to the stream) and answers the peer's low-bit ones.
//!
//! Each end's `Hello` advertises the caps of the channels its table serves
//! ([`cap_of`]); an `Open` on a channel missing from it is refused with
//! `Close`, and the link stays up. The opener requires the peer to serve
//! the spec's channel. Handlers run on the mux's read thread, so they
//! return fast: a local connect, or a socket pair whose other end a thread
//! takes (`transfer::Receivers::accept`).
//!
//! The fleet link is the one kind (`fleet::LINK`: `API` served by the
//! answerer, `TRANSFER` by both ends, each end's `transfer::Receivers`
//! through [`Serves::transfers`], `AGENT` by the opener, `fleet::agent`).
//!
//! The newest end binding a socket wins: it binds a temp name and renames
//! it over the path. An end unlinks its sockets, if they are still its own,
//! when the link ends: the opener when its reader ends, its writer fails,
//! or the keepalive hears nothing for `mux::KEEPALIVE_TIMEOUT`; the answerer
//! when its reader ends (`on_dead` ends it after that same silence). So a
//! partitioned link can't leave a socket that accepts and hangs.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::devsbd::mux::{self, Conn, Mux};
use crate::devsbd::proto::{self, caps, channel};
use crate::serve::machines::ssh::{self, SkipError};

#[cfg(test)]
use super::transfer::Receivers;

/// What one kind of link is.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// What errors call the link: `fleet link`.
    pub name: &'static str,
    /// What the peer serves on `channel`, for the opener's refusal of a
    /// peer that doesn't: `ssh-agent`, `API`.
    pub serves: &'static str,
    /// The sentinel line's word; the line is `\n<sentinel> <version>\n`.
    pub sentinel: &'static str,
    /// The sentinel's format version; a reader refuses any other.
    pub sentinel_version: u32,
    /// The channel the link stands for: the opener requires the peer to
    /// advertise it ([`cap_of`]); the opener opens it, the answerer serves
    /// it.
    pub channel: u8,
    /// What both ends say in their `Hello` (the frame protocol's `VERSION`
    /// is what has to match; a fleet worker follows the build its hub says
    /// here, `fleet::update`).
    pub hello: &'static str,
}

/// The cap a link end advertises to say it serves `channel`; `None` for a
/// channel no link carries.
pub const fn cap_of(channel: u8) -> Option<u32> {
    match channel {
        channel::SSH_AGENT => Some(caps::SSH_AGENT),
        channel::API => Some(caps::API),
        channel::TRANSFER => Some(caps::TRANSFER),
        channel::PTY => Some(caps::PTY),
        channel::TUNNEL => Some(caps::TUNNEL),
        channel::AGENT => Some(caps::AGENT),
        _ => None,
    }
}

/// Whether a peer whose `Hello` said `caps` serves `channel`.
fn advertised(caps: u32, channel: u8) -> bool {
    cap_of(channel).is_some_and(|cap| caps & cap != 0)
}

/// One served channel's handler: the local end of a stream the peer opens,
/// `None` to refuse it. Runs on the mux's read thread: it must return fast.
pub type Handler<'a> = Box<dyn Fn() -> Option<Conn> + Send + Sync + 'a>;

/// What one end of a link serves the peer: channel → [`Handler`]. The end's
/// `Hello` advertises their caps ([`cap_of`]); any other channel is
/// refused.
#[derive(Default)]
pub struct Serves<'a> {
    table: Vec<(u8, Handler<'a>)>,
}

impl<'a> Serves<'a> {
    /// Also serve `channel` with `handler` (replacing an earlier one).
    /// Panics on a channel no link carries (no [`cap_of`]): the channels are
    /// constants, so that's a bug here.
    pub fn with(mut self, channel: u8, handler: impl Fn() -> Option<Conn> + Send + Sync + 'a) -> Self {
        assert!(cap_of(channel).is_some(), "channel {channel} isn't a link channel");
        self.table.retain(|(c, _)| *c != channel);
        self.table.push((channel, Box::new(handler)));
        self
    }

    /// Also serve `TRANSFER`, each stream handed to `receivers` (tests: the
    /// daemon's ends take `TRANSFER` with [`with`](Self::with)).
    #[cfg(test)]
    pub fn transfers(self, receivers: Arc<Receivers>) -> Self {
        self.with(channel::TRANSFER, move || receivers.accept())
    }

    /// The caps this end advertises.
    fn caps(&self) -> u32 {
        self.table.iter().filter_map(|(c, _)| cap_of(*c)).fold(0, |all, cap| all | cap)
    }

    /// The answer to the peer's `Open { channel }`: its handler's, `None`
    /// (refused) for a channel not served.
    fn answer(&self, channel: u8) -> Option<Conn> {
        self.table.iter().find(|(c, _)| *c == channel).and_then(|(_, open)| open())
    }
}

/// One end of a link, whichever its role (see the module doc). `serves`
/// outlives the call for an opener (`'s = 'static`): its peer's streams are
/// answered on a thread of the link's own.
#[derive(Default)]
pub struct Endpoint<'a, 's> {
    /// OUT: a channel and its socket path each, bound before the `Hello`s
    /// (a temp name in the same dir first, [`Socket::bind`]); each
    /// accepted connection one stream on the channel.
    pub sockets: &'a [(u8, &'a Path)],
    /// IN: what this end answers the peer's streams with.
    pub serves: Serves<'s>,
    /// OUT for this end's own code: set to the link from the `Hello`s until
    /// it ends.
    pub outlet: Option<&'a Outlet>,
}

/// How much [`answerer_with`] reads looking for the sentinel: the login shell's
/// noise and the line itself together.
pub const SENTINEL_LIMIT: usize = 64 * 1024;

/// How often an end's accept loop looks up from its listeners to see
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

/// Why a link's body ended, as each part reports it ([`run`]).
enum Ending {
    /// The peer's stream ended (or carried a bad frame).
    Closed,
    /// The keepalive heard nothing.
    Dead,
    /// Writing to the peer failed: it's gone too, seen from the other side.
    Write,
    /// A listener failed.
    Accept(io::Error),
}

/// Which end of the link this is: who writes the sentinel, and which half
/// of the stream ids it allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Writes the sentinel; opens low-bit ids.
    Opener,
    /// Reads the sentinel; opens high-bit ids (`Mux::open_host`).
    Answerer,
}

/// A live link's OUT half: the streams this end opens, in its role's id
/// space.
struct Out {
    mux: Arc<Mux>,
    role: Role,
    /// The peer's caps: what it serves.
    theirs: u32,
    /// The opener's next id ([`next_stream_id`]).
    next: Mutex<u32>,
}

impl Out {
    fn next_id(&self) -> u32 {
        next_stream_id(&mut self.next.lock().unwrap())
    }

    /// A new stream on flowing `channel`: this end's half of a socket pair
    /// the mux relays. An error when the peer didn't advertise `channel`,
    /// or it doesn't flow, or the link is gone.
    fn open(&self, channel: u8) -> Result<Conn> {
        if !advertised(self.theirs, channel) {
            bail!("the peer serves no channel {channel}");
        }
        let opened = match self.role {
            Role::Answerer => self.mux.open_host(channel, |_| {}).map(|(_, conn)| conn),
            Role::Opener if !channel::flows(channel) => {
                Err(io::Error::new(io::ErrorKind::InvalidInput, format!("channel {channel} is not a flowing channel")))
            }
            Role::Opener => UnixStream::pair().and_then(|(ours, theirs)| {
                self.mux.attach(self.next_id(), ours, Some(channel))?;
                Ok(theirs.into())
            }),
        };
        opened.with_context(|| format!("cannot open a channel {channel} stream"))
    }

    /// One accepted connection to `channel`'s socket, as a stream on it:
    /// dropped unopened (the client reads EOF at once) when the peer didn't
    /// advertise `channel`. The opener attaches the connection itself (a
    /// legacy channel too); the answerer
    /// splices it to an [`open`](Self::open)ed stream (flowing channels
    /// only, else dropped too).
    fn relay(&self, channel: u8, conn: UnixStream) {
        if !advertised(self.theirs, channel) {
            return;
        }
        match self.role {
            // A failed `Open` is a write error, which `Reporting` reports.
            Role::Opener => {
                let _ = self.mux.attach(self.next_id(), conn, Some(channel));
            }
            Role::Answerer => {
                if let Ok(stream) = self.open(channel) {
                    splice(conn, stream);
                }
            }
        }
    }
}

/// An end's way to open streams toward the peer while its link is up (an
/// `Endpoint`'s `outlet`): a `TRANSFER` one for `transfer::send`, an
/// `AGENT` one (`fleet::agent`).
#[derive(Default)]
pub struct Outlet {
    /// The live link's OUT half.
    up: Mutex<Option<Arc<Out>>>,
    /// The live link's ending: [`hang_up`](Self::hang_up) sends on it.
    end: Mutex<Option<mpsc::Sender<Ending>>>,
}

impl Outlet {
    /// A new stream on flowing `channel` to the peer; an error when no link
    /// is up, the peer didn't advertise `channel`, or `channel` doesn't
    /// flow.
    pub fn open(&self, channel: u8) -> Result<Conn> {
        let out = self.up.lock().unwrap().clone().context("no link is up")?;
        out.open(channel)
    }

    /// Whether a link is up (its `Hello`s through, not ended yet).
    pub fn is_up(&self) -> bool {
        self.up.lock().unwrap().is_some()
    }

    /// End the live link from this end (a peer link's end that must follow
    /// its hub's build, `fleet::follow`): its body ends as when the peer's
    /// stream ends ([`opener_with`] returns `Ok`, [`answerer_with`]
    /// `End::Closed`), its sockets unlinked. Nothing when no link is up.
    pub fn hang_up(&self) {
        if let Some(end) = self.end.lock().unwrap().as_ref() {
            let _ = end.send(Ending::Closed);
        }
    }

    fn set(&self, up: Option<(Arc<Out>, mpsc::Sender<Ending>)>) {
        let (up, end) = up.unzip();
        *self.up.lock().unwrap() = up;
        *self.end.lock().unwrap() = end;
    }
}

/// [`opener_with`] opening only `spec.channel`, through `socket`, and
/// serving nothing: the tests' opener.
#[cfg(test)]
pub fn opener(
    spec: &Spec,
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    socket: &Path,
    on_ready: impl FnOnce(&proto::Peer) -> Result<()>,
) -> Result<()> {
    let sockets = [(spec.channel, socket)];
    opener_with(spec, reader, writer, Endpoint { sockets: &sockets, ..Endpoint::default() }, on_ready)
}

/// The opener over `reader`/`writer`: bind `endpoint`'s sockets, sentinel
/// and `Hello`s advertising its `serves`' caps (the peer must advertise
/// `spec.channel`'s), then the link's body until it ends ([`run`], see the
/// module doc). `on_ready` runs once the `Hello`s are through, with the
/// peer's (its `hash` is what the far end's spec says, e.g. its semver),
/// before the first accept; its error ends the opener (the sockets
/// unlinked) with that error. `Ok` when the peer went away (the reader
/// ended or the writer broke, whichever was seen first); an error when the
/// keepalive heard nothing or a listener failed.
pub fn opener_with(
    spec: &Spec,
    mut reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    endpoint: Endpoint<'_, 'static>,
    on_ready: impl FnOnce(&proto::Peer) -> Result<()>,
) -> Result<()> {
    // Bound before the `Hello`s, so once this end has answered, the sockets
    // are there.
    let bound = bind(endpoint.sockets)?;
    let (tx, ended) = mpsc::channel();
    let mut writer = Reporting { inner: writer, failed: tx.clone() };
    write_sentinel(spec, &mut writer).context("cannot write to stdout")?;
    let peer = proto::handshake(&mut reader, &mut writer, spec.hello, endpoint.serves.caps())
        .with_context(|| format!("{} handshake failed", spec.name))?;
    if !advertised(peer.caps, spec.channel) {
        bail!("the peer serves no {}", spec.serves);
    }
    on_ready(&peer)?;
    let mux = Mux::new(writer);
    let serves = Arc::new(endpoint.serves);
    mux.on_host_open(Arc::new(move |_, ch| serves.answer(ch)));
    let reading = Arc::clone(&mux);
    // The reader on a thread of its own: a dead keepalive (or a failed
    // listener) ends the opener without waiting for a read that may never
    // return.
    let read = move |done: mpsc::Sender<Ending>| {
        std::thread::spawn(move || {
            // Low-bit opens from the peer are refused (high-bit ones go to
            // `on_host_open`); `serve` refuses `Connect`.
            reading.serve(reader, |_, _| None);
            let _ = done.send(Ending::Closed);
        });
    };
    let (end, _) = run(&mux, Role::Opener, peer.caps, bound, endpoint.outlet, (tx, ended), || {}, read);
    match end {
        Ending::Closed | Ending::Write => Ok(()),
        Ending::Dead => bail!("no frame from the peer for {}s", mux::KEEPALIVE_TIMEOUT.as_secs()),
        Ending::Accept(e) => Err(e).with_context(|| format!("{} socket", spec.name)),
    }
}

/// How [`answerer_with`] ended.
#[derive(Debug)]
pub enum End {
    /// The link never came up: no sentinel, a socket not bound, or the
    /// `Hello`s failed.
    Handshake(String),
    /// `answerer_with`'s `on_ready` turned the peer down, with why: nothing
    /// was served.
    Refused(String),
    /// No frame for `mux::KEEPALIVE_TIMEOUT`: `on_dead` ran.
    Dead,
    /// The stream ended (the far end exited, the transport closed).
    Closed,
}

/// [`answerer_with`] serving only `spec.channel`, with `open()`'s
/// connections, and opening nothing. `on_ready` runs once the `Hello`s are
/// through (the opener's socket is bound by then): the tests' answerer.
#[cfg(test)]
pub fn answerer(
    spec: &Spec,
    reader: impl Read,
    writer: impl Write + Send + 'static,
    open: impl Fn() -> Option<Conn> + Send + Sync,
    on_ready: impl FnOnce(),
    on_dead: impl FnOnce() + Send + 'static,
) -> End {
    answerer_with(
        spec,
        reader,
        writer,
        Endpoint { serves: Serves::default().with(spec.channel, open), ..Endpoint::default() },
        |_| {
            on_ready();
            Ok(())
        },
        on_dead,
    )
}

/// The answerer over `reader`/`writer` until the link ends: skip to the
/// sentinel, bind `endpoint`'s sockets, exchange `Hello`s advertising its
/// `serves`' caps, then the link's body ([`run`], see the module doc) on
/// this thread: each `Open { channel }` answered from `serves` (`None`, or
/// a channel not there, refuses it with `Close`). `on_ready` gets the
/// opener's `Hello` (its `hash` is the far end's `spec.hello`); an `Err`
/// ends the link at once, before anything is served, as [`End::Refused`]
/// (a fleet worker that must first follow its hub's version,
/// `fleet::worker`). Keepalive pings go out every `mux::KEEPALIVE_INTERVAL`;
/// after `mux::KEEPALIVE_TIMEOUT` without a frame `on_dead` runs once: it
/// must end `reader` (kill the ssh) so this returns. A failed listener
/// ends only the accepts (its sockets unlinked); the link stays up.
pub fn answerer_with(
    spec: &Spec,
    mut reader: impl Read,
    writer: impl Write + Send + 'static,
    endpoint: Endpoint<'_, '_>,
    on_ready: impl FnOnce(&proto::Peer) -> Result<(), String>,
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
    // Bound before this end's `Hello`, as the opener's are.
    let bound = match bind(endpoint.sockets) {
        Ok(bound) => bound,
        Err(e) => return End::Handshake(format!("{e:#}")),
    };
    let (tx, ended) = mpsc::channel();
    let mut writer = Reporting { inner: writer, failed: tx.clone() };
    let peer = match proto::handshake(&mut reader, &mut writer, spec.hello, endpoint.serves.caps()) {
        Ok(peer) => peer,
        Err(e) => return End::Handshake(e.to_string()),
    };
    if let Err(why) = on_ready(&peer) {
        return End::Refused(why);
    }
    let mux = Mux::new(writer);
    let serves = &endpoint.serves;
    let read = |done: mpsc::Sender<Ending>| {
        // High-bit opens from the peer go to `on_host_open`, unset here:
        // refused.
        mux.serve(reader, |_, ch| serves.answer(ch));
        let _ = done.send(Ending::Closed);
    };
    let (_, dead) = run(&mux, Role::Answerer, peer.caps, bound, endpoint.outlet, (tx, ended), on_dead, read);
    if dead { End::Dead } else { End::Closed }
}

/// `sockets`, each bound ([`Socket::bind`]) with its channel.
fn bind(sockets: &[(u8, &Path)]) -> Result<Vec<(Socket, u8)>> {
    sockets.iter().map(|&(ch, path)| Ok((Socket::bind(path)?, ch))).collect()
}

/// The link's body past the `Hello`s, the same for both roles, until it
/// ends: `outlet` set to it, the keepalive (reporting [`Ending::Dead`],
/// then running `on_dead`), OUT's accept loop over `bound` on a thread of
/// its own, and `read`, which serves the peer's frames (IN) and reports
/// [`Ending::Closed`] when they end: on a thread of its own (returning at
/// once) or on this one. `tx`'s receiver gets the first ending; then the
/// accepts stop (`bound` unlinked), `outlet` is unset and every stream
/// closed. Returns the first ending and whether the keepalive found the
/// link dead.
#[allow(clippy::too_many_arguments)]
fn run(
    mux: &Arc<Mux>,
    role: Role,
    theirs: u32,
    bound: Vec<(Socket, u8)>,
    outlet: Option<&Outlet>,
    (tx, ended): (mpsc::Sender<Ending>, mpsc::Receiver<Ending>),
    on_dead: impl FnOnce() + Send + 'static,
    read: impl FnOnce(mpsc::Sender<Ending>),
) -> (Ending, bool) {
    let out = Arc::new(Out { mux: Arc::clone(mux), role, theirs, next: Mutex::new(1) });
    if let Some(outlet) = outlet {
        outlet.set(Some((Arc::clone(&out), tx.clone())));
    }
    let dead = Arc::new(AtomicBool::new(false));
    {
        let (flag, tx) = (Arc::clone(&dead), tx.clone());
        mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
            flag.store(true, Ordering::Relaxed);
            let _ = tx.send(Ending::Dead);
            on_dead();
        });
    }
    let stop = AtomicBool::new(false);
    let first = std::thread::scope(|s| {
        if !bound.is_empty() {
            let (out, stop, tx) = (&out, &stop, tx.clone());
            s.spawn(move || {
                if let Err(e) = accept(bound, out, stop) {
                    let _ = tx.send(Ending::Accept(e));
                }
            });
        }
        read(tx);
        // Never disconnected: the mux's writer (`Reporting`) holds a sender.
        let first = ended.recv().unwrap_or(Ending::Closed);
        stop.store(true, Ordering::Relaxed);
        first
    });
    if let Some(outlet) = outlet {
        outlet.set(None);
    }
    mux.close_all();
    (first, dead.load(Ordering::Relaxed))
}

/// OUT's accept loop: each connection to one of `bound` relayed as a
/// stream on its channel ([`Out::relay`]), until `stop` (looked at every
/// [`POLL`]) or a listener fails. The sockets go (unlinked unless
/// replaced) when it returns.
fn accept(bound: Vec<(Socket, u8)>, out: &Out, stop: &AtomicBool) -> io::Result<()> {
    let polled: Vec<&UnixListener> = bound.iter().map(|(s, _)| &s.listener).collect();
    while !stop.load(Ordering::Relaxed) {
        if let Some((i, conn)) = crate::serve::endpoint::accept_any(&polled, POLL)? {
            out.relay(bound[i].1, conn);
        }
    }
    Ok(())
}

/// Relay `client` and `stream` both ways on threads of their own, each
/// direction's end passed on as a half-close (the answerer's socket
/// connections, `fleet::agent`'s clients).
pub(crate) fn splice(client: UnixStream, stream: Conn) {
    let (Ok(client2), Ok(stream2)) = (client.try_clone(), stream.try_clone()) else { return };
    let up = move || {
        let _ = io::copy(&mut &client, &mut &stream);
        let _ = stream.shutdown(Shutdown::Write);
    };
    let down = move || {
        let _ = io::copy(&mut &stream2, &mut &client2);
        let _ = client2.shutdown(Shutdown::Write);
    };
    let _ = std::thread::Builder::new().name("link-splice-up".into()).spawn(up);
    let _ = std::thread::Builder::new().name("link-splice-down".into()).spawn(down);
}

/// The next stream id the opener allocates: 1..2^31, wrapping back to 1,
/// the in-container daemon's id space (the high bit is the peer's).
fn next_stream_id(next: &mut u32) -> u32 {
    let id = *next;
    *next = if id >= (1 << 31) - 1 { 1 } else { id + 1 };
    id
}

/// The writer to the peer, telling the link's body when a write fails (the
/// peer is gone).
struct Reporting<W> {
    inner: W,
    failed: mpsc::Sender<Ending>,
}

impl<W> Reporting<W> {
    fn check<T>(&self, r: io::Result<T>) -> io::Result<T> {
        if r.is_err() {
            let _ = self.failed.send(Ending::Write);
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

/// A link end's listening socket (also a link's agent socket,
/// `fleet::agent::serve`). Dropping it unlinks the socket file unless a
/// newer end has replaced it since (another inode there).
pub(crate) struct Socket {
    /// Non-blocking: accept with `endpoint::accept_any`.
    pub(crate) listener: UnixListener,
    path: PathBuf,
    id: (u64, u64),
}

impl Socket {
    /// Bind a temp name next to `path` (0600), then rename it over `path`: a
    /// live socket there is another end's, and the newest wins; a dead one
    /// is a leftover.
    pub(crate) fn bind(path: &Path) -> Result<Socket> {
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
    fn link(socket: &Path, open: impl Fn() -> Option<Conn> + Send + Sync + 'static) {
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
        let ours = Serves::default().transfers(receivers(hub_dir.join("in"), hub_tx));
        let theirs = Serves::default().with(channel::API, || None).transfers(receivers(worker_dir.clone(), worker_tx));
        let outlet = Arc::new(Outlet::default());
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        let sockets = [(channel::API, socket.clone()), (channel::TRANSFER, xfer.clone())];
        std::thread::spawn(move || opener_with(&LINK, o_r, o_w, Endpoint { sockets: &paths(&sockets), serves: ours, outlet: None }, |_| Ok(())));
        let answering = Arc::clone(&outlet);
        std::thread::spawn(move || {
            answerer_with(&LINK, a_r, a_w, Endpoint { serves: theirs, outlet: Some(&answering), ..Endpoint::default() }, |_| Ok(()), || {})
        });
        wait_for(|| UnixStream::connect(&xfer).is_ok(), "the transfer socket accepts");

        // Hub → worker: bigger than a window, so credit has to flow.
        let body: Vec<u8> = (0..600_000u32).map(|i| i as u8).collect();
        let (size, sha256) = transfer::digest(&body[..]).unwrap();
        let header = Header { kind: "root-tar".into(), name: "cfg.tar".into(), size, sha256, to: None };
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
        wait_for(|| outlet.is_up(), "the outlet is up");
        transfer::send(outlet.open(channel::TRANSFER).unwrap(), &header, &body[..]).unwrap();
        let got = hub_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got.path, hub_dir.join("in/cfg.tar"));
        assert_eq!(std::fs::read(&got.path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The answerer's `on_ready` sees the opener's `Hello` and may turn the
    /// link down before anything is served.
    #[test]
    fn the_answerer_may_refuse_the_opener_it_hears() {
        let dir = scratch("fleet-refuse");
        std::fs::create_dir_all(&dir).unwrap();
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        let socket = dir.join("fleet-w.sock");
        let opener = std::thread::spawn(move || opener(&LINK, o_r, o_w, &socket, |_| Ok(())));
        let serves = Serves::default().with(channel::API, || None);
        let end = answerer_with(&LINK, a_r, a_w, Endpoint { serves, ..Endpoint::default() }, |peer| Err(format!("not {}", peer.hash)), || {});
        match end {
            End::Refused(why) => assert_eq!(why, format!("not {}", LINK.hello)),
            other => panic!("{other:?}"),
        }
        // The answerer's ends are dropped: the opener sees the link close.
        opener.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `(channel, path)`s borrowed as an `Endpoint` takes them.
    fn paths(sockets: &[(u8, PathBuf)]) -> Vec<(u8, &Path)> {
        sockets.iter().map(|(ch, p)| (*ch, p.as_path())).collect()
    }

    /// A handler whose every stream gets `tag` written and is then dropped.
    fn tagged(tag: &'static [u8]) -> impl Fn() -> Option<Conn> + Send + Sync + 'static {
        move || {
            let (mut ours, theirs) = UnixStream::pair().ok()?;
            std::thread::spawn(move || {
                let _ = ours.write_all(tag);
            });
            Some(theirs.into())
        }
    }

    /// What a stream reads until its end (5 s at most).
    fn read_all(mut c: impl Read) -> Vec<u8> {
        let mut got = Vec::new();
        let _ = c.read_to_end(&mut got);
        got
    }

    /// An outlet's stream, with a read timeout.
    fn timed(c: Conn) -> UnixStream {
        let Conn::Unix(c) = c else { panic!("an outlet's stream is a socket pair") };
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c
    }

    fn connect(socket: &Path) -> UnixStream {
        let c = UnixStream::connect(socket).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c
    }

    /// The opener serves `PTY` and has `API`, `PTY` and `TUNNEL` sockets;
    /// the answerer serves `API` and `PTY`. Returns the sockets' paths and
    /// the answerer's outlet, once it is up.
    fn two_way(dir: &Path) -> ([PathBuf; 3], Arc<Outlet>) {
        std::fs::create_dir_all(dir).unwrap();
        let sockets = [
            (channel::API, endpoint::fleet_socket(dir, "w")),
            (channel::PTY, endpoint::fleet_channel_socket(dir, "w", "pty")),
            (channel::TUNNEL, endpoint::fleet_channel_socket(dir, "w", "tunnel")),
        ];
        let paths_out = sockets.clone().map(|(_, p)| p);
        let ours = Serves::default().with(channel::PTY, tagged(b"hub pty"));
        let theirs = Serves::default().with(channel::API, tagged(b"worker api")).with(channel::PTY, tagged(b"worker pty"));
        let outlet = Arc::new(Outlet::default());
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        std::thread::spawn(move || opener_with(&LINK, o_r, o_w, Endpoint { sockets: &paths(&sockets), serves: ours, outlet: None }, |_| Ok(())));
        let answering = Arc::clone(&outlet);
        std::thread::spawn(move || {
            answerer_with(&LINK, a_r, a_w, Endpoint { serves: theirs, outlet: Some(&answering), ..Endpoint::default() }, |_| Ok(()), || {})
        });
        wait_for(|| outlet.is_up(), "the outlet is up");
        wait_for(|| UnixStream::connect(&paths_out[0]).is_ok(), "the opener's sockets accept");
        (paths_out, outlet)
    }

    #[test]
    fn a_second_channel_reaches_the_peers_handler_both_ways() {
        let dir = scratch("fleet-two-way");
        let ([api, pty, _], outlet) = two_way(&dir);
        // Opener → answerer: each socket reaches its channel's handler.
        assert_eq!(read_all(connect(&pty)), b"worker pty");
        assert_eq!(read_all(connect(&api)), b"worker api");
        // Answerer → opener, through the outlet.
        assert_eq!(read_all(timed(outlet.open(channel::PTY).unwrap())), b"hub pty");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_channel_the_peer_doesnt_serve_is_refused_and_the_link_stays_up() {
        let dir = scratch("fleet-unserved");
        let ([api, _, tunnel], outlet) = two_way(&dir);
        // The answerer didn't advertise `TUNNEL`: its socket's connections
        // are dropped unopened.
        assert_eq!(read_all(connect(&tunnel)), b"");
        // The opener didn't advertise `API` or `TUNNEL`: the outlet won't open them.
        for ch in [channel::API, channel::TUNNEL] {
            let err = outlet.open(ch).err().expect("refused");
            assert_eq!(format!("{err:#}"), format!("the peer serves no channel {ch}"));
        }
        // Nor a legacy channel, even served.
        assert!(outlet.open(channel::SSH_AGENT).is_err());
        assert_eq!(read_all(connect(&api)), b"worker api");
        assert_eq!(read_all(timed(outlet.open(channel::PTY).unwrap())), b"hub pty");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_opener_refuses_peer_opens_it_doesnt_serve() {
        // Play the answerer by hand: sentinel, `Hello`, then high-bit `Open`s.
        let dir = scratch("fleet-opener-refuses");
        std::fs::create_dir_all(&dir).unwrap();
        let (mut a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, mut a_w) = std::io::pipe().unwrap();
        let socket = dir.join("fleet-w.sock");
        let ours = Serves::default().with(channel::PTY, tagged(b"hub pty"));
        let path = socket.clone();
        std::thread::spawn(move || {
            let sockets = [(channel::API, path.as_path())];
            opener_with(&LINK, o_r, o_w, Endpoint { sockets: &sockets, serves: ours, outlet: None }, |_| Ok(()))
        });
        let mut noise = None;
        ssh::skip_lines(&mut a_r, SENTINEL_LIMIT, &mut noise, |l: &[u8]| match_sentinel(&LINK, l)).unwrap();
        let peer = proto::handshake(&mut a_r, &mut a_w, "test", caps::API).unwrap();
        assert_eq!(peer.caps, caps::PTY);
        let high = 1 << 31;
        for (stream, ch) in [(high | 1, channel::TUNNEL), (high | 2, channel::API), (3, channel::PTY)] {
            proto::write_frame(&mut a_w, &Frame::Open { stream, channel: ch }).unwrap();
            loop {
                match proto::read_frame(&mut a_r).unwrap() {
                    Some(Frame::Close { stream: s, .. }) if s == stream => break,
                    Some(Frame::Ping(_) | Frame::Pong(_)) => {}
                    other => panic!("channel {ch}: {other:?}"),
                }
            }
        }
        // A served one is answered: its handler's bytes come back.
        proto::write_frame(&mut a_w, &Frame::Open { stream: high | 4, channel: channel::PTY }).unwrap();
        let mut got = Vec::new();
        while got != b"hub pty" {
            match proto::read_frame(&mut a_r).unwrap() {
                Some(Frame::Data { stream, bytes }) if stream == high | 4 => got.extend(bytes),
                Some(Frame::Window { .. } | Frame::Ping(_) | Frame::Pong(_)) => {}
                other => panic!("{other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The roles' OUT halves swapped: the answerer binds sockets (opening
    /// high-bit streams the opener's table answers) and the opener opens
    /// through an outlet (low-bit streams the answerer's table answers).
    #[test]
    fn the_answerer_binds_sockets_and_the_opener_has_an_outlet() {
        let dir = scratch("fleet-swapped");
        std::fs::create_dir_all(&dir).unwrap();
        let (pty, tunnel) = (dir.join("a.pty.sock"), dir.join("a.tunnel.sock"));
        let api = dir.join("o.sock");
        let ours = Serves::default().with(channel::PTY, tagged(b"opener pty"));
        let theirs = Serves::default().with(channel::API, tagged(b"answerer api")).with(channel::TRANSFER, tagged(b"answerer xfer"));
        let (opening, answering) = (Arc::new(Outlet::default()), Arc::new(Outlet::default()));
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        {
            let (api, opening) = (api.clone(), Arc::clone(&opening));
            std::thread::spawn(move || {
                let sockets = [(channel::API, api.as_path())];
                opener_with(&LINK, o_r, o_w, Endpoint { sockets: &sockets, serves: ours, outlet: Some(&opening) }, |_| Ok(()))
            });
        }
        {
            let sockets = [(channel::PTY, pty.clone()), (channel::TUNNEL, tunnel.clone())];
            let answering = Arc::clone(&answering);
            std::thread::spawn(move || {
                let endpoint = Endpoint { sockets: &paths(&sockets), serves: theirs, outlet: Some(&answering) };
                answerer_with(&LINK, a_r, a_w, endpoint, |_| Ok(()), || {})
            });
        }
        wait_for(|| opening.is_up() && answering.is_up(), "both outlets are up");
        // The answerer's socket reaches the opener's table, twice (two
        // high-bit ids)...
        assert_eq!(read_all(connect(&pty)), b"opener pty");
        assert_eq!(read_all(connect(&pty)), b"opener pty");
        // ...a channel the opener doesn't serve is dropped unopened...
        assert_eq!(read_all(connect(&tunnel)), b"");
        // ...and the opener's own socket still reaches the answerer's.
        assert_eq!(read_all(connect(&api)), b"answerer api");
        // The opener's outlet: low-bit streams to the answerer's table,
        // alongside its socket's.
        assert_eq!(read_all(timed(opening.open(channel::TRANSFER).unwrap())), b"answerer xfer");
        assert_eq!(read_all(timed(opening.open(channel::API).unwrap())), b"answerer api");
        let err = opening.open(channel::PTY).err().expect("refused");
        assert_eq!(format!("{err:#}"), format!("the peer serves no channel {}", channel::PTY));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An end's own code ends its live link through its outlet: the opener
    /// returns `Ok`, its sockets unlinked; nothing happens with no link up.
    #[test]
    fn an_outlet_hangs_up_its_link() {
        let dir = scratch("fleet-hang-up");
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("fleet-w.sock");
        let outlet = Arc::new(Outlet::default());
        outlet.hang_up();
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        let opener = {
            let (outlet, path) = (Arc::clone(&outlet), socket.clone());
            std::thread::spawn(move || {
                let sockets = [(channel::API, path.as_path())];
                opener_with(&LINK, o_r, o_w, Endpoint { sockets: &sockets, outlet: Some(&outlet), ..Endpoint::default() }, |_| Ok(()))
            })
        };
        std::thread::spawn(move || answerer(&LINK, a_r, a_w, || None, || {}, || {}));
        wait_for(|| outlet.is_up(), "the outlet is up");
        outlet.hang_up();
        opener.join().unwrap().unwrap();
        assert!(!outlet.is_up());
        assert!(!socket.exists(), "unlinked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_channels_flow_and_have_caps() {
        for ch in [channel::API, channel::TRANSFER, channel::PTY, channel::TUNNEL, channel::AGENT] {
            assert!(proto::channel::flows(ch), "{ch}");
            assert!(cap_of(ch).is_some(), "{ch}");
        }
        // The containers' agent channel stays legacy.
        assert!(!proto::channel::flows(channel::SSH_AGENT));
        assert_eq!(cap_of(channel::SSH_AGENT), Some(caps::SSH_AGENT));
        assert_eq!(cap_of(channel::RPC), None);
        let all = [channel::API, channel::TRANSFER, channel::PTY, channel::TUNNEL, channel::AGENT, channel::SSH_AGENT].map(|c| cap_of(c).unwrap());
        assert_eq!(all.iter().fold(0, |a, c| a | c).count_ones(), all.len() as u32, "distinct bits");
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
    fn stream_ids_stay_in_the_daemon_space() {
        let mut next = 1;
        assert_eq!(next_stream_id(&mut next), 1);
        assert_eq!(next_stream_id(&mut next), 2);
        let mut top = (1 << 31) - 1;
        assert_eq!(next_stream_id(&mut top), (1 << 31) - 1);
        assert_eq!(next_stream_id(&mut top), 1);
    }
}
