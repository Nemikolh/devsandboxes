//! `devsbd daemon`: owns the in-container listeners. ssh clients connect to
//! the agent socket; each connection becomes a stream routed to the most
//! recent live bridge (`devsbd bridge`, attached over the control socket),
//! which relays it to the host.
//!
//! It also drains the notification outbox (`outbox.rs`): a flusher thread
//! sends each queued record to the newest bridge whose host serves `NOTIFY`,
//! woken by `devsbd notify`'s poke on the API socket, by bridges attaching or
//! their host caps arriving, and by a periodic retry.
//!
//! And it relays control requests (`devsbd ensure|ls|stop|rm`, `ctl.rs`)
//! arriving on the API socket to the newest bridge whose host serves
//! `CONTROL`, each on its own client thread, answering `NoHost` itself when
//! there's none. `events-follow` (`devsbd events --follow`) is relayed as a
//! stream instead of one reply: bytes pass through as they arrive, for as
//! long as the host keeps the stream open (`control_follow`).
//!
//! One daemon per container, per build: the pidfile records the owner's build
//! hash, and a daemon of a different build (the host rewrote the binary, e.g.
//! `start` on a running container after a CLI upgrade) makes the running one
//! exit and takes over.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::control::{self, Response, Status};
use crate::mux::{self, Conn, Mux};
use crate::notify;
use crate::outbox;
use crate::proto::{self, caps, channel, Frame};

pub const DIR: &str = "/run/devsandbox";
pub const CTL: &str = "/run/devsandbox/devsbd.ctl";
const PIDFILE: &str = "/run/devsandbox/devsbd.pid";
/// Same path the bind-mount design uses, so `SSH_AUTH_SOCK` is identical in
/// both modes (docs/sandbox-helper.md).
const AGENT_SOCK: &str = "/run/devsandbox/ssh-agent.sock";
/// Client socket for in-container tools (mode 0666, like the agent socket).
/// Each connection's first byte selects the request: [`API_POKE`] or
/// [`API_CONTROL`].
pub const API_SOCK: &str = "/run/devsandbox/api.sock";
/// "The outbox has new records": wakes the flusher. No reply.
pub const API_POKE: u8 = b'n';
/// A control request: the encoded `control::Request` follows, then the client
/// shuts its write side; the daemon answers one encoded `control::Response`
/// and closes. For `events-follow`, the host's held-open response streams
/// through until either end closes.
pub const API_CONTROL: u8 = b'c';

/// The daemon's answer when no bridge's host serves `CONTROL`.
const NO_HOST: &str = "no host connected; open the devsandbox dashboard";

/// Chunks an `events-follow` relay queues for a client that reads slowly.
/// Past that the follower is dropped (the client exits 75 and reconnects,
/// getting everything unacked again) rather than letting the host's bytes
/// back up into the mux, whose frame reader writes legacy streams inline
/// and would stall every stream on the bridge.
const FOLLOW_QUEUE: usize = 64;

