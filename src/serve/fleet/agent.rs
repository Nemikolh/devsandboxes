//! `AGENT` on the fleet link (docs/architecture/daemon/fleet.md, *AGENT*):
//! the hub's ssh agent reaching every instance on an offered worker while
//! the hub's registry entry for the worker has `agent = true` (decision 14
//! of the fleet plan), the agent relay's semantics (`machines::agent`)
//! carried on the link instead of an ssh of its own.
//!
//! Every `AGENT` stream starts with one status byte from the hub:
//! [`SERVING`], then the relay to the hub's agent, or [`NOT_SERVING`], then
//! the close.
//!
//! - The worker's end, [`serve`]: while a link is up, the lending thread
//!   probes the hub every [`PROBE`] (an `AGENT` stream it reads the status
//!   byte of and closes) and binds [`socket`] (`agent-fleet-<hub>.sock` in
//!   its socket dir, as `link::opener` binds: temp name and rename, newest
//!   wins, 0600, unlinked while still its own) only while the hub serves
//!   ([`decide`]). Each connection there becomes one `AGENT` stream to the
//!   hub (`link::Outlet::open`), spliced both ways past a `SERVING` byte;
//!   any other answer closes the client and unbinds. Named `agent-*.sock`,
//!   it is one of the relay sockets `bridge::host_agent` tries, so the
//!   worker's bridges find it as they find a machine's agent relay, and
//!   only while it leads somewhere.
//! - The hub's end, [`Gate`]: `commands::fleet_link` answers every `AGENT`
//!   stream with a socket pair whose far end a thread of its own takes (the
//!   handler runs on the mux's read thread): the status byte, then, once
//!   the worker sends its first bytes, a connection to the agent path its
//!   daemon last gave (`fleet.agent`, [`Cached`]), while fresh. The path is
//!   read on a thread of its own too ([`refresh`]).

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::link::{Outlet, Socket};
use crate::devsbd::mux::Conn;
use crate::devsbd::proto::channel;
use crate::serve::endpoint;

/// The status byte of a stream the hub serves: the relay follows.
pub const SERVING: u8 = 1;
/// The status byte of a stream the hub doesn't serve: the close follows.
pub const NOT_SERVING: u8 = 0;

/// The worker's agent socket for its link to `hub` (its ssh target) in
/// `dir`: `endpoint::agent_socket` of client `fleet-<hub>`.
pub fn socket(dir: &Path, hub: &str) -> PathBuf {
    endpoint::agent_socket(dir, &format!("fleet-{hub}"))
}

/// How often the worker asks the hub whether it serves `AGENT`: with the
/// hub's own [`REFRESH`], a `machine set <w> agent on|off` binds or
/// unbinds the worker's socket within about twice this.
pub const PROBE: Duration = Duration::from_secs(2);

/// How long the worker waits for a stream's status byte.
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

/// What the worker does with its socket after an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Bind,
    Unbind,
    Keep,
}

/// The worker's socket after an answer `status` (pure): bound while the
/// hub serves, unbound on anything else.
pub fn decide(bound: bool, status: Status) -> Action {
    match (bound, status) {
        (false, Status::Serving) => Action::Bind,
        (true, Status::NotServing | Status::Failed) => Action::Unbind,
        _ => Action::Keep,
    }
}

/// The worker's end at `path` for the link `outlet` stands for, until `over`
/// is set or the outlet goes down: once the outlet is up, a probe every
/// `every` ([`PROBE`] in production) binds or unbinds the socket
/// ([`decide`]); a client the hub turns down unbinds it too. Unbound (the
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

/// A client of the worker's socket on its own thread: past a `SERVING`
/// byte spliced to `stream`; else both closed and `refused` set (the
/// socket goes).
fn relay(client: UnixStream, stream: Conn, refused: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new().name("serve-lend-agent-conn".into()).spawn(move || {
        match read_status(&stream, STATUS_WAIT) {
            Status::Serving => splice(client, stream),
            _ => refused.store(true, Ordering::Relaxed),
        }
    });
}

/// Relay `client` and `stream` both ways on threads of their own, each
/// direction's end passed on as a half-close.
fn splice(client: UnixStream, stream: Conn) {
    let (Ok(client2), Ok(stream2)) = (client.try_clone(), stream.try_clone()) else { return };
    let up = move || {
        let _ = io::copy(&mut &client, &mut &stream);
        let _ = stream.shutdown(Shutdown::Write);
    };
    let down = move || {
        let _ = io::copy(&mut &stream2, &mut &client2);
        let _ = client2.shutdown(Shutdown::Write);
    };
    let _ = std::thread::Builder::new().name("fleet-agent-up".into()).spawn(up);
    let _ = std::thread::Builder::new().name("fleet-agent-down".into()).spawn(down);
}

