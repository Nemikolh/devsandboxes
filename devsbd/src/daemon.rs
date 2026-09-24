//! `devsbd daemon`: owns the in-container listeners. ssh clients connect to
//! the agent socket; each connection becomes a stream routed to the most
//! recent live bridge (`devsbd bridge`, attached over the control socket),
//! which relays it to the host.
//!
//! One daemon per container, per build: the pidfile records the owner's build
//! hash, and a daemon of a different build (the host rewrote the binary, e.g.
//! `start` on a running container after a CLI upgrade) makes the running one
//! exit and takes over.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::mux::{self, Conn, Mux};
use crate::proto::{self, caps, channel, Frame};

pub const DIR: &str = "/run/devsandbox";
pub const CTL: &str = "/run/devsandbox/devsbd.ctl";
const PIDFILE: &str = "/run/devsandbox/devsbd.pid";
/// Same path the bind-mount design uses, so `SSH_AUTH_SOCK` is identical in
/// both modes (docs/sandbox-helper.md).
const AGENT_SOCK: &str = "/run/devsandbox/ssh-agent.sock";

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

/// Whether a bridge may carry ssh-agent streams, from what it has told us so
/// far. `bridge_caps` is the caps in the *bridge binary's* `Hello` to us;
/// `host_caps` is its host's `Caps` frame, `None` until one arrives.
///
/// The daemon registers a bridge right after the bridge↔daemon handshake, but
/// the host's `Caps` only comes after the bridge↔host handshake (a `docker
/// exec` round trip later). So "no `Caps` yet" is ambiguous, and the bridge's
/// own `Hello` caps settle it:
/// - `bridge_caps == 0`: a pre-forwarding bridge, whose host never sends `Caps`
///   → capable (the old behavior).
/// - `bridge_caps != 0`, no `Caps` yet: a current host whose `Caps` is in
///   flight → *not* capable yet. Counting it as capable would route agent
///   clients to a just-spawned `devsandbox port` bridge that refuses them,
///   breaking ssh in the container whenever a forward (re)connects.
/// - `Caps` received → capable iff it has `SSH_AGENT`.
///
/// Accepted gap: an older CLI exec'ing a newer helper sends no `Caps` behind a
/// capable bridge, so that bridge is never used for agent routing (the daemon
/// falls back to others). Mixed CLI builds already evict each other on
/// takeover (docs/sandbox-helper.md), so it's rare.
fn agent_capable(bridge_caps: u32, host_caps: Option<u32>) -> bool {
    match host_caps {
        Some(c) => c & caps::SSH_AGENT != 0,
        None => bridge_caps == 0,
    }
}

/// Index of the newest agent-capable bridge in a per-bridge list of
/// `(bridge_caps, host_caps)` (oldest first), or `None` when none qualify.
/// Pure so the routing rule is testable without a daemon.
fn agent_route(bridges: &[(u32, Option<u32>)]) -> Option<usize> {
    bridges.iter().rposition(|&(b, h)| agent_capable(b, h))
}

/// One attached bridge: its mux and the caps its binary sent in `Hello` (see
/// `agent_capable`).
struct Attached {
    mux: Arc<Mux>,
    hello_caps: u32,
}

/// Connected bridges, oldest first. Agent routing goes to the newest
/// agent-capable one; keeping the rest means a short-lived bridge (a CLI
/// `exec`) ending falls back to a long-lived one (the TUI's) instead of leaving
/// no route.
#[derive(Default)]
struct Bridges {
    list: Mutex<Vec<Attached>>,
    /// Signalled when a bridge attaches or its host's `Caps` arrives, waking
    /// held agent clients (a current bridge becomes agent-capable only once
    /// `Caps` arrives, after it attaches). Notifiers take `list` first so a
    /// wake-up can't slip between `route`'s predicate check and its wait.
    arrived: Condvar,
}

impl Bridges {
    /// The newest agent-capable bridge, waiting up to `HOLD` for one to appear.
    /// Waits while no attached bridge is agent-capable, not merely while the
    /// list is empty, so a lone non-agent (or pending) bridge doesn't cut the
    /// hold short.
    fn route(&self) -> Option<Arc<Mux>> {
        let list = self.list.lock().unwrap();
        let (list, _) = self
            .arrived
            .wait_timeout_while(list, HOLD, |l| agent_route(&caps_of(l)).is_none())
            .unwrap();
        agent_route(&caps_of(&list)).map(|i| Arc::clone(&list[i].mux))
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
    }