/// How long an API client may take to send its first byte.
const API_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a control client may take to send the rest of its request.
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a control request may wait for the host's reply. Generous: an
/// `ensure` may build an image and create a worktree before it answers.
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Periodic outbox retry, for records a failed flush left behind (a host that
/// never replied, a bridge that died mid-flush).
const FLUSH_RETRY: Duration = Duration::from_secs(30);

/// How long the flusher waits for the host's `ok` on one record before
/// giving up on this flush.
const NOTIFY_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an agent client that arrives with no bridge attached waits for
/// one. The host starts a CLI `exec`'s bridge concurrently with the command
/// (no handshake wait), so a command using the agent at once can beat it.
const HOLD: Duration = Duration::from_secs(1);

/// How long a taking-over daemon waits for the outdated one to exit.
const TAKEOVER: Duration = Duration::from_secs(3);

/// How long the daemon waits for a `Connect` target's TCP handshake, per
/// resolved address. A forwarded dial that hangs shouldn't tie up its thread
/// (one per `Connect`) forever.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// The caps a bridge's host serves, as far as we know from what it has told
/// us so far; `None` while that's still unknown. `bridge_caps` is the caps in
/// the *bridge binary's* `Hello` to us; `host_caps` is its host's `Caps`
/// frame, `None` until one arrives.
///
/// The daemon registers a bridge right after the bridge↔daemon handshake, but
/// the host's `Caps` only comes after the bridge↔host handshake (a `docker
/// exec` round trip later). So "no `Caps` yet" is ambiguous, and the bridge's
/// own `Hello` caps settle it:
/// - `bridge_caps == 0`: a pre-forwarding bridge, whose host never sends `Caps`
///   → `SSH_AGENT` only (the old behavior: agent-capable, nothing newer).
/// - `bridge_caps != 0`, no `Caps` yet: a current host whose `Caps` is in
///   flight → unknown, *not* capable of anything yet. Counting it as
///   agent-capable would route agent clients to a just-spawned `devsandbox
///   port` bridge that refuses them, breaking ssh in the container whenever a
///   forward (re)connects.
/// - `Caps` received → exactly those.
///
/// Accepted gap: an older CLI exec'ing a newer helper sends no `Caps` behind a
/// capable bridge, so that bridge is never used for agent routing (the daemon
/// falls back to others). Mixed CLI builds already evict each other on
/// takeover (docs/sandbox-helper.md), so it's rare.
fn known_host_caps(bridge_caps: u32, host_caps: Option<u32>) -> Option<u32> {
    match host_caps {
        Some(c) => Some(c),
        None if bridge_caps == 0 => Some(caps::SSH_AGENT),
        None => None,
    }
}

/// Whether a bridge's host is known to serve `cap` streams (`known_host_caps`).
fn capable(cap: u32, bridge_caps: u32, host_caps: Option<u32>) -> bool {
    known_host_caps(bridge_caps, host_caps).is_some_and(|c| c & cap != 0)
}

/// Index of the newest bridge capable of `cap` in a per-bridge list of
/// `(bridge_caps, host_caps)` (oldest first), or `None` when none qualify.
/// Pure so the routing rule is testable without a daemon.
fn route_index(cap: u32, bridges: &[(u32, Option<u32>)]) -> Option<usize> {
    bridges.iter().rposition(|&(b, h)| capable(cap, b, h))
}

/// A wake-up flag for the outbox flusher: `fire` any number of times, the
/// next `wait` consumes them all. Firing while a flush runs makes the flusher
/// go around once more, so no trigger is lost.
#[derive(Default)]
struct Trigger {
    fired: Mutex<bool>,
    cv: Condvar,
}

impl Trigger {
    fn fire(&self) {
        *self.fired.lock().unwrap() = true;
        self.cv.notify_all();
    }

    /// Wait until fired or `timeout` passes; `true` when fired.
    fn wait(&self, timeout: Duration) -> bool {
        let fired = self.fired.lock().unwrap();
        let (mut fired, _) = self.cv.wait_timeout_while(fired, timeout, |f| !*f).unwrap();
        std::mem::replace(&mut *fired, false)
    }
}

/// One attached bridge: its mux and the caps its binary sent in `Hello` (see
/// `known_host_caps`).
struct Attached {
    mux: Arc<Mux>,
    hello_caps: u32,
}

/// Connected bridges, oldest first. A stream on a channel is routed to the
/// newest bridge whose host serves it; keeping the rest means a short-lived
/// bridge (a CLI `exec`) ending falls back to a long-lived one (the TUI's)
/// instead of leaving no route.
#[derive(Default)]
struct Bridges {
    list: Mutex<Vec<Attached>>,
    /// Signalled when a bridge attaches or its host's `Caps` arrives, waking
    /// held agent clients (a current bridge becomes agent-capable only once
    /// `Caps` arrives, after it attaches). Notifiers take `list` first so a
    /// wake-up can't slip between `route`'s predicate check and its wait.
    arrived: Condvar,
    /// Fired on the same events as `arrived` (and by pokes on `API_SOCK`): a
    /// bridge that can take notifications may have appeared.
    flush: Trigger,
}

impl Bridges {
    /// The newest bridge capable of `cap`, waiting up to `hold` for one to
    /// appear. Waits while no attached bridge is capable, not merely while the
    /// list is empty, so a lone incapable (or pending) bridge doesn't cut the
    /// hold short.
    fn route(&self, cap: u32, hold: Duration) -> Option<Arc<Mux>> {
        let list = self.list.lock().unwrap();
        let (list, _) = self
            .arrived
            .wait_timeout_while(list, hold, |l| route_index(cap, &caps_of(l)).is_none())
            .unwrap();
        route_index(cap, &caps_of(&list)).map(|i| Arc::clone(&list[i].mux))
    }

    /// Register a bridge that just finished its handshake with us. Call before
    /// `serve_with` starts reading, so the `on_caps` wake-up is in place before
    /// any `Caps` can arrive: a current bridge becomes agent-capable only once
    /// its host's `Caps` lands, and held clients must hear about it.
    fn attach(this: &Arc<Bridges>, mux: &Arc<Mux>, hello_caps: u32) {
        let b = Arc::clone(this);
        mux.on_caps(move |_| b.wake());
        this.list.lock().unwrap().push(Attached { mux: Arc::clone(mux), hello_caps });
        this.arrived.notify_all();
        this.flush.fire();
    }

    /// Wake held agent clients and the flusher. Takes `list` (briefly) before
    /// notifying: the state change (a push, or the mux's recorded `Caps`) is
    /// already visible, so a `route` either sees it in its predicate or is
    /// already waiting.
    fn wake(&self) {
        drop(self.list.lock().unwrap());
        self.arrived.notify_all();
        self.flush.fire();
    }
}

/// The per-bridge caps snapshot for the routing rule.
fn caps_of(list: &[Attached]) -> Vec<(u32, Option<u32>)> {
    list.iter().map(|a| (a.hello_caps, a.mux.peer_caps())).collect()
}

pub fn run(hash: &str) -> io::Result<()> {
    fs::create_dir_all(DIR)?;
    let Some(_lock) = take_over(hash)? else {
        return Ok(()); // a daemon of this build is alive
    };
    let ctl = listen(CTL, 0o600)?;
    // Fails when the host agent is bind-mounted at this path (mount mode);
    // the control socket still serves, there are just no agent streams.
    let agent = match listen(AGENT_SOCK, 0o666) {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("devsbd: agent socket {AGENT_SOCK}: {e}");
            None
        }
    };

    let api = match listen(API_SOCK, 0o666) {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("devsbd: api socket {API_SOCK}: {e}");
            None
        }
    };
    // Normally made by the installer; the daemon runs as root, so it can
    // repair a missing one. Best-effort: `notify` reports its own failure.
    let _ = outbox::ensure_dir(Path::new(notify::OUTBOX));

    let bridges = Arc::new(Bridges::default());
    // Daemon-allocated ids start at 1; 0 is reserved for control frames and the
    // high bit for host-allocated ids, so ids wrap within 1..2^31 (never 0,
    // high bit never set — see the host's `Open` policy in mux.rs). Shared by
    // agent streams and notify streams.
    let ids = Arc::new(AtomicU32::new(1));
    let b = Arc::clone(&bridges);
    let hash = hash.to_string();
    std::thread::spawn(move || accept_bridges(ctl, b, &hash));
    let (b, i) = (Arc::clone(&bridges), Arc::clone(&ids));
    std::thread::spawn(move || flush_loop(Path::new(notify::OUTBOX), &b, &i));
    if let Some(api) = api {
        let (b, i) = (Arc::clone(&bridges), Arc::clone(&ids));
        std::thread::spawn(move || accept_api_clients(api, b, i));
    }
    match agent {
        Some(agent) => accept_agent_clients(agent, bridges, ids),
        None => loop {
            std::thread::park();
        },
    }
    Ok(())
}

/// Take the pidfile lock, held for the daemon's lifetime (the kernel drops it
/// with the process, so a stale pidfile never blocks). `None` when a daemon
/// of this build holds it. When the holder is another build, ask it to `Quit`
/// over the control socket and wait for the lock.
fn take_over(hash: &str) -> io::Result<Option<File>> {
    let mut f = OpenOptions::new().create(true).truncate(false).write(true).open(PIDFILE)?;
    if !try_lock(&f)? {
        let owner = fs::read_to_string(PIDFILE).unwrap_or_default();
        // No hash yet: its owner is mid-startup. Only evict a daemon proven to
        // be another build, never one of ours racing us.
        if owner.split_whitespace().nth(1).is_none_or(|h| h == hash) {
            return Ok(None);
        }
        if let Ok(mut ctl) = UnixStream::connect(CTL) {
            let _ = proto::write_frame(&mut ctl, &Frame::Quit);
        }
        let step = Duration::from_millis(50);
        let mut waited = Duration::ZERO;
        while !try_lock(&f)? {
            if waited >= TAKEOVER {
                return Err(io::Error::other(format!("outdated daemon ({}) did not exit", owner.trim())));
            }
            std::thread::sleep(step);
            waited += step;
        }
    }
    f.set_len(0)?;
    writeln!(f, "{} {hash}", std::process::id())?;
    Ok(Some(f))
}

fn try_lock(f: &File) -> io::Result<bool> {
    match f.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

fn listen(path: &str, mode: u32) -> io::Result<UnixListener> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let l = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(l)
}

