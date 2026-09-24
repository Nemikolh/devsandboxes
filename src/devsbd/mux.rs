//! Stream multiplexer over one frame connection, shared by the helper daemon
//! and the host bridge driver (devsbd includes it via `#[path]`, like
//! `proto.rs`). Both ends run the same routing so their close semantics can't
//! drift: whoever sees a local stream end first sends `Close`; receiving
//! `Close` shuts the local socket down without echoing one back.
//!
//! Local ends are a [`Conn`]: a unix socket (ssh-agent streams) or a TCP
//! socket (forwarded ports, docs/port-forwarding.md). Unix-only: the Windows
//! host will need a named-pipe stream here (docs/sandbox-helper.md).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::proto::{self, Frame};

/// Read chunk for local stream -> `Data` frames; far below `MAX_PAYLOAD`.
const CHUNK: usize = 16 * 1024;

/// Keepalive `Ping` cadence and the inbound-silence window after which the
/// peer is declared dead (`on_dead`). A wedged peer that stops reading (so our
/// `Ping`s pile up unanswered) is caught within `KEEPALIVE_TIMEOUT`.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(45);

/// Pure liveness rule, split out so the timeout logic is testable with
/// injected durations: dead once no inbound frame has arrived for `timeout`.
fn is_dead(since_last_inbound: Duration, timeout: Duration) -> bool {
    since_last_inbound >= timeout
}

/// Cap on concurrently live streams per bridge. An `Open` past this is refused
/// (`Close`); ssh-agent volumes never approach it, so it's purely a bound on a
/// misbehaving or hostile peer.
const MAX_STREAMS: usize = 64;

/// Stream-id spaces: the daemon allocates ids with the high bit clear
/// (1..2^31), the host with it set (none yet). The host refuses a peer `Open`
/// whose id has this bit set (docs/sandbox-helper.md, _Frame protocol_).
const HOST_ID_BIT: u32 = 1 << 31;

/// A stream's local socket. An enum rather than a trait object keeps the mux
/// std-only and monomorphic; both variants share the same read/write/shutdown
/// surface, so routing doesn't care which one it holds.
#[derive(Debug)]
pub enum Conn {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Conn {
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Conn::Unix(s) => s.shutdown(how),
            Conn::Tcp(s) => s.shutdown(how),
        }
    }

    pub fn try_clone(&self) -> io::Result<Conn> {
        Ok(match self {
            Conn::Unix(s) => Conn::Unix(s.try_clone()?),
            Conn::Tcp(s) => Conn::Tcp(s.try_clone()?),
        })
    }
}

impl From<UnixStream> for Conn {
    fn from(s: UnixStream) -> Conn {
        Conn::Unix(s)
    }
}

impl From<TcpStream> for Conn {
    fn from(s: TcpStream) -> Conn {
        Conn::Tcp(s)
    }
}

/// `&Conn` like `&UnixStream`/`&TcpStream`: lets the `Data` arm write through
/// a shared `Arc<Conn>` without a lock.
impl Read for &Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            Conn::Unix(s) => (&*s).read(buf),
            Conn::Tcp(s) => (&*s).read(buf),
        }
    }
}

