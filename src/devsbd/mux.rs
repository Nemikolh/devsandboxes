//! Stream multiplexer over one frame connection, shared by the helper daemon
//! and the host bridge driver (devsbd includes it via `#[path]`, like
//! `proto.rs`). Both ends run the same routing so their close semantics can't
//! drift: whoever sees a local stream end first sends `Close`; receiving
//! `Close` shuts the local socket down without echoing one back.
//!
//! Local ends are a [`Conn`]: a unix socket (ssh-agent streams) or a TCP
//! socket (forwarded ports, docs/port-forwarding.md). Unix-only: the Windows
//! host will need a named-pipe stream here (docs/sandbox-helper.md).
//!
//! Two kinds of stream share the connection:
//! - **Legacy** (`Open`, ssh-agent, notify): `Data` is written straight into the
//!   local socket on the frame-reader thread, and local EOF sends `Close`. Small
//!   request/response traffic, kept as it was, plus one addition: a peer `Eof`
//!   `shutdown(Write)`s the local socket, so a sender that writes its request
//!   with `Mux::send` and then `Eof` (the daemon's notify flush) still gets the
//!   reply. Never sent on agent streams, so their behavior is unchanged.
//! - **Flow-controlled** (`Connect`, forwarded TCP): per-direction credit
//!   (`Window`), a per-stream write queue drained by its own thread so the
//!   reader never blocks, and half-close (`Eof`). One slow local reader can't
//!   stall the other streams or starve the keepalive.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::proto::{self, Frame, INITIAL_WINDOW};

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

/// Cap on live host-initiated (`Connect`) streams, enforced by the receiving
/// side. Separate from `MAX_STREAMS` and higher: a browser behind one forward
/// opens many connections at once.
const MAX_HOST_STREAMS: usize = 256;

/// Stream-id spaces: the daemon allocates ids with the high bit clear
/// (1..2^31), the host with it set (`Mux::connect`). The host refuses a peer
/// `Open` whose id has this bit set, the daemon a `Connect` whose id doesn't
/// (docs/sandbox-helper.md, _Frame protocol_).
const HOST_ID_BIT: u32 = 1 << 31;

/// Reason sent with the `Close` for a peer that overran its credit.
const FLOW_VIOLATION: &str = "flow control violation";

/// Pure grant rule: return credit once half the window has been consumed, so
/// a bulk transfer costs one `Window` per half-window, not one per chunk, and
/// the sender still has half a window in flight while the grant travels.
fn should_grant(ungranted: u32) -> bool {
    ungranted >= INITIAL_WINDOW / 2
}

/// Pure admission check for an inbound `Data` of `len` bytes on a
/// flow-controlled stream. `Err` is the `Close` reason: the peer sent more
/// than we granted (the queue would no longer be bounded by the window), or
/// sent data after its own `Eof`.
fn check_data(len: usize, recv_credit: u32, eof_received: bool) -> Result<(), &'static str> {
    if eof_received {
        return Err("data after eof");
    }
    if len > recv_credit as usize {
        return Err(FLOW_VIOLATION);
    }
    Ok(())
}

/// Pure teardown rule for a half-closed stream: done once we've sent our
/// `Eof`, received the peer's, and the writer has drained the queue into the
/// local socket and shut its write side. Neither side sends `Close` then;
/// both reach this state independently.
fn teardown_ready(eof_sent: bool, eof_received: bool, drained: bool) -> bool {
    eof_sent && eof_received && drained
}

/// Pure id policy for an inbound `Connect`: host id space (high bit set), not
/// already live, and under the host-stream cap.
fn connect_allowed(stream: u32, live: bool, live_host_streams: usize) -> bool {
    stream & HOST_ID_BIT != 0 && !live && live_host_streams < MAX_HOST_STREAMS
}

/// Next host-allocated id: `HOST_ID_BIT | n`, `n` counting up monotonically
/// through 1..2^31 and wrapping back to 1, skipping live ids. Monotonic rather
/// than lowest-free because right after a clean both-`Eof` teardown the peer
/// may not have processed our last frame for that id yet; reusing it at once
/// could graft stale frames onto a new stream.
fn next_host_id(next: &mut u32, is_live: impl Fn(u32) -> bool) -> u32 {
    loop {
        let n = *next;
        *next = if n + 1 >= HOST_ID_BIT { 1 } else { n + 1 };
        let id = HOST_ID_BIT | n;
        if !is_live(id) {
            return id;
        }
    }
}

/// Validate a `Connect` target before it hits the wire: the frame encodes the
/// host length as one byte, and the peer rejects an empty host or port 0 as
/// malformed (which ends the whole connection, not just the stream).
fn validate_connect_target(host: &str, port: u16) -> io::Result<()> {
    let bad = |msg: &str| Err(io::Error::new(io::ErrorKind::InvalidInput, msg.to_string()));
    if host.is_empty() {
        return bad("connect: empty host");
    }
    if host.len() > u8::MAX as usize {
        return bad("connect: host longer than 255 bytes");
    }
    if port == 0 {
        return bad("connect: port 0");
    }
    Ok(())
}

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

/// Called once when a host-initiated stream ends, with the reason: the peer's
/// `Close` reason (e.g. `connection refused`), our own local error, `"bridge
/// closed"` when the connection went away, or empty for a clean close. Runs on
/// a mux thread, possibly the frame reader: keep it quick (e.g. send on a
/// channel), or every stream on the connection waits.
pub type OnClose = Box<dyn FnOnce(String) + Send>;

/// A registered stream.
enum Entry {
    /// `Open` stream (ssh-agent): the original unbuffered path.
    Legacy(Arc<Conn>),
    /// `Connect` stream: credit, write queue, half-close.
    Flow(Arc<Flow>),
}

impl Entry {
    fn end(self, reason: String) {
        match self {
            Entry::Legacy(conn) => {
                let _ = conn.shutdown(Shutdown::Both);
            }
            Entry::Flow(flow) => flow.end(reason),
        }
    }
}

/// Shared state of one flow-controlled stream. One condvar serves both waiters
/// (the pump waiting for credit, the writer waiting for queued data); every
/// change is announced with `notify_all`, so neither can miss its wake-up.
struct Flow {
    state: Mutex<FlowState>,
    cv: Condvar,
}