fn accept_bridges(ctl: UnixListener, bridges: Arc<Bridges>, hash: &str) {
    for conn in ctl.incoming() {
        let Ok(conn) = conn else { continue };
        let bridges = Arc::clone(&bridges);
        let hash = hash.to_string();
        std::thread::spawn(move || serve_bridge(conn, bridges, &hash));
    }
}

/// One control connection. Its first frame is `Quit` (a daemon of another
/// build taking over: exit) or a bridge's `Hello`, answered with ours even on
/// a version mismatch so the bridge can report it. A matching bridge then
/// becomes the newest route until it disconnects.
fn serve_bridge(mut conn: UnixStream, bridges: Arc<Bridges>, hash: &str) {
    // The bridge binary's own `Hello` caps: nonzero means its host will send
    // `Caps`, so until then the bridge isn't capable of anything
    // (`known_host_caps`).
    let hello_caps = match proto::read_frame(&mut conn) {
        Ok(Some(Frame::Quit)) => std::process::exit(0),
        Ok(Some(Frame::Hello { version, caps: bridge_caps, .. })) => {
            // Advertise that we serve `Connect` streams, so the bridge can pass
            // the host `own & daemon` caps and a forward is only attempted when
            // this daemon can serve it.
            let ours = Frame::Hello { version: proto::VERSION, hash: hash.to_string(), caps: caps::TCP_FORWARD };
            if proto::write_frame(&mut conn, &ours).is_err() || version != proto::VERSION {
                return;
            }
            bridge_caps
        }
        _ => return,
    };
    let Ok(out) = conn.try_clone() else { return };
    let mux = Mux::new(out);
    Bridges::attach(&bridges, &mux, hello_caps);
    // Keepalive: a wedged bridge (stopped reading, so it also stops sending)
    // is caught by inbound silence; `on_dead` shuts the control connection,
    // which unblocks the `serve` read below so it returns and the bridge
    // leaves the routing list. Without this a wedged newest bridge would stall
    // every agent request routed to it.
    if let Ok(dead) = conn.try_clone() {
        mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
            let _ = dead.shutdown(std::net::Shutdown::Both);
        });
    }
    // The daemon never accepts `Open`; it dials each `Connect` on its own
    // thread so the frame reader never blocks on a slow TCP handshake.
    mux.serve_with(conn, |_, _| None, |_, host, port, reply| {
        std::thread::spawn(move || reply.finish(dial(&host, port)));
    });
    bridges.list.lock().unwrap().retain(|a| !Arc::ptr_eq(&a.mux, &mux));
}

/// Dial `host:port` inside the container for a forwarded stream: resolve, then
/// `connect_timeout` each address in order, then any [`loopback_fallback`].
/// `Ok` disables Nagle (the mux already chunks). `Err` is a short human reason
/// the host surfaces (`connection refused (127.0.0.1:3000)`, `cannot resolve
/// postgres`, `timed out`); it names the requested address, not the fallback.
fn dial(host: &str, port: u16) -> Result<Conn, String> {
    let addrs = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect::<Vec<_>>(),
        Err(_) => return Err(format!("cannot resolve {host}")),
    };
    if addrs.is_empty() {
        return Err(format!("cannot resolve {host}"));
    }
    let fallback = loopback_fallback(&addrs);
    let mut last = String::new();
    for (i, addr) in addrs.iter().chain(&fallback).enumerate() {
        match TcpStream::connect_timeout(addr, DIAL_TIMEOUT) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream.into());
            }
            Err(e) if i < addrs.len() => last = dial_error(&addr.to_string(), &e),
            Err(_) => {}
        }
    }
    Err(last)
}

/// When `addrs` are all loopback, the other family's loopback on the same port.
/// Dev servers told `localhost` often bind only `::1` (Node 17+ resolves it
/// IPv6-first) while forwards dial `127.0.0.1`, or the reverse; "a port on
/// this container's loopback" should reach either. Non-loopback targets (a
/// service alias) get nothing: their address is the one that was asked for.
fn loopback_fallback(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    if addrs.is_empty() || !addrs.iter().all(|a| a.ip().is_loopback()) {
        return Vec::new();
    }
    let port = addrs[0].port();
    let mut out = Vec::new();
    if !addrs.iter().any(SocketAddr::is_ipv4) {
        out.push(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    }
    if !addrs.iter().any(SocketAddr::is_ipv6) {
        out.push(SocketAddr::from((Ipv6Addr::LOCALHOST, port)));
    }
    out
}

/// A one-line reason for a failed dial, naming the address so the host can tell
/// which candidate failed. `TimedOut` (from `connect_timeout`) and the common
/// `ConnectionRefused` get terse phrasings; anything else falls back to the OS
/// message.
fn dial_error(addr: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::TimedOut => format!("timed out ({addr})"),
        io::ErrorKind::ConnectionRefused => format!("connection refused ({addr})"),
        _ => format!("{e} ({addr})"),
    }
}

fn accept_agent_clients(agent: UnixListener, bridges: Arc<Bridges>, next: Arc<AtomicU32>) {
    for conn in agent.incoming() {
        let Ok(conn) = conn else { continue };
        let bridges = Arc::clone(&bridges);
        let next = Arc::clone(&next);
        // A thread per client: `route` may hold it until a bridge attaches.
        std::thread::spawn(move || {
            // No bridge even after HOLD: dropping `conn` closes it, and ssh
            // sees a refusing agent.
            let Some(mux) = bridges.route(caps::SSH_AGENT, HOLD) else { return };
            let stream = next_stream_id(&next);
            let _ = mux.attach(stream, conn, Some(channel::SSH_AGENT));
        });
    }
}

/// Serve `API_SOCK`: read each client's first byte and dispatch on it. A
/// thread per client so a silent one can't stall the others (`API_READ_TIMEOUT`
/// bounds it anyway), and so a control request can take its time without
/// holding up pokes or the flusher.
fn accept_api_clients(api: UnixListener, bridges: Arc<Bridges>, ids: Arc<AtomicU32>) {
    for conn in api.incoming() {
        let Ok(mut conn) = conn else { continue };
        let bridges = Arc::clone(&bridges);
        let ids = Arc::clone(&ids);
        std::thread::spawn(move || {
            let _ = conn.set_read_timeout(Some(API_READ_TIMEOUT));
            let mut verb = [0u8; 1];
            if conn.read_exact(&mut verb).is_err() {
                return;
            }
            match verb[0] {
                API_POKE => bridges.flush.fire(),
                API_CONTROL => serve_control_client(conn, &bridges, &ids),
                // Unknown verb: dropping `conn` closes it.
                _ => {}
            }
        });
    }
}