/// How often the hub's `fleet-link` asks its daemon for the agent
/// (`fleet.agent`, [`refresh`]).
pub const REFRESH: Duration = Duration::from_secs(2);

/// How old the last answer may be and still count: past it (the daemon
/// stopped answering) streams are not served.
pub const MAX_AGE: Duration = Duration::from_secs(10);

/// The hub daemon's last `fleet.agent` answer, and when it came.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cached {
    agent: Option<PathBuf>,
    at: Option<Instant>,
}

impl Cached {
    /// `agent`, as answered at `at`.
    pub fn read(agent: Option<PathBuf>, at: Instant) -> Cached {
        Cached { agent, at: Some(at) }
    }

    /// The agent a stream gets at `now`: the answer's, while it is less
    /// than [`MAX_AGE`] old; never before a first answer.
    pub fn fresh(&self, now: Instant) -> Option<&Path> {
        let at = self.at?;
        (now.saturating_duration_since(at) < MAX_AGE).then_some(self.agent.as_deref()).flatten()
    }
}

/// The hub's `AGENT` handler's state: the [`Cached`] answer [`refresh`]
/// keeps.
#[derive(Debug, Default)]
pub struct Gate {
    cached: Mutex<Cached>,
}

impl Gate {
    pub fn set(&self, cached: Cached) {
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = cached;
    }

    /// The answer to one `AGENT` stream at `now`, at once: a socket pair
    /// whose far end [`answer`] takes on a thread of its own, with the agent
    /// fresh at `now` (`None` without a socket pair or a thread: refused).
    pub fn open(&self, now: Instant) -> Option<Conn> {
        let agent = self.cached.lock().unwrap_or_else(|e| e.into_inner()).fresh(now).map(Path::to_path_buf);
        let (ours, theirs) = UnixStream::pair().ok()?;
        std::thread::Builder::new().name("fleet-link-agent-conn".into()).spawn(move || answer(ours, agent)).ok()?;
        Some(theirs.into())
    }
}

