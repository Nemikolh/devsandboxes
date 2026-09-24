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
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::mux::{self, Mux};
use crate::proto::{self, channel, Frame};

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

/// Connected bridges, oldest first. Routing goes to the last one; keeping the
/// rest means a short-lived bridge (a CLI `exec`) ending falls back to a
/// long-lived one (the TUI's) instead of leaving no route.
#[derive(Default)]
struct Bridges {
    list: Mutex<Vec<Arc<Mux>>>,
    /// Signalled when a bridge attaches, waking held agent clients.
    arrived: Condvar,
}

impl Bridges {
    /// The newest bridge, waiting up to `HOLD` for one to attach.
    fn route(&self) -> Option<Arc<Mux>> {
        let list = self.list.lock().unwrap();
        let (list, _) = self.arrived.wait_timeout_while(list, HOLD, |l| l.is_empty()).unwrap();
        list.last().cloned()
    }
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
        std::thread::spawn(move || serve_bridge(conn, &bridges, &hash));
    }
}

/// One control connection. Its first frame is `Quit` (a daemon of another
/// build taking over: exit) or a bridge's `Hello`, answered with ours even on
/// a version mismatch so the bridge can report it. A matching bridge then
/// becomes the newest route until it disconnects.
fn serve_bridge(mut conn: UnixStream, bridges: &Bridges, hash: &str) {
    match proto::read_frame(&mut conn) {
        Ok(Some(Frame::Quit)) => std::process::exit(0),
        Ok(Some(Frame::Hello { version, .. })) => {
            let ours = Frame::Hello { version: proto::VERSION, hash: hash.to_string(), caps: 0 };
            if proto::write_frame(&mut conn, &ours).is_err() || version != proto::VERSION {
                return;
            }
        }
        _ => return,
    }
    let Ok(out) = conn.try_clone() else { return };
    let mux = Mux::new(out);
    bridges.list.lock().unwrap().push(Arc::clone(&mux));
    bridges.arrived.notify_all();
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
    // The daemon never accepts `Open`. `Connect` is refused until the daemon
    // learns to dial (docs/port-forwarding.md, step 4).
    mux.serve_with(conn, |_, _| None, |_, _, _, reply: mux::ConnectReply| {
        reply.finish(Err("port forwarding not enabled".into()))
    });
    bridges.list.lock().unwrap().retain(|m| !Arc::ptr_eq(m, &mux));
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
}