/// One control client, after its verb byte: read the request to EOF, relay
/// it (`control_exchange`), write the encoded response back.
fn serve_control_client(mut conn: UnixStream, bridges: &Bridges, ids: &AtomicU32) {
    let _ = conn.set_read_timeout(Some(CONTROL_REQUEST_TIMEOUT));
    let mut request = Vec::new();
    let reply = match (&conn).take(control::MAX_REQUEST as u64 + 1).read_to_end(&mut request) {
        Err(e) => encoded(Status::Usage, format!("reading request: {e}")),
        Ok(_) if request.len() > control::MAX_REQUEST => {
            encoded(Status::Usage, format!("request longer than {} bytes", control::MAX_REQUEST))
        }
        Ok(_) if is_follow(&request) => return control_follow(bridges, ids, &request, conn),
        Ok(_) => control_exchange(bridges, ids, &request, CONTROL_REPLY_TIMEOUT),
    };
    let _ = conn.write_all(&reply);
}

/// Whether an encoded request is `events-follow`. Anything that doesn't
/// decode takes the one-reply path, and the host answers its `Usage`.
fn is_follow(request: &[u8]) -> bool {
    std::str::from_utf8(request)
        .ok()
        .and_then(|t| control::decode_request(t).ok())
        .is_some_and(|r| r.op == control::Op::Subscribe)
}

/// Relay an `events-follow` request like [`control_exchange`], but stream the
/// host's response to `conn` as it arrives, with no reply timeout or size
/// cap: the host's ping lines keep it alive, and [`control::FOLLOW_SILENCE`]
/// without a byte ends it. Without a route the answer is `NoHost`; any other
/// end (the bridge died, the host closed, the client left) just closes both
/// sides, and the client tells a finished stream from a cut one itself.
fn control_follow(bridges: &Bridges, ids: &AtomicU32, request: &[u8], conn: UnixStream) {
    let Some(mux) = bridges.route(caps::CONTROL, Duration::ZERO) else {
        let _ = (&conn).write_all(&encoded(Status::NoHost, NO_HOST));
        return;
    };
    let stream = next_stream_id(ids);
    let Ok((ours, theirs)) = UnixStream::pair() else { return };
    let sent = mux.attach(stream, theirs, Some(channel::CONTROL)).and_then(|()| {
        mux.send(&Frame::Data { stream, bytes: request.to_vec() })?;
        mux.send(&Frame::Eof { stream })
    });
    if sent.is_err() {
        return;
    }
    let _ = ours.set_read_timeout(Some(control::FOLLOW_SILENCE));
    let _ = conn.set_write_timeout(Some(control::FOLLOW_SILENCE));
    relay(ours, conn);
}

/// Copy `from` (the mux stream) to `to` (the client) until either ends. The
/// reading side never blocks on the client: chunks go through a queue of
/// [`FOLLOW_QUEUE`] drained by a writer thread, and a full queue ends the
/// relay. Whichever side ends first, both close: shutting `from` sends the
/// host a `Close`, so its next write fails; dropping `to` is the client's EOF.
fn relay(mut from: UnixStream, mut to: UnixStream) {
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(FOLLOW_QUEUE);
    let wake = from.try_clone().ok();
    let writer = std::thread::spawn(move || {
        for chunk in rx {
            if to.write_all(&chunk).is_err() {
                break;
            }
        }
        // The client left: unblock the reader so the stream closes now.
        if let Some(from) = wake {
            let _ = from.shutdown(std::net::Shutdown::Both);
        }
    });
    let mut buf = [0u8; 4096];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            // Full (a stalled client) or disconnected (the writer quit).
            Ok(n) => {
                if tx.try_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = from.shutdown(std::net::Shutdown::Both);
    drop(tx);
    let _ = writer.join();
}

fn encoded(status: Status, body: impl Into<String>) -> Vec<u8> {
    control::encode_response(&Response::new(status, body)).into_bytes()
}

/// Relay one encoded control request to the newest bridge whose host serves
/// `CONTROL` and return the host's encoded response verbatim. The daemon
/// answers itself when it can't: `NoHost` with no capable bridge (at once, no
/// hold: the script retries), `Failed` when the host doesn't reply within
/// `timeout` or its bridge dies (the mux closes every stream then, so the
/// reply ends short). Same stream shape as `send_record`: `Open(CONTROL)`,
/// the request as one `Data`, `Eof`, the reply read through a socket pair.
fn control_exchange(bridges: &Bridges, ids: &AtomicU32, request: &[u8], timeout: Duration) -> Vec<u8> {
    let Some(mux) = bridges.route(caps::CONTROL, Duration::ZERO) else {
        return encoded(Status::NoHost, NO_HOST);
    };
    let disconnected = || encoded(Status::Failed, "host disconnected");
    let stream = next_stream_id(ids);
    let Ok((mut ours, theirs)) = UnixStream::pair() else { return disconnected() };
    let sent = mux.attach(stream, theirs, Some(channel::CONTROL)).and_then(|()| {
        mux.send(&Frame::Data { stream, bytes: request.to_vec() })?;
        mux.send(&Frame::Eof { stream })
    });
    if sent.is_err() {
        return disconnected();
    }
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || ours.set_read_timeout(Some(left)).is_err() {
            return encoded(Status::Failed, "timed out");
        }
        match ours.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return encoded(Status::Failed, "timed out");
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return disconnected(),
        }
        if reply.len() > control::MAX_RESPONSE {
            return encoded(Status::Failed, "host response too large");
        }
    }
    // A reply cut short by a dying bridge doesn't decode; neither does none.
    let complete = std::str::from_utf8(&reply).is_ok_and(|t| control::decode_response(t).is_ok());
    if complete { reply } else { disconnected() }
}

/// The flusher thread: flush now (records queued while no daemon ran), then
/// again on every trigger (`Bridges::flush`) or after `FLUSH_RETRY`.
fn flush_loop(dir: &Path, bridges: &Bridges, ids: &AtomicU32) {
    loop {
        flush(dir, bridges, ids, NOTIFY_REPLY_TIMEOUT);
        bridges.flush.wait(FLUSH_RETRY);
    }
}

/// Send queued records oldest first, each on its own `Open(NOTIFY)` stream to
/// the newest bridge whose host serves notify, deleting a file only once its
/// host replied `ok`. Stops at the first failure (no capable bridge, no
/// reply, I/O error), leaving the rest for the next trigger or retry, so order
/// is kept. A file with bad content is moved aside instead, so it can't block
/// the queue forever. Returns how many records were delivered.
fn flush(dir: &Path, bridges: &Bridges, ids: &AtomicU32, timeout: Duration) -> usize {
    let Ok(queue) = outbox::pending(dir) else { return 0 };
    let mut sent = 0;
    for path in queue {
        // Routed per record: the bridge used for the last one may be gone.
        let Some(mux) = bridges.route(caps::NOTIFY, Duration::ZERO) else { break };
        let record = match outbox::load(&path) {
            Ok(Ok(record)) => record,
            Ok(Err(_)) => {
                let _ = outbox::move_aside(&path);
                continue;
            }
            // Gone since `pending` listed it: `devsbd thread put` coalesces its
            // key's pending files (outbox::enqueue_thread) and queues a newer
            // one, so this is routine, not a reason to stall the queue.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => break,
        };
        if send_record(&mux, next_stream_id(ids), &record, timeout).is_err() {
            break;
        }
        // Delivered but undeletable would resend it forever; stop instead.
        // Already gone (coalesced mid-send) is fine: it won't be resent.
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => break,
        }
        sent += 1;
    }
    sent
}