struct FlowState {
    /// The local socket; `None` while the daemon's dial is still pending.
    conn: Option<Arc<Conn>>,
    /// Bytes we may still send (granted by the peer's `Window`s).
    send_credit: u32,
    /// Bytes the peer may still send us; mirrors the peer's own credit, so an
    /// inbound `Data` beyond it is an overrun. Bounds `queue` by construction.
    recv_credit: u32,
    /// Inbound `Data` waiting for the writer thread.
    queue: VecDeque<Vec<u8>>,
    /// Bytes written to the local socket but not yet granted back.
    ungranted: u32,
    eof_sent: bool,
    eof_received: bool,
    /// The writer drained the queue after the peer's `Eof` and shut the local
    /// write side down.
    drained: bool,
    /// Torn down: threads exit, late events are ignored.
    closed: bool,
    on_close: Option<OnClose>,
}

impl Flow {
    fn new(conn: Option<Arc<Conn>>, on_close: Option<OnClose>) -> Arc<Flow> {
        Arc::new(Flow {
            state: Mutex::new(FlowState {
                conn,
                send_credit: INITIAL_WINDOW,
                recv_credit: INITIAL_WINDOW,
                queue: VecDeque::new(),
                ungranted: 0,
                eof_sent: false,
                eof_received: false,
                drained: false,
                closed: false,
                on_close,
            }),
            cv: Condvar::new(),
        })
    }

    /// Mark closed (once), wake both threads, shut the local socket (which
    /// unblocks a pending read or write) and report `reason`. Callers
    /// unregister the stream first; nothing here touches the stream map.
    fn end(&self, reason: String) {
        let (conn, on_close) = {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return;
            }
            st.closed = true;
            st.queue.clear();
            (st.conn.clone(), st.on_close.take())
        };
        self.cv.notify_all();
        if let Some(conn) = conn {
            let _ = conn.shutdown(Shutdown::Both);
        }
        if let Some(f) = on_close {
            f(reason);
        }
    }
}

/// Answer to an inbound `Connect`, handed to `serve_with`'s `on_connect`.
/// `finish(Ok(conn))` attaches the dialed socket, `finish(Err(reason))` refuses
/// the stream with `Close { reason }`. Dropping it unfinished refuses too, so a
/// forgotten reply can't leave a stream pending forever.
pub struct ConnectReply {
    mux: Arc<Mux>,
    stream: u32,
    flow: Option<Arc<Flow>>,
}

impl ConnectReply {
    pub fn finish(mut self, result: Result<Conn, String>) {
        let flow = self.flow.take().expect("finish runs once");
        self.mux.complete_connect(self.stream, &flow, result);
    }
}

impl Drop for ConnectReply {
    fn drop(&mut self) {
        if let Some(flow) = self.flow.take() {
            self.mux.complete_connect(self.stream, &flow, Err("dial abandoned".into()));
        }
    }
}

/// Control frames queued by the frame reader for the control writer thread.
struct Ctl {
    queue: VecDeque<Frame>,
    /// A writer thread is running (spawned on demand, exits once the mux has
    /// ended and the queue is empty).
    writer: bool,
}

pub struct Mux {
    out: Mutex<Box<dyn Write + Send>>,
    /// Frames the reader must send without touching `out` (see `send_ctl`).
    ctl: Mutex<Ctl>,
    ctl_cv: Condvar,
    // Entries hold `Arc`s so the frame reader can clone a handle and drop the
    // guard before touching the stream, instead of holding the map lock.
    streams: Mutex<HashMap<u32, Entry>>,
    /// Counter behind `next_host_id`; only read/written under `streams`.
    next_host: AtomicU32,
    /// When `serve` last saw any inbound frame; the keepalive thread reads it
    /// to decide liveness. Initialised at construction so a peer that never
    /// sends a frame is still declared dead after `KEEPALIVE_TIMEOUT`.
    last_inbound: Mutex<Instant>,
    /// Set when `serve` returns, so the keepalive thread stops instead of
    /// leaking (and pinging a dead connection) once the bridge is gone.
    ended: AtomicBool,
    /// The peer's capabilities, learned from its `Caps` frame (docs/
    /// port-forwarding.md). `None` until one arrives: an older peer never sends
    /// it, so the daemon treats `None` as agent-capable. The host reads it to
    /// know the daemon's caps before opening a forward stream.
    peer_caps: Mutex<Option<u32>>,
    /// Called once with the peer's caps when a `Caps` frame arrives, so the
    /// daemon can wake agent clients held while no agent-capable bridge existed
    /// (a bridge becomes capable only after it advertises `SSH_AGENT`).
    on_caps: Mutex<Option<Box<dyn FnMut(u32) + Send>>>,
}

impl Mux {
    pub fn new(out: impl Write + Send + 'static) -> Arc<Mux> {
        Arc::new(Mux {
            out: Mutex::new(Box::new(out)),
            ctl: Mutex::new(Ctl { queue: VecDeque::new(), writer: false }),
            ctl_cv: Condvar::new(),
            streams: Mutex::new(HashMap::new()),
            next_host: AtomicU32::new(1),
            last_inbound: Mutex::new(Instant::now()),
            ended: AtomicBool::new(false),
            peer_caps: Mutex::new(None),
            on_caps: Mutex::new(None),
        })
    }

    /// The peer's advertised capabilities, or `None` if it never sent `Caps`
    /// (an older peer). The host reads the daemon's caps here after the
    /// handshake to decide whether a forward is possible.
    #[allow(dead_code)] // daemon routing (devsbd); host reads via Bridge (step 5)
    pub fn peer_caps(&self) -> Option<u32> {
        *self.peer_caps.lock().unwrap()
    }

