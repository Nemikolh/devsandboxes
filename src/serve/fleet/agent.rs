//! `AGENT` on the fleet link (docs/architecture/daemon/fleet.md, *AGENT*):
//! the dialer's ssh agent reaching every instance on the machine it dialed
//! while the dialer's registry entry for it has `agent = true` (decisions
//! 14, 18 and 30 of the fleet plan), carried on the link instead of an ssh
//! of its own.
//!
//! Every `AGENT` stream starts with one status byte from the dialer:
//! [`SERVING`], then the relay to its agent, or [`NOT_SERVING`], then the
//! close.
//!
//! - The dialed end, [`serve`]: while a link is up, `fleet-link --peer`
//!   probes the dialer every [`PROBE`] (an `AGENT` stream it reads the
//!   status byte of and closes) and binds `agent-link-<dialer>.sock`
//!   (`endpoint::link_agent_socket`, as `link::opener` binds: temp name and
//!   rename, newest wins, 0600, unlinked while still its own) only while
//!   the dialer serves ([`decide`]). Each connection there becomes one
//!   `AGENT` stream to the dialer (`link::Outlet::open`), spliced both ways
//!   past a `SERVING` byte; any other answer closes the client and
//!   unbinds. Named `agent-*.sock`, it is one of the relayed agents'
//!   sockets `bridge::host_agent` tries, and only while it leads somewhere.
//! - The dialer's end, [`answer`]: the daemon answers each `AGENT` stream
//!   (`Shared::serve_channel`) with the status byte, then, once the far end
//!   sends its first bytes, a connection to this host's agent.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::link::{Outlet, Socket, splice};
use crate::devsbd::mux::Conn;
use crate::devsbd::proto::channel;
use crate::serve::endpoint;

/// The status byte of a stream the dialer serves: the relay follows.
pub const SERVING: u8 = 1;
/// The status byte of a stream the dialer doesn't serve: the close follows.
pub const NOT_SERVING: u8 = 0;

/// How often the dialed end asks the dialer whether it serves `AGENT`: a
/// `machine set <m> agent on|off` there binds or unbinds the socket here
/// within about this.
pub const PROBE: Duration = Duration::from_secs(2);

/// How long the dialed end waits for a stream's status byte.
const STATUS_WAIT: Duration = Duration::from_secs(5);

/// How often [`serve`] looks up from its listener (or its sleep) to see
/// whether the link ended or a probe is due.
const POLL: Duration = Duration::from_millis(200);

/// What a stream's status byte said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Serving,
    NotServing,
    /// No byte (the stream closed, the wait ran out, it couldn't be
    /// opened), or another one.
    Failed,
}

/// The status of the byte read (`None`: none came).
pub fn status_of(byte: Option<u8>) -> Status {
    match byte {
        Some(SERVING) => Status::Serving,
        Some(NOT_SERVING) => Status::NotServing,
        _ => Status::Failed,
    }
}

/// What the dialed end does with its socket after an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Bind,
    Unbind,
    Keep,
}

/// The dialed end's socket after an answer `status` (pure): bound while
/// the dialer serves, unbound on anything else.
pub fn decide(bound: bool, status: Status) -> Action {
    match (bound, status) {
        (false, Status::Serving) => Action::Bind,
        (true, Status::NotServing | Status::Failed) => Action::Unbind,
        _ => Action::Keep,
    }
}