impl Write for &Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            Conn::Unix(s) => (&*s).write(buf),
            Conn::Tcp(s) => (&*s).write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match *self {
            Conn::Unix(s) => (&*s).flush(),
            Conn::Tcp(s) => (&*s).flush(),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

pub struct Mux {
    out: Mutex<Box<dyn Write + Send>>,
    // `Arc` so the `Data` arm can clone a handle and write after dropping the
    // `streams` guard, instead of holding the lock across a blocking write.
    streams: Mutex<HashMap<u32, Arc<Conn>>>,
    /// When `serve` last saw any inbound frame; the keepalive thread reads it
    /// to decide liveness. Initialised at construction so a peer that never
    /// sends a frame is still declared dead after `KEEPALIVE_TIMEOUT`.
    last_inbound: Mutex<Instant>,
    /// Set when `serve` returns, so the keepalive thread stops instead of
    /// leaking (and pinging a dead connection) once the bridge is gone.
    ended: AtomicBool,
}

impl Mux {
    pub fn new(out: impl Write + Send + 'static) -> Arc<Mux> {
        Arc::new(Mux {
            out: Mutex::new(Box::new(out)),
            streams: Mutex::new(HashMap::new()),
            last_inbound: Mutex::new(Instant::now()),
            ended: AtomicBool::new(false),
        })
    }

    /// Spawn the keepalive thread: send `Ping` every `interval` and, once
    /// `timeout` passes with no inbound frame, call `on_dead` once and stop.
    /// Also stops (without calling `on_dead`) once `serve` has returned, so a
    /// thread never outlives its bridge. `on_dead` typically tears the
    /// connection down (shutdown/kill), which unblocks the `serve` read.
    pub fn keepalive(
        self: &Arc<Self>,
        interval: Duration,
        timeout: Duration,
        on_dead: impl FnOnce() + Send + 'static,
    ) {
        let mux = Arc::clone(self);
        std::thread::spawn(move || {
            let mut on_dead = Some(on_dead);
            while !mux.ended.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                if mux.ended.load(Ordering::Relaxed) {
                    return;
                }
                let idle = mux.last_inbound.lock().unwrap().elapsed();
                if is_dead(idle, timeout) {
                    if let Some(f) = on_dead.take() {
                        f();
                    }
                    return;
                }
                // `try_lock`: a writer stuck on a wedged peer holds `out`;
                // queuing behind it would park this thread and the silence
                // check above would never fire. Skip this tick's ping instead.
                if let Ok(mut out) = mux.out.try_lock() {
                    let _ = proto::write_frame(&mut *out, &Frame::Ping(Vec::new()));
                }
            }
        });
    }

    pub fn send(&self, frame: &Frame) -> io::Result<()> {
        proto::write_frame(&mut *self.out.lock().unwrap(), frame)
    }

    /// Register `conn` as `stream` and pump its reads into `Data` frames on a
    /// new thread, sending `Close` when it ends (unless the peer closed it
    /// first). With `open`, announces the stream with `Open { channel }` before
    /// any data can flow.
    pub fn attach(self: &Arc<Self>, stream: u32, conn: impl Into<Conn>, open: Option<u8>) -> io::Result<()> {
        let conn = conn.into();
        let reader = conn.try_clone()?;
        self.streams.lock().unwrap().insert(stream, Arc::new(conn));
        if let Some(channel) = open {
            if let Err(e) = self.send(&Frame::Open { stream, channel }) {
                self.streams.lock().unwrap().remove(&stream);
                return Err(e);
            }
        }
        let mux = Arc::clone(self);
        std::thread::spawn(move || mux.pump(stream, reader));
        Ok(())
    }

    fn pump(&self, stream: u32, mut conn: Conn) {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if self.send(&Frame::Data { stream, bytes: buf[..n].to_vec() }).is_err() {
                        break;
                    }
                }
            }
        }
        // Still registered = we ended first, so the peer needs to hear it.
        if self.drop_stream(stream) {
            let _ = self.send(&Frame::Close { stream, reason: String::new() });
        }
    }

    /// Unregister and shut down `stream`; `false` when it was already gone.
    fn drop_stream(&self, stream: u32) -> bool {
        match self.streams.lock().unwrap().remove(&stream) {
            Some(conn) => {
                let _ = conn.shutdown(Shutdown::Both);
                true
            }
            None => false,
        }
    }

    /// Route frames from `reader` until it ends: `Data` to its stream,
    /// `Close` shuts it, `Ping` gets a `Pong`, and `Open` asks `on_open` for a
    /// local connection (`None` refuses the stream with `Close`). Every stream
    /// is shut down on return, since the peer can no longer see it.
    pub fn serve(
        self: &Arc<Self>,
        mut reader: impl Read,
        on_open: impl Fn(u32, u8) -> Option<Conn>,
    ) {
        while let Ok(Some(frame)) = proto::read_frame(&mut reader) {
            *self.last_inbound.lock().unwrap() = Instant::now();
            match frame {
                Frame::Data { stream, bytes } => {
                    // Clone the handle and drop the guard before the blocking
                    // write, so a slow local socket can't stall other streams'
                    // routing (`&Conn: Write`).
                    let conn = self.streams.lock().unwrap().get(&stream).cloned();
                    let failed = match conn {
                        Some(conn) => (&*conn).write_all(&bytes).is_err(),
                        // Raced a local close; its `Close` is already on the way.
                        None => false,
                    };
                    if failed && self.drop_stream(stream) {
                        let _ = self.send(&Frame::Close { stream, reason: String::new() });
                    }
                }
                Frame::Close { stream, .. } => {
                    self.drop_stream(stream);
                }
                Frame::Ping(payload) => {
                    let _ = self.send(&Frame::Pong(payload));
                }
                Frame::Open { stream, channel } => {
                    // Host-side policy for peer-allocated stream ids. The
                    // high bit is reserved for host-allocated ids (none yet),
                    // so a peer `Open` must have it clear; refuse a duplicate
                    // id or one past the stream cap. Sending `Close` for a
                    // duplicate makes a misbehaving peer drop its *existing*
                    // stream with that id — acceptable, since only a broken or
                    // hostile peer reuses a live id. The daemon side passes
                    // `on_open = |_,_| None` and never reaches the accept path.
                    let attached = {
                        let live = self.streams.lock().unwrap();
                        let allowed = stream & HOST_ID_BIT == 0
                            && !live.contains_key(&stream)
                            && live.len() < MAX_STREAMS;
                        drop(live);
                        allowed
                            && match on_open(stream, channel) {
                                Some(conn) => self.attach(stream, conn, None).is_ok(),
                                None => false,
                            }
                    };
                    if !attached {
                        let _ = self.send(&Frame::Close { stream, reason: String::new() });
                    }
                }
                // `Quit` only means something as a control socket's first
                // frame, which the daemon reads before handing over to `serve`.
                // The port-forwarding frames (`Connect`/`Window`/`Eof`/`Caps`)
                // are handled in step 3; ignored here so this build is inert to
                // them, exactly as an old peer would be.
                Frame::Hello { .. }
                | Frame::Pong(_)
                | Frame::Quit
                | Frame::Connect { .. }
                | Frame::Window { .. }
                | Frame::Eof { .. }
                | Frame::Caps(_) => {}
            }
        }
        self.ended.store(true, Ordering::Relaxed);
        self.close_all();
    }

    pub fn close_all(&self) {
        for (_, conn) in self.streams.lock().unwrap().drain() {
            let _ = conn.shutdown(Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::channel;
    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    /// Fake agent: answers each line with `re:<line>`.
    fn echo_agent(conn: UnixStream) {
        std::thread::spawn(move || {
            let mut w = conn.try_clone().unwrap();
            for line in BufReader::new(conn).lines() {
                let Ok(line) = line else { break };
                if writeln!(w, "re:{line}").is_err() {
                    break;
                }
            }
        });
    }

    /// A daemon-side mux and a host-side mux joined by OS pipes, the host
    /// connecting each `Open` to a fresh echo agent.
    fn pair() -> Arc<Mux> {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || d.serve(d_r, |_, _| None));
        std::thread::spawn(move || {
            host.serve(h_r, |_, ch| {
                (ch == channel::SSH_AGENT).then(|| {
                    let (ours, agent) = UnixStream::pair().unwrap();
                    echo_agent(agent);
                    ours.into()
                })
            })
        });
        daemon
    }

    fn client(daemon: &Arc<Mux>, stream: u32, channel: u8) -> BufReader<UnixStream> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        daemon.attach(stream, theirs, Some(channel)).unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        BufReader::new(ours)
    }

    #[test]
    fn streams_relay_independently() {
        let daemon = pair();
        let mut a = client(&daemon, 1, channel::SSH_AGENT);
        let mut b = client(&daemon, 2, channel::SSH_AGENT);
        let roundtrip = |c: &mut BufReader<UnixStream>, msg: &str| {
            writeln!(c.get_mut(), "{msg}").unwrap();
            let mut line = String::new();
            c.read_line(&mut line).unwrap();
            assert_eq!(line, format!("re:{msg}\n"));
        };
        roundtrip(&mut a, "one");
        roundtrip(&mut b, "two");
        roundtrip(&mut a, "three");
    }

    /// Same relay as `streams_relay_independently`, but with TCP on both local
    /// ends: the daemon attaches a loopback client, the host's `on_open` dials a
    /// loopback echo server. Exercises every `Conn::Tcp` arm.
    #[test]
    fn tcp_conns_relay_through_the_mux() {
        use std::net::{TcpListener, TcpStream};
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let echo_addr = echo.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in echo.incoming() {
                let conn = conn.unwrap();
                std::thread::spawn(move || {
                    let mut w = conn.try_clone().unwrap();
                    for line in BufReader::new(conn).lines() {
                        let Ok(line) = line else { break };
                        if writeln!(w, "re:{line}").is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || d.serve(d_r, |_, _| None));
        std::thread::spawn(move || host.serve(h_r, |_, _| TcpStream::connect(echo_addr).ok().map(Into::into)));

        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let ours = TcpStream::connect(front.local_addr().unwrap()).unwrap();
        let (theirs, _) = front.accept().unwrap();
        daemon.attach(1, theirs, Some(channel::SSH_AGENT)).unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut c = BufReader::new(ours);
        for msg in ["one", "two"] {
            writeln!(c.get_mut(), "{msg}").unwrap();
            let mut line = String::new();
            c.read_line(&mut line).unwrap();
            assert_eq!(line, format!("re:{msg}\n"));
        }
    }

    #[test]
    fn refused_open_closes_the_client() {
        let daemon = pair();
        // Host only serves SSH_AGENT: any other channel is refused.
        let mut c = client(&daemon, 1, channel::HTTP_PROXY);
        let mut buf = String::new();
        assert_eq!(c.read_line(&mut buf).unwrap(), 0, "EOF after refusal");
        assert!(daemon.streams.lock().unwrap().is_empty());
    }

    #[test]
    fn client_close_is_forwarded_and_unregistered() {
        let daemon = pair();
        let c = client(&daemon, 1, channel::SSH_AGENT);
        drop(c);
        for _ in 0..100 {
            if daemon.streams.lock().unwrap().is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("stream still registered after client closed");
    }

    #[test]
    fn peer_loss_shuts_every_stream() {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (_h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let mut c = client(&daemon, 1, channel::SSH_AGENT);
        drop(h_w); // host went away
        daemon.serve(d_r, |_, _| None);
        let mut buf = String::new();
        assert_eq!(c.read_line(&mut buf).unwrap(), 0);
    }

    /// A `Write` that appends to a shared buffer, so a test can read back the
    /// frames a `Mux` emitted on its `out` side.
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Drive a host-side `Mux::serve` with `opens`, always granting a live
    /// socket, and return the stream ids it refused with `Close`.
    fn refused_opens(opens: &[u32]) -> Vec<u32> {
        let mut wire = Vec::new();
        for &stream in opens {
            proto::write_frame(&mut wire, &Frame::Open { stream, channel: channel::SSH_AGENT }).unwrap();
        }
        let out = Arc::new(Mutex::new(Vec::new()));
        let mux = Mux::new(Sink(Arc::clone(&out)));
        // Grant every Open a fresh socket; the policy inside `serve` decides.
        // Retain each peer end so an accepted stream's pump never sees EOF and
        // emits its own `Close` — only the policy's refusals reach `out`.
        let peers = Mutex::new(Vec::new());
        mux.serve(io::Cursor::new(wire), |_, _| {
            let (ours, theirs) = UnixStream::pair().unwrap();
            peers.lock().unwrap().push(theirs);
            Some(ours.into())
        });
        let mut r = io::Cursor::new(std::mem::take(&mut *out.lock().unwrap()));
        let mut closed = Vec::new();
        while let Ok(Some(frame)) = proto::read_frame(&mut r) {
            if let Frame::Close { stream, .. } = frame {
                closed.push(stream);
            }
        }
        closed
    }

    #[test]
    fn is_dead_fires_only_after_the_timeout() {
        let timeout = Duration::from_secs(45);
        assert!(!is_dead(Duration::from_secs(0), timeout));
        assert!(!is_dead(Duration::from_secs(44), timeout));
        // The window is inclusive at the boundary.
        assert!(is_dead(Duration::from_secs(45), timeout));
        assert!(is_dead(Duration::from_secs(90), timeout));
    }

    #[test]
    fn keepalive_calls_on_dead_when_peer_stops_answering() {
        // A live peer that reads our frames but never replies: inbound stays
        // silent, so `on_dead` fires once the tiny timeout elapses.
        let (mux_r, peer_w) = std::io::pipe().unwrap();
        let (peer_r, mux_w) = std::io::pipe().unwrap();
        let mux = Mux::new(mux_w);
        // Drain the peer's read end so our `Ping`s never block on a full pipe.
        std::thread::spawn(move || {
            let mut sink = peer_r;
            let mut buf = [0u8; 256];
            while let Ok(n) = sink.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        mux.keepalive(Duration::from_millis(5), Duration::from_millis(30), move || {
            let _ = tx.send(());
        });
        let m = Arc::clone(&mux);
        std::thread::spawn(move || m.serve(mux_r, |_, _| None));
        rx.recv_timeout(Duration::from_secs(2)).expect("on_dead fired");
        // Keep the peer's write end alive until on_dead so `serve` doesn't end
        // (and set `ended`) before liveness is judged.
        drop(peer_w);
    }

    #[test]
    fn keepalive_stops_once_serve_returns_without_calling_on_dead() {
        let (mux_r, peer_w) = std::io::pipe().unwrap();
        let (_peer_r, mux_w) = std::io::pipe().unwrap();
        let mux = Mux::new(mux_w);
        let fired = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&fired);
        mux.keepalive(Duration::from_millis(5), Duration::from_secs(30), move || {
            f.store(true, Ordering::Relaxed);
        });
        // Peer vanishes: `serve` returns and flips `ended`.
        drop(peer_w);
        mux.serve(mux_r, |_, _| None);
        std::thread::sleep(Duration::from_millis(40));
        assert!(!fired.load(Ordering::Relaxed), "on_dead must not fire after serve ended");
    }

    #[test]
    fn open_with_high_bit_is_refused() {
        assert_eq!(refused_opens(&[HOST_ID_BIT, HOST_ID_BIT | 5]), vec![HOST_ID_BIT, HOST_ID_BIT | 5]);
    }

    #[test]
    fn duplicate_open_id_is_refused() {
        // First Open for id 1 is accepted (no Close), the second refused.
        assert_eq!(refused_opens(&[1, 1]), vec![1]);
    }

    #[test]
    fn opens_past_the_stream_cap_are_refused() {
        // 1..=MAX_STREAMS accepted, then two more refused.
        let opens: Vec<u32> = (1..=(MAX_STREAMS as u32 + 2)).collect();
        let over = vec![MAX_STREAMS as u32 + 1, MAX_STREAMS as u32 + 2];
        assert_eq!(refused_opens(&opens), over);
    }
}