    /// Register a callback run once with the peer's caps when its `Caps` frame
    /// arrives during `serve_with`. The daemon uses it to wake agent clients
    /// held while no agent-capable bridge existed.
    #[allow(dead_code)] // used by the daemon (devsbd crate) only
    pub fn on_caps(&self, f: impl FnMut(u32) + Send + 'static) {
        *self.on_caps.lock().unwrap() = Some(Box::new(f));
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

    /// Send `frame` without blocking: queue it for the control writer thread.
    /// The frame reader must never wait on `out`: a pump can hold it while
    /// blocked on a full pipe that only the peer's reader drains, and if the
    /// peer's reader is in the same spot, neither pipe drains again. Any frame
    /// sent from the reader (or from code it calls) goes through here. Identical
    /// queued `Pong`s are coalesced, so a peer flooding `Ping`s while not
    /// reading can't grow the queue without bound.
    fn send_ctl(self: &Arc<Self>, frame: Frame) {
        let mut ctl = self.ctl.lock().unwrap();
        if matches!(frame, Frame::Pong(_)) && ctl.queue.contains(&frame) {
            return;
        }
        ctl.queue.push_back(frame);
        if !ctl.writer {
            ctl.writer = true;
            let mux = Arc::clone(self);
            std::thread::spawn(move || mux.ctl_writer());
        }
        drop(ctl);
        self.ctl_cv.notify_all();
    }

    /// Drain the control queue into `out`. Exits once `serve` has ended and the
    /// queue is empty (frames queued before the end still go out, best effort),
    /// so it never outlives the connection except while stuck in a write, like
    /// any other sender.
    fn ctl_writer(self: Arc<Self>) {
        loop {
            let frame = {
                let mut ctl = self.ctl.lock().unwrap();
                loop {
                    if let Some(frame) = ctl.queue.pop_front() {
                        break frame;
                    }
                    if self.ended.load(Ordering::Relaxed) {
                        ctl.writer = false;
                        return;
                    }
                    ctl = self.ctl_cv.wait(ctl).unwrap();
                }
            };
            let _ = self.send(&frame);
        }
    }

    /// Register `conn` as `stream` and pump its reads into `Data` frames on a
    /// new thread, sending `Close` when it ends (unless the peer closed it
    /// first). With `open`, announces the stream with `Open { channel }` before
    /// any data can flow.
    pub fn attach(self: &Arc<Self>, stream: u32, conn: impl Into<Conn>, open: Option<u8>) -> io::Result<()> {
        let conn = conn.into();
        let reader = conn.try_clone()?;
        self.streams.lock().unwrap().insert(stream, Entry::Legacy(Arc::new(conn)));
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
        let entry = self.streams.lock().unwrap().remove(&stream);
        match entry {
            Some(entry) => {
                entry.end(String::new());
                true
            }
            None => false,
        }
    }

    /// Open a flow-controlled stream to `host:port` on the peer's side (the
    /// daemon dials it inside the container) and relay `conn` over it. Returns
    /// the allocated host id. `on_close` runs once when the stream ends, with
    /// the reason (see [`OnClose`]); it isn't called when this returns `Err`.
    #[allow(dead_code)] // host forwarder (docs/port-forwarding.md, steps 4-5)
    pub fn connect(
        self: &Arc<Self>,
        conn: impl Into<Conn>,
        host: &str,
        port: u16,
        on_close: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<u32> {
        validate_connect_target(host, port)?;
        let conn = conn.into();
        let reader = conn.try_clone()?;
        let flow = Flow::new(None, Some(Box::new(on_close)));
        let stream = {
            let mut live = self.streams.lock().unwrap();
            let mut next = self.next_host.load(Ordering::Relaxed);
            let id = next_host_id(&mut next, |id| live.contains_key(&id));
            self.next_host.store(next, Ordering::Relaxed);
            live.insert(id, Entry::Flow(Arc::clone(&flow)));
            id
        };
        // Registered before `Connect` goes out, so the peer's answer (Data,
        // Window, Close) always finds the stream; threads start after it, so
        // no `Data` can precede the `Connect`.
        if let Err(e) = self.send(&Frame::Connect { stream, host: host.to_string(), port }) {
            self.unregister(stream, &flow);
            return Err(e);
        }
        self.start_flow(stream, &flow, conn, reader);
        Ok(stream)
    }

    /// Settle a pending inbound `Connect`. A stream that was closed (or whose
    /// id was replaced) while the dial ran is no longer registered: the late
    /// socket is dropped silently, the peer already knows the stream is gone.
    fn complete_connect(self: &Arc<Self>, stream: u32, flow: &Arc<Flow>, result: Result<Conn, String>) {
        match result.and_then(|conn| {
            let reader = conn.try_clone().map_err(|e| format!("clone socket: {e}"))?;
            Ok((conn, reader))
        }) {
            Ok((conn, reader)) => self.start_flow(stream, flow, conn, reader),
            Err(reason) => self.close_flow(stream, flow, reason),
        }
    }

    /// Attach the local socket to a registered flow stream and start its
    /// writer and pump. Checked under the flow lock against `closed`, so a
    /// `Close` racing the attach either sees the socket (and shuts it) or makes
    /// us drop it here — never a socket with no owner.
    fn start_flow(self: &Arc<Self>, stream: u32, flow: &Arc<Flow>, conn: Conn, reader: Conn) {
        let conn = Arc::new(conn);
        {
            let mut st = flow.state.lock().unwrap();
            if st.closed {
                return;
            }
            st.conn = Some(Arc::clone(&conn));
        }
        // Data queued before the attach needs no wake-up: the writer checks
        // the queue before its first wait.
        let (mux, f) = (Arc::clone(self), Arc::clone(flow));
        std::thread::spawn(move || mux.flow_writer(stream, f, conn));
        let (mux, f) = (Arc::clone(self), Arc::clone(flow));
        std::thread::spawn(move || mux.flow_pump(stream, f, reader));
    }

    /// Local socket -> `Data`, never more than the peer granted: reads at most
    /// `min(CHUNK, credit)` and parks on the condvar at zero credit. Local EOF
    /// sends `Eof` (after all our `Data`, same thread) and stops this
    /// direction only; the other keeps flowing until the peer's `Eof`.
    fn flow_pump(self: Arc<Self>, stream: u32, flow: Arc<Flow>, mut conn: Conn) {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let want = {
                let mut st = flow.state.lock().unwrap();
                loop {
                    if st.closed {
                        return;
                    }
                    if st.send_credit > 0 {
                        break (st.send_credit as usize).min(CHUNK);
                    }
                    st = flow.cv.wait(st).unwrap();
                }
            };
            match conn.read(&mut buf[..want]) {
                Ok(0) => {
                    if flow.state.lock().unwrap().closed || self.send(&Frame::Eof { stream }).is_err() {
                        return;
                    }
                    let ready = {
                        let mut st = flow.state.lock().unwrap();
                        st.eof_sent = true;
                        !st.closed && teardown_ready(st.eof_sent, st.eof_received, st.drained)
                    };
                    if ready {
                        self.finish_flow(stream, &flow);
                    }
                    return;
                }
                Ok(n) => {
                    {
                        let mut st = flow.state.lock().unwrap();
                        if st.closed {
                            return;
                        }
                        // Only this thread spends credit, and `n <= want <= credit`.
                        st.send_credit -= n as u32;
                    }
                    if self.send(&Frame::Data { stream, bytes: buf[..n].to_vec() }).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.close_flow(stream, &flow, format!("read: {e}"));
                    return;
                }
            }
        }
    }

    /// Queue -> local socket, off the frame-reader thread: a slow local reader
    /// only backs up this stream's queue (bounded by the window). Credit goes
    /// back once bytes are actually written, batched by `should_grant`. After
    /// the peer's `Eof` and an empty queue, shuts the local write side down.
    fn flow_writer(self: Arc<Self>, stream: u32, flow: Arc<Flow>, conn: Arc<Conn>) {
        loop {
            let next = {
                let mut st = flow.state.lock().unwrap();
                loop {
                    if st.closed {
                        return;
                    }
                    if let Some(bytes) = st.queue.pop_front() {
                        break Some(bytes);
                    }
                    if st.eof_received {
                        break None;
                    }
                    st = flow.cv.wait(st).unwrap();
                }
            };
            let Some(bytes) = next else {
                let _ = conn.shutdown(Shutdown::Write);
                let ready = {
                    let mut st = flow.state.lock().unwrap();
                    st.drained = true;
                    !st.closed && teardown_ready(st.eof_sent, st.eof_received, st.drained)
                };
                if ready {
                    self.finish_flow(stream, &flow);
                }
                return;
            };
            if let Err(e) = (&*conn).write_all(&bytes) {
                self.close_flow(stream, &flow, format!("write: {e}"));
                return;
            }
            let grant = {
                let mut st = flow.state.lock().unwrap();
                if st.closed {
                    return;
                }
                st.ungranted += bytes.len() as u32;
                // After the peer's `Eof` it sends nothing more: no point granting.
                (should_grant(st.ungranted) && !st.eof_received).then(|| {
                    let credit = std::mem::take(&mut st.ungranted);
                    st.recv_credit += credit;
                    credit
                })
            };
            if let Some(credit) = grant {
                let _ = self.send(&Frame::Window { stream, credit });
            }
        }
    }

    /// Remove `stream` from the map only if it still maps to `flow` (the id may
    /// have been closed and reused meanwhile).
    fn unregister(&self, stream: u32, flow: &Arc<Flow>) -> bool {
        let mut live = self.streams.lock().unwrap();
        match live.get(&stream) {
            Some(Entry::Flow(f)) if Arc::ptr_eq(f, flow) => {
                live.remove(&stream);
                true
            }
            _ => false,
        }
    }

    /// We end the stream: tear down and tell the peer why (`Close { reason }`).
    /// A no-op when it's already gone (peer `Close`, teardown, mux end). The
    /// `Close` goes through `send_ctl` because the reader calls this (credit
    /// overrun, an inline `ConnectReply`); order still holds for pump/writer
    /// callers, since every `Data` they sent was written before they got here.
    fn close_flow(self: &Arc<Self>, stream: u32, flow: &Arc<Flow>, reason: String) {
        if self.unregister(stream, flow) {
            flow.end(reason.clone());
            self.send_ctl(Frame::Close { stream, reason });
        }
    }

    /// Clean end after both `Eof`s: each side reaches this on its own, so no
    /// `Close` is sent.
    fn finish_flow(&self, stream: u32, flow: &Arc<Flow>) {
        if self.unregister(stream, flow) {
            flow.end(String::new());
        }
    }

    fn flow(&self, stream: u32) -> Option<Arc<Flow>> {
        match self.streams.lock().unwrap().get(&stream) {
            Some(Entry::Flow(f)) => Some(Arc::clone(f)),
            _ => None,
        }
    }

    /// `serve_with` for a peer that never accepts `Connect` (the host side):
    /// each one is refused with `Close`.
    #[allow(dead_code)] // devsbd only uses `serve_with`
    pub fn serve(self: &Arc<Self>, reader: impl Read, on_open: impl Fn(u32, u8) -> Option<Conn>) {
        self.serve_with(reader, on_open, |_, _, _, reply: ConnectReply| {
            reply.finish(Err("port forwarding not accepted by this peer".into()))
        });
    }

    /// Route frames from `reader` until it ends: `Data` to its stream,
    /// `Close` shuts it, `Ping` gets a `Pong`, and `Open` asks `on_open` for a
    /// local connection (`None` refuses the stream with `Close`). `Connect`
    /// registers a pending flow stream (so `Data` racing the dial is queued,
    /// bounded by credit) and hands a [`ConnectReply`] to `on_connect`, which
    /// must return at once and dial elsewhere. This thread never blocks on a
    /// flow stream's local socket, nor on `out` (its replies go through
    /// `send_ctl`), which keeps the pipe draining and `last_inbound` truthful
    /// for the keepalive. It still runs `on_open`, `on_connect` and `on_close`
    /// callbacks inline: they must be quick. Every stream is shut down on
    /// return, since the peer can no longer see it.
    pub fn serve_with(
        self: &Arc<Self>,
        mut reader: impl Read,
        on_open: impl Fn(u32, u8) -> Option<Conn>,
        on_connect: impl Fn(u32, String, u16, ConnectReply),
    ) {
        while let Ok(Some(frame)) = proto::read_frame(&mut reader) {
            *self.last_inbound.lock().unwrap() = Instant::now();
            match frame {
                Frame::Data { stream, bytes } => {
                    let conn = match self.streams.lock().unwrap().get(&stream) {
                        Some(Entry::Legacy(conn)) => Ok(Arc::clone(conn)),
                        Some(Entry::Flow(flow)) => Err(Arc::clone(flow)),
                        // Raced a local close; its `Close` is already on the way.
                        None => continue,
                    };
                    match conn {
                        // Legacy: write inline, after dropping the guard, so a
                        // slow local socket can't stall other streams' routing.
                        Ok(conn) => {
                            if (&*conn).write_all(&bytes).is_err() && self.drop_stream(stream) {
                                self.send_ctl(Frame::Close { stream, reason: String::new() });
                            }
                        }
                        Err(flow) => self.queue_data(stream, &flow, bytes),
                    }
                }
                Frame::Window { stream, credit } => {
                    if let Some(flow) = self.flow(stream) {
                        let mut st = flow.state.lock().unwrap();
                        st.send_credit = st.send_credit.saturating_add(credit);
                        drop(st);
                        flow.cv.notify_all();
                    }
                }
                Frame::Eof { stream } => {
                    let entry = match self.streams.lock().unwrap().get(&stream) {
                        Some(Entry::Legacy(conn)) => Ok(Arc::clone(conn)),
                        Some(Entry::Flow(flow)) => Err(Arc::clone(flow)),
                        None => continue,
                    };
                    match entry {
                        // Legacy half-close: the request is complete, the
                        // reply direction stays open (see the module doc).
                        Ok(conn) => {
                            let _ = conn.shutdown(Shutdown::Write);
                        }
                        Err(flow) => {
                            flow.state.lock().unwrap().eof_received = true;
                            flow.cv.notify_all();
                        }
                    }
                }
                Frame::Close { stream, reason } => {
                    let entry = self.streams.lock().unwrap().remove(&stream);
                    if let Some(entry) = entry {
                        entry.end(reason);
                    }
                }
                Frame::Ping(payload) => self.send_ctl(Frame::Pong(payload)),
                Frame::Open { stream, channel } => {
                    // Host-side policy for peer-allocated stream ids. The
                    // high bit is reserved for host-allocated ids, so a peer
                    // `Open` must have it clear; refuse a duplicate id or one
                    // past the stream cap (counting peer-allocated streams
                    // only, so forwarded connections can't crowd out the
                    // agent). Sending `Close` for a duplicate makes a
                    // misbehaving peer drop its *existing* stream with that
                    // id — acceptable, since only a broken or hostile peer
                    // reuses a live id. The daemon side passes
                    // `on_open = |_,_| None` and never reaches the accept path.
                    let attached = {
                        let live = self.streams.lock().unwrap();
                        let peer_streams = live.keys().filter(|&&id| id & HOST_ID_BIT == 0).count();
                        let allowed = stream & HOST_ID_BIT == 0
                            && !live.contains_key(&stream)
                            && peer_streams < MAX_STREAMS;
                        drop(live);
                        allowed
                            && match on_open(stream, channel) {
                                Some(conn) => self.attach(stream, conn, None).is_ok(),
                                None => false,
                            }
                    };
                    if !attached {
                        self.send_ctl(Frame::Close { stream, reason: String::new() });
                    }
                }
                Frame::Connect { stream, host, port } => {
                    // Same duplicate caveat as `Open`: refusing a live id makes
                    // the peer drop its own stream with it.
                    let flow = {
                        let mut live = self.streams.lock().unwrap();
                        let host_streams = live.keys().filter(|&&id| id & HOST_ID_BIT != 0).count();
                        connect_allowed(stream, live.contains_key(&stream), host_streams).then(|| {
                            let flow = Flow::new(None, None);
                            live.insert(stream, Entry::Flow(Arc::clone(&flow)));
                            flow
                        })
                    };
                    match flow {
                        Some(flow) => {
                            on_connect(stream, host, port, ConnectReply { mux: Arc::clone(self), stream, flow: Some(flow) })
                        }
                        None => self.send_ctl(Frame::Close { stream, reason: "stream refused".into() }),
                    }
                }
                // The peer's capabilities (docs/port-forwarding.md): record them
                // and wake anything waiting on them (the daemon's held agent
                // clients). Arrives once on stream 0 right after the handshake.
                Frame::Caps(c) => {
                    *self.peer_caps.lock().unwrap() = Some(c);
                    if let Some(f) = self.on_caps.lock().unwrap().as_mut() {
                        f(c);
                    }
                }
                // `Quit` only means something as a control socket's first
                // frame, which the daemon reads before handing over to `serve`.
                Frame::Hello { .. } | Frame::Pong(_) | Frame::Quit => {}
            }
        }
        self.ended.store(true, Ordering::Relaxed);
        // Taking the lock orders this after a writer's `ended` check, so its
        // wait can't miss the wake-up.
        drop(self.ctl.lock().unwrap());
        self.ctl_cv.notify_all();
        self.close_all();
    }

    /// Enqueue inbound `Data` for a flow stream's writer, or close the stream
    /// if the peer overran its credit. Never blocks: the queue is bounded by
    /// the credit we granted.
    fn queue_data(self: &Arc<Self>, stream: u32, flow: &Arc<Flow>, bytes: Vec<u8>) {
        let verdict = {
            let mut st = flow.state.lock().unwrap();
            if st.closed {
                return;
            }
            let verdict = check_data(bytes.len(), st.recv_credit, st.eof_received);
            if verdict.is_ok() {
                st.recv_credit -= bytes.len() as u32;
                st.queue.push_back(bytes);
            }
            verdict
        };
        match verdict {
            Ok(()) => flow.cv.notify_all(),
            Err(reason) => self.close_flow(stream, flow, reason.to_string()),
        }
    }

    /// Tear down every stream (the connection is gone): shuts local sockets,
    /// wakes and stops every flow thread, reports `"bridge closed"`.
    pub fn close_all(&self) {
        let entries: Vec<Entry> = self.streams.lock().unwrap().drain().map(|(_, e)| e).collect();
        for entry in entries {
            entry.end("bridge closed".into());
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

    /// Legacy half-close: a request sent as `Data` + `Eof` reaches the host's
    /// local end as bytes then EOF, and the reply still flows back — the notify
    /// exchange (the local-EOF path would send `Close` and lose the reply).
    #[test]
    fn eof_half_closes_a_legacy_stream_and_the_reply_flows_back() {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || d.serve(d_r, |_, _| None));
        std::thread::spawn(move || {
            host.serve(h_r, |_, _| {
                let (ours, mut handler) = UnixStream::pair().unwrap();
                std::thread::spawn(move || {
                    let mut req = Vec::new();
                    handler.read_to_end(&mut req).unwrap();
                    handler.write_all(format!("got {}", req.len()).as_bytes()).unwrap();
                });
                Some(ours.into())
            })
        });
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        daemon.attach(1, theirs, Some(channel::NOTIFY)).unwrap();
        daemon.send(&Frame::Data { stream: 1, bytes: b"hello".to_vec() }).unwrap();
        daemon.send(&Frame::Eof { stream: 1 }).unwrap();
        let mut reply = String::new();
        ours.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "got 5");
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

    /// Reader-sent frames go out on the control writer thread; once `serve`
    /// has returned, wait for it to flush them and exit before inspecting `out`.
    fn wait_ctl_drained(mux: &Mux) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ctl = mux.ctl.lock().unwrap();
            if ctl.queue.is_empty() && !ctl.writer {
                return;
            }
            drop(ctl);
            assert!(Instant::now() < deadline, "control writer never drained");
            std::thread::sleep(Duration::from_millis(1));
        }
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
        wait_ctl_drained(&mux);
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

    // ---- flow-controlled (`Connect`) streams ----

    use std::sync::mpsc;

    #[test]
    fn grant_rule_batches_to_half_a_window() {
        assert!(!should_grant(0));
        assert!(!should_grant(INITIAL_WINDOW / 2 - 1));
        assert!(should_grant(INITIAL_WINDOW / 2));
        assert!(should_grant(INITIAL_WINDOW));
    }

    #[test]
    fn data_admission_enforces_credit_and_eof() {
        assert_eq!(check_data(10, 10, false), Ok(()), "exactly the credit is fine");
        assert_eq!(check_data(0, 0, false), Ok(()));
        assert_eq!(check_data(11, 10, false), Err(FLOW_VIOLATION));
        assert_eq!(check_data(1, 10, true), Err("data after eof"));
    }

    #[test]
    fn teardown_needs_both_eofs_and_a_drained_queue() {
        assert!(teardown_ready(true, true, true));
        for (sent, received, drained) in [(false, true, true), (true, false, true), (true, true, false), (false, false, false)] {
            assert!(!teardown_ready(sent, received, drained), "{sent} {received} {drained}");
        }
    }

    #[test]
    fn connect_id_rule() {
        assert!(connect_allowed(HOST_ID_BIT | 1, false, 0));
        assert!(!connect_allowed(1, false, 0), "daemon id space");
        assert!(!connect_allowed(HOST_ID_BIT | 1, true, 0), "live id");
        assert!(connect_allowed(HOST_ID_BIT | 1, false, MAX_HOST_STREAMS - 1));
        assert!(!connect_allowed(HOST_ID_BIT | 1, false, MAX_HOST_STREAMS), "cap");
    }

    #[test]
    fn host_ids_are_monotonic_skip_live_and_wrap() {
        let mut next = 1;
        assert_eq!(next_host_id(&mut next, |_| false), HOST_ID_BIT | 1);
        assert_eq!(next_host_id(&mut next, |_| false), HOST_ID_BIT | 2);
        assert_eq!(next_host_id(&mut next, |id| id == HOST_ID_BIT | 3), HOST_ID_BIT | 4);
        // A freed id is not handed out again right away.
        assert_eq!(next_host_id(&mut next, |_| false), HOST_ID_BIT | 5);
        let mut next = HOST_ID_BIT - 1;
        assert_eq!(next_host_id(&mut next, |_| false), u32::MAX);
        assert_eq!(next_host_id(&mut next, |_| false), HOST_ID_BIT | 1, "wraps to 1, never HOST_ID_BIT | 0");
    }

    #[test]
    fn connect_rejects_bad_targets_before_the_wire() {
        let out = Arc::new(Mutex::new(Vec::new()));
        let mux = Mux::new(Sink(Arc::clone(&out)));
        let long = "h".repeat(256);
        for (host, port) in [("", 80), (long.as_str(), 80), ("db", 0)] {
            let (a, _b) = UnixStream::pair().unwrap();
            let err = mux.connect(a, host, port, |_| {}).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{host:?}:{port}");
        }
        assert!(out.lock().unwrap().is_empty());
        assert!(mux.streams.lock().unwrap().is_empty());
        // 255 bytes is the most one length byte can carry.
        let (a, _b) = UnixStream::pair().unwrap();
        mux.connect(a, &"h".repeat(255), 80, |_| {}).unwrap();
    }

    /// Host and daemon muxes over OS pipes; the daemon settles each `Connect`
    /// with `dial`, inline (standing in for the daemon's dial thread).
    fn flow_pair_with(dial: impl Fn(&str, u16) -> Result<Conn, String> + Send + 'static) -> (Arc<Mux>, Arc<Mux>) {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || d.serve_with(d_r, |_, _| None, move |_, h, p, reply| reply.finish(dial(&h, p))));
        let h = Arc::clone(&host);
        std::thread::spawn(move || h.serve(h_r, |_, _| None));
        (host, daemon)
    }

    struct Flows {
        host: Arc<Mux>,
        daemon: Arc<Mux>,
        /// The "container" end of each dialed stream, in `Connect` order.
        servers: mpsc::Receiver<UnixStream>,
    }

    fn flow_pair() -> Flows {
        let (tx, servers) = mpsc::channel();
        let (host, daemon) = flow_pair_with(move |_, _| {
            let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
            tx.send(theirs).unwrap();
            Ok(ours.into())
        });
        Flows { host, daemon, servers }
    }

    /// Forward one stream: (client end on the host, server end in the
    /// container, host id, the host's `on_close` reasons).
    fn open(f: &Flows) -> (UnixStream, UnixStream, u32, mpsc::Receiver<String>) {
        let (client, theirs) = UnixStream::pair().unwrap();
        let (tx, closed) = mpsc::channel();
        let id = f
            .host
            .connect(theirs, "svc", 80, move |reason| {
                let _ = tx.send(reason);
            })
            .unwrap();
        let server = f.servers.recv_timeout(Duration::from_secs(10)).unwrap();
        for s in [&client, &server] {
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
        }
        (client, server, id, closed)
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn both_unregistered(f: &Flows) -> bool {
        f.host.streams.lock().unwrap().is_empty() && f.daemon.streams.lock().unwrap().is_empty()
    }

    /// Deterministic, non-repeating-per-chunk byte pattern.
    fn pat(i: usize) -> u8 {
        (i ^ (i >> 8) ^ (i >> 17)).wrapping_mul(31) as u8
    }

    fn write_pattern(mut w: impl Write, n: usize) -> io::Result<()> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut off = 0;
        while off < n {
            let len = buf.len().min(n - off);
            for (j, b) in buf[..len].iter_mut().enumerate() {
                *b = pat(off + j);
            }
            w.write_all(&buf[..len])?;
            off += len;
        }
        Ok(())
    }

    /// Read to EOF, checking every byte against `pat`; returns the count.
    fn read_check(mut r: impl Read) -> usize {
        let mut buf = vec![0u8; 64 * 1024];
        let mut off = 0;
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                return off;
            }
            for (j, &b) in buf[..n].iter().enumerate() {
                assert_eq!(b, pat(off + j), "byte {} differs", off + j);
            }
            off += n;
        }
    }

    #[test]
    fn bulk_transfer_is_byte_identical_both_ways() {
        const N: usize = 64 << 20;
        let f = flow_pair();
        let (client, server, _, closed) = open(&f);
        let (cw, sw) = (client.try_clone().unwrap(), server.try_clone().unwrap());
        let up = std::thread::spawn(move || {
            write_pattern(&cw, N).unwrap();
            cw.shutdown(Shutdown::Write).unwrap();
        });
        let down = std::thread::spawn(move || {
            write_pattern(&sw, N).unwrap();
            sw.shutdown(Shutdown::Write).unwrap();
        });
        let received_up = std::thread::spawn(move || read_check(&server));
        assert_eq!(read_check(&client), N);
        assert_eq!(received_up.join().unwrap(), N);
        up.join().unwrap();
        down.join().unwrap();
        // Both directions hit EOF: a clean teardown on both sides, no Close.
        assert_eq!(closed.recv_timeout(Duration::from_secs(10)).unwrap(), "");
        wait_until("both sides to unregister", || both_unregistered(&f));
    }

    /// Regression: the frame reader used to write `Pong`/`Close` inline on
    /// `out`. With bulk traffic both ways, each side's pump could hold `out`
    /// while blocked on a full pipe that only the *other* side's reader drains,
    /// and that reader was itself waiting on its own `out` to send a `Pong`: a
    /// cycle. 1 ms pings on both sides make the collision near-certain; the
    /// keepalive timeout sits past the test bound, so a hang shows up as a
    /// timeout here rather than as `on_dead`.
    #[test]
    fn bidirectional_bulk_with_pings_does_not_deadlock() {
        const N: usize = 32 << 20;
        const STREAMS: usize = 8;
        let f = flow_pair();
        let fired = Arc::new(AtomicBool::new(false));
        for mux in [&f.host, &f.daemon] {
            let fired = Arc::clone(&fired);
            mux.keepalive(Duration::from_millis(1), Duration::from_secs(60), move || {
                fired.store(true, Ordering::Relaxed);
            });
        }
        let (done_tx, done) = mpsc::channel();
        for _ in 0..STREAMS {
            let (client, server, _, _) = open(&f);
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let (cw, sw) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                std::thread::spawn(move || {
                    let _ = write_pattern(&cw, N).and_then(|_| cw.shutdown(Shutdown::Write));
                });
                std::thread::spawn(move || {
                    let _ = write_pattern(&sw, N).and_then(|_| sw.shutdown(Shutdown::Write));
                });
                let up = std::thread::spawn(move || read_check(&server));
                let down = read_check(&client);
                let _ = done_tx.send((up.join().unwrap(), down));
            });
        }
        drop(done_tx);
        let deadline = Instant::now() + Duration::from_secs(20);
        for _ in 0..STREAMS {
            let left = deadline.saturating_duration_since(Instant::now());
            let got = done.recv_timeout(left).expect("bulk transfer wedged: mux deadlock");
            assert_eq!(got, (N, N));
        }
        assert!(!fired.load(Ordering::Relaxed), "keepalive declared a live peer dead");
    }

    #[test]
    fn half_close_still_delivers_the_response() {
        let f = flow_pair();
        let (mut client, mut server, _, closed) = open(&f);
        client.write_all(b"GET /").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut req = Vec::new();
        server.read_to_end(&mut req).unwrap();
        assert_eq!(req, b"GET /");
        // Bigger than the window + socket buffers, so it needs Window grants
        // flowing while the client's write side is already shut.
        let response: Vec<u8> = (0..1 << 20).map(pat).collect();
        let expected = response.clone();
        std::thread::spawn(move || {
            server.write_all(&response).unwrap();
            // Dropping closes our end: the daemon pump sees EOF.
        });
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert!(got == expected, "response differs ({} of {} bytes)", got.len(), expected.len());
        assert_eq!(closed.recv_timeout(Duration::from_secs(10)).unwrap(), "");
        wait_until("both sides to unregister", || both_unregistered(&f));
    }

    /// Stream A backed up (its server end never reads, and the host has spent
    /// all its credit), plus an idle stream B. Returns A's server end (keep it
    /// alive, unread) and B's two ends.
    fn stalled(f: &Flows) -> (UnixStream, UnixStream, UnixStream) {
        let (a_client, a_server, a_id, _) = open(f);
        // Blocks once everything between it and the unread server is full.
        std::thread::spawn(move || {
            let _ = write_pattern(&a_client, 64 << 20);
        });
        wait_until("A to exhaust its credit", || {
            f.host.flow(a_id).is_some_and(|fl| fl.state.lock().unwrap().send_credit == 0)
        });
        let (b_client, b_server, _, _) = open(f);
        (a_server, b_client, b_server)
    }

    fn echo(s: UnixStream) {
        std::thread::spawn(move || {
            let mut w = s.try_clone().unwrap();
            let _ = io::copy(&mut &s, &mut w);
        });
    }

    #[test]
    fn stalled_stream_does_not_delay_others() {
        let f = flow_pair();
        let (_a_server, b_client, b_server) = stalled(&f);
        echo(b_server);
        let mut b = BufReader::new(b_client);
        for i in 0..20 {
            let t = Instant::now();
            writeln!(b.get_mut(), "ping {i}").unwrap();
            let mut line = String::new();
            b.read_line(&mut line).unwrap();
            assert_eq!(line, format!("ping {i}\n"));
            assert!(t.elapsed() < Duration::from_secs(2), "B round-trip took {:?}", t.elapsed());
        }
    }

    #[test]
    fn keepalive_holds_while_a_stream_is_stalled() {
        let f = flow_pair();
        let fired = Arc::new(AtomicBool::new(false));
        for mux in [&f.host, &f.daemon] {
            let fired = Arc::clone(&fired);
            mux.keepalive(Duration::from_millis(5), Duration::from_millis(300), move || {
                fired.store(true, Ordering::Relaxed);
            });
        }
        let _ends = stalled(&f);
        std::thread::sleep(Duration::from_secs(1));
        assert!(!fired.load(Ordering::Relaxed), "keepalive declared a live peer dead");
    }

    #[test]
    fn dial_failure_reaches_the_host_with_its_reason() {
        let (host, _daemon) = flow_pair_with(|_, _| Err("connection refused".into()));
        let (mut client, theirs) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (tx, rx) = mpsc::channel();
        host.connect(theirs, "db", 5432, move |reason| {
            let _ = tx.send(reason);
        })
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "connection refused");
        let mut buf = [0u8; 1];
        assert_eq!(client.read(&mut buf).unwrap(), 0, "client sees EOF");
        assert!(host.streams.lock().unwrap().is_empty());
    }

    /// Plain `serve` (the host side) refuses every `Connect`, with a reason.
    #[test]
    fn plain_serve_refuses_connect() {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let (a, b) = (Mux::new(d_w), Mux::new(h_w));
        let a2 = Arc::clone(&a);
        std::thread::spawn(move || a2.serve(d_r, |_, _| None));
        std::thread::spawn(move || b.serve(h_r, |_, _| None));
        let (_client, theirs) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        a.connect(theirs, "svc", 80, move |reason| {
            let _ = tx.send(reason);
        })
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "port forwarding not accepted by this peer");
    }

    /// A daemon-side `serve_with` fed frames through a pipe, so the test
    /// controls timing; each `Connect`'s reply is handed to the test unsettled.
    fn pending_daemon() -> (Arc<Mux>, Arc<Mutex<Vec<u8>>>, std::io::PipeWriter, mpsc::Receiver<ConnectReply>) {
        let (r, w) = std::io::pipe().unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let mux = Mux::new(Sink(Arc::clone(&out)));
        let (tx, replies) = mpsc::channel();
        let m = Arc::clone(&mux);
        std::thread::spawn(move || m.serve_with(r, |_, _| None, move |_, _, _, reply| tx.send(reply).unwrap()));
        (mux, out, w, replies)
    }

    #[test]
    fn data_racing_the_dial_is_queued_and_delivered() {
        let (_mux, _out, mut w, replies) = pending_daemon();
        let id = HOST_ID_BIT | 1;
        proto::write_frame(&mut w, &Frame::Connect { stream: id, host: "svc".into(), port: 80 }).unwrap();
        proto::write_frame(&mut w, &Frame::Data { stream: id, bytes: b"early".to_vec() }).unwrap();
        proto::write_frame(&mut w, &Frame::Eof { stream: id }).unwrap();
        let reply = replies.recv_timeout(Duration::from_secs(10)).unwrap();
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        // Give serve time to queue the Data and Eof before the dial "completes".
        std::thread::sleep(Duration::from_millis(50));
        reply.finish(Ok(ours.into()));
        let mut got = Vec::new();
        theirs.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"early");
    }

    #[test]
    fn close_while_dialing_drops_the_late_socket() {
        let (mux, out, mut w, replies) = pending_daemon();
        let id = HOST_ID_BIT | 1;
        proto::write_frame(&mut w, &Frame::Connect { stream: id, host: "svc".into(), port: 80 }).unwrap();
        let reply = replies.recv_timeout(Duration::from_secs(10)).unwrap();
        proto::write_frame(&mut w, &Frame::Data { stream: id, bytes: b"early".to_vec() }).unwrap();
        proto::write_frame(&mut w, &Frame::Close { stream: id, reason: String::new() }).unwrap();
        wait_until("the pending stream to unregister", || mux.streams.lock().unwrap().is_empty());
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        reply.finish(Ok(ours.into()));
        let mut got = Vec::new();
        assert_eq!(theirs.read_to_end(&mut got).unwrap(), 0, "late socket dropped, nothing delivered");
        assert!(mux.streams.lock().unwrap().is_empty());
        assert!(out.lock().unwrap().is_empty(), "no Close echoed, nothing sent");
    }

    /// Feed `wire` to a daemon-side `serve_with` and return the frames it sent.
    /// With `dial`, every `Connect` gets a live socket (peer ends retained, so
    /// no pump sees EOF); without, replies stay pending until serve returns.
    fn daemon_frames(wire: Vec<u8>, dial: bool) -> Vec<Frame> {
        let out = Arc::new(Mutex::new(Vec::new()));
        let mux = Mux::new(Sink(Arc::clone(&out)));
        let peers = Mutex::new(Vec::new());
        let pending = Mutex::new(Vec::new());
        mux.serve_with(io::Cursor::new(wire), |_, _| None, |_, _, _, reply| {
            if dial {
                let (ours, theirs) = UnixStream::pair().unwrap();
                peers.lock().unwrap().push(theirs);
                reply.finish(Ok(ours.into()));
            } else {
                pending.lock().unwrap().push(reply);
            }
        });
        wait_ctl_drained(&mux);
        let mut r = io::Cursor::new(std::mem::take(&mut *out.lock().unwrap()));
        let mut frames = Vec::new();
        while let Ok(Some(frame)) = proto::read_frame(&mut r) {
            frames.push(frame);
        }
        frames
    }

    fn closes(frames: &[Frame]) -> Vec<(u32, String)> {
        frames
            .iter()
            .filter_map(|f| match f {
                Frame::Close { stream, reason } => Some((*stream, reason.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn credit_overrun_closes_only_that_stream() {
        let (a, b) = (HOST_ID_BIT | 1, HOST_ID_BIT | 2);
        let mut wire = Vec::new();
        for frame in [
            Frame::Connect { stream: a, host: "svc".into(), port: 80 },
            Frame::Connect { stream: b, host: "svc".into(), port: 80 },
            Frame::Data { stream: a, bytes: vec![0; INITIAL_WINDOW as usize + 1] },
            Frame::Data { stream: b, bytes: vec![0; 10] },
            Frame::Ping(b"alive".to_vec()),
        ] {
            proto::write_frame(&mut wire, &frame).unwrap();
        }
        let frames = daemon_frames(wire, true);
        assert_eq!(closes(&frames), [(a, FLOW_VIOLATION.to_string())]);
        assert!(frames.contains(&Frame::Pong(b"alive".to_vec())), "connection stays up");
    }

    fn refused_connects(ids: &[u32]) -> Vec<u32> {
        let mut wire = Vec::new();
        for &stream in ids {
            proto::write_frame(&mut wire, &Frame::Connect { stream, host: "svc".into(), port: 80 }).unwrap();
        }
        let frames = daemon_frames(wire, false);
        let closes = closes(&frames);
        assert!(closes.iter().all(|(_, r)| r == "stream refused"), "{closes:?}");
        closes.into_iter().map(|(s, _)| s).collect()
    }

    #[test]
    fn connect_id_policy() {
        let h = |n: u32| HOST_ID_BIT | n;
        // Daemon id space refused; a duplicate of a live (pending) id refused.
        assert_eq!(refused_connects(&[5, h(1), h(1)]), [5, h(1)]);
        // Up to MAX_HOST_STREAMS accepted (well past MAX_STREAMS), then refused.
        let ids: Vec<u32> = (1..=MAX_HOST_STREAMS as u32 + 2).map(h).collect();
        assert_eq!(refused_connects(&ids), [h(MAX_HOST_STREAMS as u32 + 1), h(MAX_HOST_STREAMS as u32 + 2)]);
    }
}