/// The dialed end at `path` for the link `outlet` stands for, until `over`
/// is set or the outlet goes down: once the outlet is up, a probe every
/// `every` ([`PROBE`] in production) binds or unbinds the socket
/// ([`decide`]); a client the dialer turns down unbinds it too. Unbound (the
/// file unlinked unless a newer one replaced it) when this returns.
pub fn serve(path: &Path, outlet: &Outlet, over: &AtomicBool, every: Duration) -> Result<()> {
    while !outlet.is_up() {
        if over.load(Ordering::Relaxed) {
            return Ok(());
        }
        std::thread::sleep(POLL / 10);
    }
    let refused = Arc::new(AtomicBool::new(false));
    let mut bound: Option<Socket> = None;
    let mut next_probe = Instant::now();
    while !over.load(Ordering::Relaxed) && outlet.is_up() {
        let answer = if refused.swap(false, Ordering::Relaxed) {
            Some(Status::NotServing)
        } else if Instant::now() >= next_probe {
            next_probe = Instant::now() + every;
            Some(probe(outlet))
        } else {
            None
        };
        if let Some(status) = answer {
            match decide(bound.is_some(), status) {
                Action::Bind => bound = Some(Socket::bind(path)?),
                Action::Unbind => bound = None,
                Action::Keep => {}
            }
        }
        let wait = POLL.min(next_probe.saturating_duration_since(Instant::now()));
        let Some(socket) = &bound else {
            std::thread::sleep(wait);
            continue;
        };
        match endpoint::accept_any(&[&socket.listener], wait) {
            Ok(Some((_, client))) => {
                if let Ok(stream) = outlet.open(channel::AGENT) {
                    relay(client, stream, Arc::clone(&refused));
                }
            }
            Ok(None) => {}
            Err(e) => return Err(e).with_context(|| format!("agent socket {}", path.display())),
        }
    }
    Ok(())
}

/// One probe: an `AGENT` stream's status byte, the stream then closed.
fn probe(outlet: &Outlet) -> Status {
    match outlet.open(channel::AGENT) {
        Ok(stream) => read_status(&stream, STATUS_WAIT),
        Err(_) => Status::Failed,
    }
}

/// The status byte on `stream`, waiting up to `wait`; the wait cleared
/// after.
fn read_status(stream: &Conn, wait: Duration) -> Status {
    let timeout = |t| match stream {
        Conn::Unix(s) => s.set_read_timeout(t),
        Conn::Tcp(s) => s.set_read_timeout(t),
    };
    if timeout(Some(wait)).is_err() {
        return Status::Failed;
    }
    let mut byte = [0u8; 1];
    let mut r = stream;
    let status = status_of(r.read_exact(&mut byte).ok().map(|()| byte[0]));
    if timeout(None).is_err() {
        return Status::Failed;
    }
    status
}

/// A client of the dialed end's socket on its own thread: past a `SERVING`
/// byte spliced to `stream`; else both closed and `refused` set (the
/// socket goes).
fn relay(client: UnixStream, stream: Conn, refused: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new().name("fleet-link-agent-conn".into()).spawn(move || {
        match read_status(&stream, STATUS_WAIT) {
            Status::Serving => splice(client, stream),
            _ => refused.store(true, Ordering::Relaxed),
        }
    });
}