    /// Wake held agent clients. Takes `list` (briefly) before notifying: the
    /// state change (a push, or the mux's recorded `Caps`) is already visible,
    /// so a `route` either sees it in its predicate or is already waiting.
    fn wake(&self) {
        drop(self.list.lock().unwrap());
        self.arrived.notify_all();
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

    let bridges = Arc::new(Bridges::default());
    let b = Arc::clone(&bridges);
    let hash = hash.to_string();
    std::thread::spawn(move || accept_bridges(ctl, b, &hash));
    match agent {
        Some(agent) => accept_agent_clients(agent, bridges),
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
    // `Caps`, so until then the bridge isn't agent-capable (`agent_capable`).
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
/// `connect_timeout` each address in order. `Ok` disables Nagle (the mux
/// already chunks). `Err` is a short human reason the host surfaces
/// (`connection refused (127.0.0.1:3000)`, `cannot resolve postgres`,
/// `timed out`).
fn dial(host: &str, port: u16) -> Result<Conn, String> {
    let addrs = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect::<Vec<_>>(),
        Err(_) => return Err(format!("cannot resolve {host}")),
    };
    if addrs.is_empty() {
        return Err(format!("cannot resolve {host}"));
    }
    let mut last = String::new();
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, DIAL_TIMEOUT) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream.into());
            }
            Err(e) => last = dial_error(&addr.to_string(), &e),
        }
    }
    Err(last)
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

fn accept_agent_clients(agent: UnixListener, bridges: Arc<Bridges>) {
    // Daemon-allocated ids start at 1; 0 is reserved for control frames and the
    // high bit for host-allocated ids, so ids wrap within 1..2^31 (never 0,
    // high bit never set — see the host's `Open` policy in mux.rs).
    let next = Arc::new(AtomicU32::new(1));
    for conn in agent.incoming() {
        let Ok(conn) = conn else { continue };
        let bridges = Arc::clone(&bridges);
        let next = Arc::clone(&next);
        // A thread per client: `route` may hold it until a bridge attaches.
        std::thread::spawn(move || {
            // No bridge even after HOLD: dropping `conn` closes it, and ssh
            // sees a refusing agent.
            let Some(mux) = bridges.route() else { return };
            let stream = next_stream_id(&next);
            let _ = mux.attach(stream, conn, Some(channel::SSH_AGENT));
        });
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
        assert!(agent_capable(0, None));
        // Current bridge (Hello caps != 0), `Caps` still in flight: pending,
        // not capable — the window where a new `devsandbox port` bridge would
        // otherwise steal agent streams.
        assert!(!agent_capable(TF, None));
        // `Caps` received: capable iff SSH_AGENT, whatever the Hello said.
        for bridge in [0, TF] {
            assert!(agent_capable(bridge, Some(caps::SSH_AGENT)));
            assert!(agent_capable(bridge, Some(caps::SSH_AGENT | TF)));
            assert!(!agent_capable(bridge, Some(0)));
            assert!(!agent_capable(bridge, Some(TF)));
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
        assert!(Arc::ptr_eq(&bridges.route().unwrap(), &old));
        assert!(t.elapsed() < HOLD / 2, "no hold when a capable bridge exists");

        // Only a pending bridge: a held client is woken by its `Caps(SSH_AGENT)`.
        let bridges = Arc::new(Bridges::default());
        let (pending, mut w) = pending_bridge(&bridges);
        let b = Arc::clone(&bridges);
        let held = std::thread::spawn(move || (b.route(), std::time::Instant::now()));
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
        assert!(bridges.route().is_none());
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
        assert_eq!(agent_route(&[]), None);
        assert_eq!(agent_route(&[(TF, Some(TF)), (TF, Some(0)), (TF, None)]), None);
        // Newest capable wins over an older capable one.
        assert_eq!(agent_route(&[(0, None), (0, None)]), Some(1));
        assert_eq!(agent_route(&[(TF, AG), (TF, AG)]), Some(1));
        // A newer non-agent bridge is skipped for the older agent bridge — the
        // fix for a `devsandbox port` stealing ssh routing.
        assert_eq!(agent_route(&[(0, None), (TF, Some(0))]), Some(0));
        assert_eq!(agent_route(&[(TF, AG), (TF, Some(0)), (TF, Some(TF))]), Some(0));
        // A newer bridge whose `Caps` is still in flight is skipped too.
        assert_eq!(agent_route(&[(TF, AG), (TF, None)]), Some(0));
        assert_eq!(agent_route(&[(0, None), (TF, None)]), Some(0));
        // The newest capable among a mix.
        assert_eq!(agent_route(&[(TF, AG), (TF, Some(0)), (0, None), (TF, None)]), Some(2));
    }
}