/// One notify exchange on `stream`: `Open(NOTIFY)`, the record as one `Data`,
/// `Eof`, then wait up to `timeout` for the host's `notify::REPLY_OK`. The
/// reply comes back through a socket pair attached like an agent client's;
/// the request goes out with `Mux::send` rather than through that pair, since
/// a legacy stream turns local EOF into `Close` (which would drop the reply),
/// while a peer `Eof` half-closes it (mux.rs module doc). Dropping our end on
/// return closes the stream if the host hasn't already.
fn send_record(mux: &Arc<Mux>, stream: u32, record: &[u8], timeout: Duration) -> io::Result<()> {
    let (mut ours, theirs) = UnixStream::pair()?;
    mux.attach(stream, theirs, Some(channel::NOTIFY))?;
    mux.send(&Frame::Data { stream, bytes: record.to_vec() })?;
    mux.send(&Frame::Eof { stream })?;
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    let mut buf = [0u8; 16];
    while reply.len() < notify::REPLY_OK.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        ours.set_read_timeout(Some(left))?;
        match ours.read(&mut buf)? {
            0 => return Err(io::Error::other("notify stream closed without a reply")),
            n => reply.extend_from_slice(&buf[..n]),
        }
    }
    if reply.starts_with(notify::REPLY_OK) {
        Ok(())
    } else {
        Err(io::Error::other("unexpected notify reply"))
    }
}