/// One `AGENT` stream on the dialer: [`NOT_SERVING`] without an agent;
/// else [`SERVING`], and once the far end's first bytes come (a probe
/// sends none and closes: no connection to the agent), a connection to
/// `agent` they open, spliced (`Shared::serve_channel`).
pub fn answer(ours: UnixStream, agent: Option<PathBuf>) {
    let Some(agent) = agent else {
        let _ = (&ours).write_all(&[NOT_SERVING]);
        return;
    };
    if (&ours).write_all(&[SERVING]).is_err() {
        return;
    }
    let mut first = [0u8; 8192];
    let n = match (&ours).read(&mut first) {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let Ok(to) = UnixStream::connect(&agent) else { return };
    if (&to).write_all(&first[..n]).is_err() {
        return;
    }
    splice(ours, to.into());
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::Shutdown;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-fagent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn status_bytes_and_the_bind_decision() {
        assert_eq!(status_of(Some(1)), Status::Serving);
        assert_eq!(status_of(Some(0)), Status::NotServing);
        assert_eq!(status_of(Some(7)), Status::Failed);
        assert_eq!(status_of(None), Status::Failed);
        use Action::*;
        for (bound, status, want) in [
            (false, Status::Serving, Bind),
            (true, Status::Serving, Keep),
            (true, Status::NotServing, Unbind),
            (true, Status::Failed, Unbind),
            (false, Status::NotServing, Keep),
            (false, Status::Failed, Keep),
        ] {
            assert_eq!(decide(bound, status), want, "{bound} {status:?}");
        }
    }

    /// A fake agent at `dir/agent`: echoes each connection, counted.
    fn echo_agent(dir: &Path) -> (PathBuf, Arc<AtomicUsize>) {
        let path = dir.join("agent");
        let listener = UnixListener::bind(&path).unwrap();
        let n = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&n);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                counted.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let _ = io::copy(&mut &conn, &mut &conn);
                });
            }
        });
        (path, n)
    }

    /// The dialed end over a real link to a fake dialer ([`answer`] in front
    /// of an echo agent while `on`): bound only while the dialer serves,
    /// relaying with half-closes, unbound when it stops serving or the link
    /// dies.
    #[test]
    fn the_socket_follows_the_dialer_and_goes_with_the_link() {
        use super::super::LINK;
        use super::super::link::{self, Serves};
        let dir = scratch("w");
        let (agent, _) = echo_agent(&dir);
        let (worker_io, hub_io) = UnixStream::pair().unwrap();
        let cut = hub_io.try_clone().unwrap();
        let api = dir.join("fleet-w.sock");
        let on = Arc::new(AtomicBool::new(false));
        let hub = {
            let (on, agent) = (Arc::clone(&on), agent.clone());
            std::thread::spawn(move || {
                let open = move || {
                    let (ours, theirs) = UnixStream::pair().ok()?;
                    let agent = on.load(Ordering::SeqCst).then(|| agent.clone());
                    std::thread::spawn(move || answer(ours, agent));
                    Some(theirs.into())
                };
                let serves = Serves::default().with(channel::AGENT, open);
                let sockets = [(channel::API, api.as_path())];
                let endpoint = link::Endpoint { sockets: &sockets, serves, outlet: None };
                link::opener_with(&LINK, hub_io.try_clone().unwrap(), hub_io, endpoint, |_| Ok(()))
            })
        };
        let outlet = Arc::new(Outlet::default());
        let answering = Arc::clone(&outlet);
        let worker = std::thread::spawn(move || {
            let serves = Serves::default().with(channel::API, || None);
            let endpoint = link::Endpoint { serves, outlet: Some(&answering), ..link::Endpoint::default() };
            link::answerer_with(&LINK, worker_io.try_clone().unwrap(), worker_io, endpoint, |_| Ok(()), || {})
        });
        let path = endpoint::link_agent_socket(&dir, "hub");
        let over = Arc::new(AtomicBool::new(false));
        let serving = {
            let (path, outlet, over) = (path.clone(), Arc::clone(&outlet), Arc::clone(&over));
            std::thread::spawn(move || serve(&path, &outlet, &over, Duration::from_millis(30)))
        };
        let wait = |want: bool, what: &str| {
            let start = Instant::now();
            while path.exists() != want {
                assert!(start.elapsed() < Duration::from_secs(5), "{what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!path.exists(), "not served: not bound");

        on.store(true, Ordering::SeqCst);
        wait(true, "served: bound");
        assert_eq!(endpoint::agent_sockets(&dir), [path.clone()]);
        for msg in [&b"one"[..], b"two"] {
            let mut c = UnixStream::connect(&path).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            c.write_all(msg).unwrap();
            c.shutdown(Shutdown::Write).unwrap();
            let mut got = Vec::new();
            c.read_to_end(&mut got).unwrap();
            assert_eq!(got, msg, "the agent's answer, past the status byte");
        }

        on.store(false, Ordering::SeqCst);
        wait(false, "no longer served: unbound");
        on.store(true, Ordering::SeqCst);
        wait(true, "served again");

        // The link dies: the outlet goes down, the socket with it.
        cut.shutdown(Shutdown::Both).unwrap();
        serving.join().unwrap().unwrap();
        assert!(!path.exists(), "unlinked");
        assert!(endpoint::agent_sockets(&dir).is_empty(), "no stale socket for a bridge to pick");
        assert!(matches!(worker.join().unwrap(), link::End::Closed));
        hub.join().unwrap().unwrap();
        assert!(!over.load(Ordering::Relaxed), "it ended with the outlet, not the flag");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serve_returns_unbound_when_the_link_ends_before_it_is_up() {
        let path = std::env::temp_dir().join(format!("dsv-fagent-never-{}.sock", std::process::id()));
        let over = AtomicBool::new(true);
        serve(&path, &Outlet::default(), &over, PROBE).unwrap();
        assert!(!path.exists());
    }
}