/// One `AGENT` stream on the hub: [`NOT_SERVING`] without an agent; else
/// [`SERVING`], and once the worker's first bytes come (a probe sends none
/// and closes: no connection to the agent), a connection to `agent` they
/// open, spliced.
fn answer(ours: UnixStream, agent: Option<PathBuf>) {
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

/// Keep `gate` fresh on a thread of its own: `read()` (the daemon's
/// `fleet.agent` answer) at once, then every `every`, until the returned
/// sender is dropped. A failed read keeps the last answer, which ages out
/// ([`MAX_AGE`]); `on_error` hears the first of each run of failures.
pub fn refresh(
    gate: Arc<Gate>,
    every: Duration,
    mut read: impl FnMut() -> Result<Option<PathBuf>> + Send + 'static,
    on_error: impl Fn(&anyhow::Error) + Send + 'static,
) -> mpsc::Sender<()> {
    let (stop, stopped) = mpsc::channel::<()>();
    let _ = std::thread::Builder::new().name("fleet-link-agent".into()).spawn(move || {
        let mut failing = false;
        loop {
            match read() {
                Ok(agent) => {
                    gate.set(Cached::read(agent, Instant::now()));
                    failing = false;
                }
                Err(e) => {
                    if !failing {
                        on_error(&e);
                    }
                    failing = true;
                }
            }
            match stopped.recv_timeout(every) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        }
    });
    stop
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn a_cached_answer_counts_while_fresh() {
        let t = Instant::now();
        let a = || Some(PathBuf::from("/a"));
        assert_eq!(Cached::default().fresh(t), None, "never read");
        assert_eq!(Cached::read(a(), t).fresh(t), Some(Path::new("/a")));
        assert_eq!(Cached::read(a(), t).fresh(t + MAX_AGE - Duration::from_millis(1)), Some(Path::new("/a")));
        assert_eq!(Cached::read(a(), t).fresh(t + MAX_AGE), None, "aged out");
        assert_eq!(Cached::read(None, t).fresh(t), None);
        // A clock read before the answer (another thread's `now`) still counts.
        assert!(Cached::read(a(), t + Duration::from_secs(1)).fresh(t).is_some());
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

    /// Everything a `Gate` stream says to a client sending `send`, then
    /// half-closing.
    fn through(gate: &Gate, now: Instant, send: &[u8]) -> Vec<u8> {
        let Some(Conn::Unix(mut c)) = gate.open(now) else { panic!("a stream is always answered") };
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // A refusal may close before this write (a broken pipe), or after it
        // with what was sent unread (a reset after the byte).
        let _ = c.write_all(send);
        let _ = c.shutdown(Shutdown::Write);
        let mut got = Vec::new();
        let _ = c.read_to_end(&mut got);
        got
    }

    #[test]
    fn the_gate_says_whether_it_serves_and_relays_only_then() {
        let dir = scratch("gate");
        let (agent, connects) = echo_agent(&dir);
        let gate = Gate::default();
        let now = Instant::now();
        assert_eq!(through(&gate, now, b"hi"), [NOT_SERVING], "nothing read yet");
        gate.set(Cached::read(None, now));
        assert_eq!(through(&gate, now, b"hi"), [NOT_SERVING], "off");
        gate.set(Cached::read(Some(agent.clone()), now));
        assert_eq!(through(&gate, now, b"hi"), b"\x01hi", "on: the status byte, then the agent");
        assert_eq!(through(&gate, now + MAX_AGE, b"hi"), [NOT_SERVING], "stale");
        // A probe sends nothing: the agent is never dialed for it.
        let before = connects.load(Ordering::SeqCst);
        assert_eq!(through(&gate, now, b""), [SERVING]);
        assert_eq!(connects.load(Ordering::SeqCst), before);
        // A dead agent: served, then closed.
        gate.set(Cached::read(Some(dir.join("gone")), now));
        assert_eq!(through(&gate, now, b"hi"), [SERVING]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The worker's end over a real link to a fake hub (a [`Gate`] in front
    /// of an echo agent): bound only while the hub serves, relaying with
    /// half-closes, unbound when it stops serving or the link dies.
    #[test]
    fn the_worker_socket_follows_the_hub_and_goes_with_the_link() {
        use super::super::LINK;
        use super::super::link::{self, Serves};
        let dir = scratch("w");
        let (agent, _) = echo_agent(&dir);
        let (worker_io, hub_io) = UnixStream::pair().unwrap();
        let cut = hub_io.try_clone().unwrap();
        let api = dir.join("fleet-w.sock");
        let gate = Arc::new(Gate::default());
        let hub = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                let serves = Serves::default().with(channel::AGENT, move || gate.open(Instant::now()));
                link::opener_with(&LINK, hub_io.try_clone().unwrap(), hub_io, &[(channel::API, api.as_path())], serves, |_| Ok(()))
            })
        };
        let outlet = Arc::new(Outlet::default());
        let answering = Arc::clone(&outlet);
        let worker = std::thread::spawn(move || {
            let serves = Serves::default().with(channel::API, || None);
            link::answerer_with(&LINK, worker_io.try_clone().unwrap(), worker_io, &serves, Some(&answering), |_| Ok(()), || {})
        });
        let path = socket(&dir, "me@hub");
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

        gate.set(Cached::read(Some(agent.clone()), Instant::now()));
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

        gate.set(Cached::read(None, Instant::now()));
        wait(false, "no longer served: unbound");
        gate.set(Cached::read(Some(agent), Instant::now()));
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

    #[test]
    fn refresh_reads_until_dropped_and_keeps_the_answer_on_errors() {
        let gate = Arc::new(Gate::default());
        let (tx, reads) = mpsc::channel();
        let (errors, errored) = mpsc::channel();
        let mut n = 0;
        let stop = refresh(
            Arc::clone(&gate),
            Duration::from_millis(10),
            move || {
                n += 1;
                let _ = tx.send(n);
                if n == 1 { Ok(Some(PathBuf::from("/a"))) } else { anyhow::bail!("daemon away") }
            },
            move |e| errors.send(e.to_string()).unwrap(),
        );
        assert_eq!(reads.recv_timeout(Duration::from_secs(5)), Ok(1));
        assert_eq!(reads.recv_timeout(Duration::from_secs(5)), Ok(2));
        assert_eq!(reads.recv_timeout(Duration::from_secs(5)), Ok(3));
        // The failed reads left the first one's answer.
        assert_eq!(gate.cached.lock().unwrap().fresh(Instant::now()), Some(Path::new("/a")));
        assert_eq!(errored.try_iter().collect::<Vec<_>>(), ["daemon away"], "the first of a run only");
        drop(stop);
        // At most one read already under way; then none.
        let _ = reads.recv_timeout(Duration::from_millis(100));
        assert!(reads.recv_timeout(Duration::from_millis(100)).is_err(), "stopped");
    }
}