/// Next daemon-allocated stream id in 1..2^31, wrapping back to 1. A CAS loop
/// keeps two concurrent accepts from landing on the same id.
fn next_stream_id(next: &AtomicU32) -> u32 {
    let mut cur = next.load(Ordering::Relaxed);
    loop {
        let step = if cur >= (1 << 31) - 1 { 1 } else { cur + 1 };
        match next.compare_exchange_weak(cur, step, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return cur,
            Err(actual) => cur = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ids_stay_in_lower_half_and_wrap() {
        let next = AtomicU32::new(1);
        assert_eq!(next_stream_id(&next), 1);
        assert_eq!(next_stream_id(&next), 2);
        // At the top of the space it wraps back to 1, never hitting 0 or the
        // host's high-bit space.
        let top = AtomicU32::new((1 << 31) - 1);
        assert_eq!(next_stream_id(&top), (1 << 31) - 1);
        assert_eq!(next_stream_id(&top), 1);
        assert_eq!(top.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn agent_capability_by_bridge_hello_and_host_caps() {
        const TF: u32 = caps::TCP_FORWARD;
        // Pre-forwarding bridge (Hello caps 0), no `Caps`: its host never sends
        // one, so it's capable (the old behavior).
        assert!(capable(caps::SSH_AGENT, 0, None));
        // Current bridge (Hello caps != 0), `Caps` still in flight: pending,
        // not capable — the window where a new `devsandbox port` bridge would
        // otherwise steal agent streams.
        assert!(!capable(caps::SSH_AGENT, TF, None));
        // `Caps` received: capable iff SSH_AGENT, whatever the Hello said.
        for bridge in [0, TF] {
            assert!(capable(caps::SSH_AGENT, bridge, Some(caps::SSH_AGENT)));
            assert!(capable(caps::SSH_AGENT, bridge, Some(caps::SSH_AGENT | TF)));
            assert!(!capable(caps::SSH_AGENT, bridge, Some(0)));
            assert!(!capable(caps::SSH_AGENT, bridge, Some(TF)));
        }
    }

    /// A bridge attached with nonzero Hello caps, fed frames through a pipe by
    /// the test (standing in for its host). Returns the mux and the writer.
    fn pending_bridge(bridges: &Arc<Bridges>) -> (Arc<Mux>, std::io::PipeWriter) {
        let (r, w) = std::io::pipe().unwrap();
        let mux = Mux::new(io::sink());
        Bridges::attach(bridges, &mux, caps::TCP_FORWARD);
        let m = Arc::clone(&mux);
        std::thread::spawn(move || m.serve_with(r, |_, _| None, |_, _, _, _reply| {}));
        (mux, w)
    }

    /// The window the Hello caps close: a current bridge registered but whose
    /// host's `Caps` hasn't arrived is not routed to; its `Caps` then wakes a
    /// held client (no lost wake-up), or leaves it unrouted when it lacks
    /// `SSH_AGENT`.
    #[test]
    fn pending_bridge_is_skipped_until_its_caps_arrive() {
        // Older pre-forwarding bridge + newer pending one: the older wins, at once.
        let bridges = Arc::new(Bridges::default());
        let old = Mux::new(io::sink());
        Bridges::attach(&bridges, &old, 0);
        let (_pending, _w) = pending_bridge(&bridges);
        let t = std::time::Instant::now();
        assert!(Arc::ptr_eq(&bridges.route(caps::SSH_AGENT, HOLD).unwrap(), &old));
        assert!(t.elapsed() < HOLD / 2, "no hold when a capable bridge exists");

        // Only a pending bridge: a held client is woken by its `Caps(SSH_AGENT)`.
        let bridges = Arc::new(Bridges::default());
        let (pending, mut w) = pending_bridge(&bridges);
        let b = Arc::clone(&bridges);
        let held = std::thread::spawn(move || (b.route(caps::SSH_AGENT, HOLD), std::time::Instant::now()));
        std::thread::sleep(Duration::from_millis(100));
        let sent = std::time::Instant::now();
        proto::write_frame(&mut w, &Frame::Caps(caps::SSH_AGENT)).unwrap();
        let (routed, at) = held.join().unwrap();
        assert!(Arc::ptr_eq(&routed.expect("woken by Caps"), &pending));
        assert!(at.duration_since(sent) < HOLD / 2, "woken by Caps, not by the HOLD timeout");

        // Only a pending bridge whose host has no agent: never routed.
        let bridges = Arc::new(Bridges::default());
        let (_pending, mut w) = pending_bridge(&bridges);
        proto::write_frame(&mut w, &Frame::Caps(0)).unwrap();
        assert!(bridges.route(caps::SSH_AGENT, HOLD).is_none());
    }

    #[test]
    fn loopback_fallback_adds_the_other_family_only_for_loopback() {
        let v4: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        let v6: SocketAddr = "[::1]:3000".parse().unwrap();
        assert_eq!(loopback_fallback(&[v4]), vec![v6]);
        assert_eq!(loopback_fallback(&[v6]), vec![v4]);
        // Both families already there (`localhost` via /etc/hosts): nothing.
        assert!(loopback_fallback(&[v6, v4]).is_empty());
        // Any non-loopback address (a service alias): leave the dial alone.
        let svc: SocketAddr = "172.18.0.3:5432".parse().unwrap();
        assert!(loopback_fallback(&[svc]).is_empty());
        assert!(loopback_fallback(&[svc, v4]).is_empty());
        assert!(loopback_fallback(&[]).is_empty());
    }

    /// The case that motivated the fallback: a server bound only on `::1` is
    /// reached by the forwarder's `127.0.0.1` dial.
    #[test]
    fn dial_v4_loopback_reaches_a_v6_only_listener() {
        let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(dial("127.0.0.1", port).is_ok());
        drop(listener);
        // Nothing listening on either family: the note names the requested
        // address, not the fallback. A freed ephemeral port can be taken at
        // once by another test binding in parallel, so a dial that connects
        // means "port reused", not a failure: retry on a fresh one.
        for _ in 0..20 {
            let port = std::net::TcpListener::bind("[::1]:0").unwrap().local_addr().unwrap().port();
            if let Err(err) = dial("127.0.0.1", port) {
                assert_eq!(err, format!("connection refused (127.0.0.1:{port})"));
                return;
            }
        }
        panic!("every freed port was reused before it could be dialed");
    }

    #[test]
    fn dial_error_names_the_address_with_a_terse_reason() {
        let addr = "127.0.0.1:3000";
        assert_eq!(
            dial_error(addr, &io::Error::from(io::ErrorKind::ConnectionRefused)),
            "connection refused (127.0.0.1:3000)"
        );
        assert_eq!(
            dial_error(addr, &io::Error::from(io::ErrorKind::TimedOut)),
            "timed out (127.0.0.1:3000)"
        );
        // Anything else keeps the OS message, still address-tagged.
        let other = dial_error(addr, &io::Error::new(io::ErrorKind::Other, "boom"));
        assert_eq!(other, "boom (127.0.0.1:3000)");
    }

    #[test]
    fn agent_route_picks_the_newest_capable_bridge() {
        const TF: u32 = caps::TCP_FORWARD;
        const AG: Option<u32> = Some(caps::SSH_AGENT);
        // Empty list, or one made only of non-agent / pending bridges: no route.
        assert_eq!(route_index(caps::SSH_AGENT, &[]), None);
        assert_eq!(route_index(caps::SSH_AGENT, &[(TF, Some(TF)), (TF, Some(0)), (TF, None)]), None);
        // Newest capable wins over an older capable one.
        assert_eq!(route_index(caps::SSH_AGENT, &[(0, None), (0, None)]), Some(1));
        assert_eq!(route_index(caps::SSH_AGENT, &[(TF, AG), (TF, AG)]), Some(1));
        // A newer non-agent bridge is skipped for the older agent bridge — the
        // fix for a `devsandbox port` stealing ssh routing.
        assert_eq!(route_index(caps::SSH_AGENT, &[(0, None), (TF, Some(0))]), Some(0));
        assert_eq!(route_index(caps::SSH_AGENT, &[(TF, AG), (TF, Some(0)), (TF, Some(TF))]), Some(0));
        // A newer bridge whose `Caps` is still in flight is skipped too.
        assert_eq!(route_index(caps::SSH_AGENT, &[(TF, AG), (TF, None)]), Some(0));
        assert_eq!(route_index(caps::SSH_AGENT, &[(0, None), (TF, None)]), Some(0));
        // The newest capable among a mix.
        assert_eq!(route_index(caps::SSH_AGENT, &[(TF, AG), (TF, Some(0)), (0, None), (TF, None)]), Some(2));
    }

    #[test]
    fn notify_route_needs_the_hosts_notify_cap() {
        const TF: u32 = caps::TCP_FORWARD;
        const NO: u32 = caps::NOTIFY;
        // A pre-forwarding host is agent-only: never a notify route.
        assert!(!capable(NO, 0, None));
        // Pending, or a current host without NOTIFY: not capable.
        assert!(!capable(NO, TF, None));
        assert!(!capable(NO, TF, Some(caps::SSH_AGENT)));
        assert!(capable(NO, TF, Some(NO)));
        assert!(capable(NO, 0, Some(NO | caps::SSH_AGENT)));
        // Agent and notify route independently: the newest of each.
        let list = [(TF, Some(NO)), (TF, Some(caps::SSH_AGENT)), (0, None)];
        assert_eq!(route_index(NO, &list), Some(0));
        assert_eq!(route_index(caps::SSH_AGENT, &list), Some(2));
    }

    #[test]
    fn trigger_coalesces_and_is_consumed_by_wait() {
        let t = Trigger::default();
        assert!(!t.wait(Duration::ZERO));
        t.fire();
        t.fire();
        assert!(t.wait(Duration::ZERO));
        assert!(!t.wait(Duration::ZERO), "one wait consumes every firing");
    }

    /// A bridge attached to `bridges` whose far end is a real host-side mux:
    /// it sends `Caps(host_caps)`, and each `Open(NOTIFY)` gets a handler
    /// that reads the record to EOF, reports it on the returned channel, then
    /// replies `ok` (or, with `reply: false`, holds the stream open silently).
    /// Other channels are refused.
    fn host_bridge(bridges: &Arc<Bridges>, host_caps: u32, reply: bool) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        Bridges::attach(bridges, &daemon, caps::TCP_FORWARD);
        std::thread::spawn(move || daemon.serve_with(d_r, |_, _| None, |_, _, _, _reply| {}));
        let (tx, rx) = std::sync::mpsc::channel();
        let h = Arc::clone(&host);
        std::thread::spawn(move || {
            h.serve(h_r, move |_, ch| {
                if ch != channel::NOTIFY {
                    return None;
                }
                let (ours, mut handler) = UnixStream::pair().unwrap();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut record = Vec::new();
                    handler.read_to_end(&mut record).unwrap();
                    let _ = tx.send(record);
                    if reply {
                        let _ = handler.write_all(notify::REPLY_OK);
                    } else {
                        std::thread::sleep(Duration::from_secs(5));
                    }
                });
                Some(ours.into())
            })
        });
        host.send(&Frame::Caps(host_caps)).unwrap();
        rx
    }

    fn record(at: u64, msg: &str) -> notify::Message {
        notify::Message::Notify(notify::Record {
            level: notify::Level::Info,
            key: None,
            link: None,
            msg: msg.into(),
            at,
        })
    }

    /// Wait (bounded) for the host's `Caps` to land, so `flush`'s zero-hold
    /// route sees the bridge.
    fn wait_routable(bridges: &Bridges, cap: u32) {
        assert!(bridges.route(cap, Duration::from_secs(5)).is_some(), "bridge never became capable");
    }

    #[test]
    fn flush_sends_oldest_first_and_deletes_on_ok() {
        let dir = outbox::test_dir("flush-ok");
        let b = outbox::enqueue(&dir, &record(20, "second"), 0).unwrap();
        let a = outbox::enqueue(&dir, &record(10, "first"), 0).unwrap();
        let bad = dir.join("0000000015-000000000-1");
        fs::write(&bad, "garbage").unwrap();
        let bridges = Arc::new(Bridges::default());
        let got = host_bridge(&bridges, caps::NOTIFY, true);
        wait_routable(&bridges, caps::NOTIFY);

        let ids = AtomicU32::new(1);
        assert_eq!(flush(&dir, &bridges, &ids, Duration::from_secs(5)), 2);
        let first = got.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = got.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(notify::decode(std::str::from_utf8(&first).unwrap()), Ok(record(10, "first")));
        assert_eq!(notify::decode(std::str::from_utf8(&second).unwrap()), Ok(record(20, "second")));
        assert!(!a.exists() && !b.exists(), "delivered records are deleted");
        // The unparseable one was moved aside, not sent, and didn't block the queue.
        assert!(!bad.exists());
        assert!(dir.join(format!("0000000015-000000000-1{}", outbox::BAD_SUFFIX)).is_file());
        assert!(outbox::pending(&dir).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_keeps_the_record_when_the_host_never_replies() {
        let dir = outbox::test_dir("flush-silent");
        let a = outbox::enqueue(&dir, &record(10, "first"), 0).unwrap();
        let b = outbox::enqueue(&dir, &record(20, "second"), 0).unwrap();
        let bridges = Arc::new(Bridges::default());
        let got = host_bridge(&bridges, caps::NOTIFY, false);
        wait_routable(&bridges, caps::NOTIFY);

        let t = Instant::now();
        assert_eq!(flush(&dir, &bridges, &AtomicU32::new(1), Duration::from_millis(300)), 0);
        assert!(t.elapsed() < Duration::from_secs(3), "bounded by the reply timeout");
        // The host got the first record, but without its `ok` nothing is
        // deleted, and the flush stopped there (order is kept for the retry).
        assert!(got.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(got.recv_timeout(Duration::from_millis(200)).is_err(), "no second record sent");
        assert!(a.exists() && b.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_sends_nothing_without_a_notify_capable_host() {
        let dir = outbox::test_dir("flush-nocap");
        let a = outbox::enqueue(&dir, &record(10, "first"), 0).unwrap();
        let bridges = Arc::new(Bridges::default());
        let got = host_bridge(&bridges, caps::SSH_AGENT, true);
        wait_routable(&bridges, caps::SSH_AGENT);

        assert_eq!(flush(&dir, &bridges, &AtomicU32::new(1), Duration::from_secs(5)), 0);
        assert!(got.recv_timeout(Duration::from_millis(200)).is_err(), "no stream opened");
        assert!(a.exists());
        // No bridge at all: same.
        assert_eq!(flush(&dir, &Bridges::default(), &AtomicU32::new(1), Duration::from_secs(5)), 0);
        assert!(a.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// What a test control host does with a request it read to EOF.
    #[derive(Clone, Copy)]
    enum Host {
        /// Write these bytes, then close.
        Reply(&'static [u8]),
        /// Close without a reply.
        Hangup,
        /// Hold the stream open silently.
        Silent,
        /// Write these chunks 50 ms apart, then hold the stream open.
        Stream(&'static [&'static [u8]]),
    }

    /// A bridge attached to `bridges` whose far end is a host-side mux sending
    /// `Caps(host_caps)` and serving `Open(CONTROL)` per `host`; each request
    /// is reported on the returned channel. Returns the host mux too, so a
    /// test can end the bridge (`close_all` stands in for its death).
    fn control_host(bridges: &Arc<Bridges>, host_caps: u32, host: Host) -> (std::sync::mpsc::Receiver<Vec<u8>>, Arc<Mux>) {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host_mux = Mux::new(h_w);
        Bridges::attach(bridges, &daemon, caps::TCP_FORWARD);
        std::thread::spawn(move || daemon.serve_with(d_r, |_, _| None, |_, _, _, _reply| {}));
        let (tx, rx) = std::sync::mpsc::channel();
        let h = Arc::clone(&host_mux);
        std::thread::spawn(move || {
            h.serve(h_r, move |_, ch| {
                if ch != channel::CONTROL {
                    return None;
                }
                let (ours, mut handler) = UnixStream::pair().unwrap();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut request = Vec::new();
                    handler.read_to_end(&mut request).unwrap();
                    let _ = tx.send(request);
                    match host {
                        Host::Reply(bytes) => {
                            let _ = handler.write_all(bytes);
                        }
                        Host::Hangup => {}
                        Host::Silent => std::thread::sleep(Duration::from_secs(5)),
                        Host::Stream(chunks) => {
                            for chunk in chunks {
                                if handler.write_all(chunk).is_err() {
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(50));
                            }
                            std::thread::sleep(Duration::from_secs(5));
                        }
                    }
                });
                Some(ours.into())
            })
        });
        host_mux.send(&Frame::Caps(host_caps)).unwrap();
        (rx, host_mux)
    }

    fn decoded(reply: &[u8]) -> Response {
        control::decode_response(std::str::from_utf8(reply).unwrap()).unwrap()
    }

    const REQUEST: &[u8] = b"op ensure\nsandbox web\nkey pr-1\n";

    #[test]
    fn control_without_a_capable_bridge_is_no_host_at_once() {
        let ids = AtomicU32::new(1);
        let t = Instant::now();
        let resp = decoded(&control_exchange(&Bridges::default(), &ids, REQUEST, Duration::from_secs(5)));
        assert_eq!(resp.status, Status::NoHost);
        assert!(resp.body.contains("no host connected"), "{}", resp.body);
        // A notify-only host (a sink-less or older TUI) doesn't count.
        let bridges = Arc::new(Bridges::default());
        let (got, _host) = control_host(&bridges, caps::NOTIFY | caps::SSH_AGENT, Host::Reply(b"x"));
        wait_routable(&bridges, caps::NOTIFY);
        let resp = decoded(&control_exchange(&bridges, &ids, REQUEST, Duration::from_secs(5)));
        assert_eq!(resp.status, Status::NoHost);
        assert!(t.elapsed() < Duration::from_secs(3), "no hold");
        assert!(got.recv_timeout(Duration::from_millis(200)).is_err(), "no stream opened");
    }

    #[test]
    fn control_reply_is_relayed_verbatim() {
        let bridges = Arc::new(Bridges::default());
        let reply: &[u8] = b"status ok\nbody web-pr-1\n";
        let (got, _host) = control_host(&bridges, caps::CONTROL, Host::Reply(reply));
        wait_routable(&bridges, caps::CONTROL);
        let out = control_exchange(&bridges, &AtomicU32::new(1), REQUEST, Duration::from_secs(5));
        assert_eq!(out, reply);
        assert_eq!(got.recv_timeout(Duration::from_secs(5)).unwrap(), REQUEST);
    }

    #[test]
    fn control_fails_on_hangup_garbage_timeout_and_bridge_death() {
        let ids = AtomicU32::new(1);
        let check = |host: Host, timeout: Duration, want: &str| {
            let bridges = Arc::new(Bridges::default());
            let (_got, _host) = control_host(&bridges, caps::CONTROL, host);
            wait_routable(&bridges, caps::CONTROL);
            let resp = decoded(&control_exchange(&bridges, &ids, REQUEST, timeout));
            assert_eq!((resp.status, resp.body.as_str()), (Status::Failed, want));
        };
        check(Host::Hangup, Duration::from_secs(5), "host disconnected");
        check(Host::Reply(b"status o"), Duration::from_secs(5), "host disconnected");
        let t = Instant::now();
        check(Host::Silent, Duration::from_millis(300), "timed out");
        assert!(t.elapsed() < Duration::from_secs(3));

        // The bridge dies mid-request: the daemon's mux ends every stream.
        let bridges = Arc::new(Bridges::default());
        let (got, _host) = control_host(&bridges, caps::CONTROL, Host::Silent);
        wait_routable(&bridges, caps::CONTROL);
        let mux = bridges.route(caps::CONTROL, Duration::ZERO).unwrap();
        let b = Arc::clone(&bridges);
        let pending = std::thread::spawn(move || control_exchange(&b, &AtomicU32::new(1), REQUEST, Duration::from_secs(10)));
        got.recv_timeout(Duration::from_secs(5)).unwrap();
        let t = Instant::now();
        mux.close_all();
        let resp = decoded(&pending.join().unwrap());
        assert_eq!((resp.status, resp.body.as_str()), (Status::Failed, "host disconnected"));
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    const FOLLOW: &[u8] = b"op events-follow\n";

    /// `control_follow` on its own thread, as `serve_control_client` runs it;
    /// the client end comes back.
    fn follow_client(bridges: &Arc<Bridges>) -> UnixStream {
        let (client, conn) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let b = Arc::clone(bridges);
        std::thread::spawn(move || control_follow(&b, &AtomicU32::new(1), FOLLOW, conn));
        client
    }

    fn read_some(client: &mut UnixStream) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn only_events_follow_takes_the_stream_path() {
        assert!(is_follow(FOLLOW));
        assert!(is_follow(b"op events-follow\nkey pr-1\n"));
        assert!(!is_follow(b"op events\n"));
        assert!(!is_follow(b"op nope\n"));
        assert!(!is_follow(b"\xff"));
    }

    /// A held-open response passes through as it arrives, not at EOF; the
    /// bridge dying ends the client's stream at once.
    #[test]
    fn follow_streams_through_until_the_bridge_dies() {
        let bridges = Arc::new(Bridges::default());
        let chunks: &[&[u8]] = &[b"status ok\nbody \n", b"{\"id\":\"e-1\"}\n", b"{\"kind\":\"ping\"}\n"];
        let (got, _host) = control_host(&bridges, caps::CONTROL, Host::Stream(chunks));
        wait_routable(&bridges, caps::CONTROL);
        let mut client = follow_client(&bridges);
        let mut seen = Vec::new();
        while seen.len() < chunks.concat().len() {
            seen.extend(read_some(&mut client));
        }
        assert_eq!(seen, chunks.concat(), "streamed while the host still holds the stream");
        assert_eq!(got.recv_timeout(Duration::from_secs(5)).unwrap(), FOLLOW);

        let t = Instant::now();
        bridges.route(caps::CONTROL, Duration::ZERO).unwrap().close_all();
        assert_eq!(read_some(&mut client), b"", "EOF");
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    /// The client leaving closes the host's stream, so its next write fails.
    #[test]
    fn follow_client_leaving_closes_the_host_stream() {
        let bridges = Arc::new(Bridges::default());
        let (tx, wrote) = std::sync::mpsc::channel();
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        Bridges::attach(&bridges, &daemon, caps::TCP_FORWARD);
        std::thread::spawn(move || daemon.serve_with(d_r, |_, _| None, |_, _, _, _reply| {}));
        let h = Arc::clone(&host);
        std::thread::spawn(move || {
            h.serve(h_r, move |_, _| {
                let (ours, mut handler) = UnixStream::pair().unwrap();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    handler.read_to_end(&mut Vec::new()).unwrap();
                    // Ping until a write fails.
                    loop {
                        if handler.write_all(b"{\"kind\":\"ping\"}\n").is_err() {
                            let _ = tx.send(());
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                });
                Some(ours.into())
            })
        });
        host.send(&Frame::Caps(caps::CONTROL)).unwrap();
        wait_routable(&bridges, caps::CONTROL);
        let mut client = follow_client(&bridges);
        assert!(!read_some(&mut client).is_empty());
        drop(client);
        assert!(wrote.recv_timeout(Duration::from_secs(5)).is_ok(), "the host's write failed");
    }

    #[test]
    fn follow_without_a_host_is_no_host() {
        let mut client = follow_client(&Arc::new(Bridges::default()));
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert_eq!(decoded(&reply).status, Status::NoHost);
    }

    /// End to end through the flusher thread: a record queued while no bridge
    /// exists goes out as soon as a notify-capable bridge attaches and its
    /// `Caps` arrives — well before the periodic retry.
    #[test]
    fn flusher_drains_on_bridge_attach() {
        let dir = outbox::test_dir("flush-attach");
        let a = outbox::enqueue(&dir, &record(10, "queued"), 0).unwrap();
        let bridges = Arc::new(Bridges::default());
        let (b, d) = (Arc::clone(&bridges), dir.clone());
        std::thread::spawn(move || flush_loop(&d, &b, &AtomicU32::new(1)));
        std::thread::sleep(Duration::from_millis(100));
        assert!(a.exists(), "no bridge yet");

        let got = host_bridge(&bridges, caps::NOTIFY, true);
        assert!(got.recv_timeout(Duration::from_secs(5)).is_ok());
        let deadline = Instant::now() + Duration::from_secs(5);
        while a.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!a.exists(), "delivered after attach, not at the {FLUSH_RETRY:?} retry");
        let _ = fs::remove_dir_all(&dir);
    }
}
