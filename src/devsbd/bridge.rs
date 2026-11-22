//! Host half of the ssh-agent relay (docs/architecture/helper/bridges.md): runs
//! `exec -i <c> devsbd bridge` and serves the streams it carries, connecting
//! each `Open` to the host agent. Owners: `devsandbox serve` (one per running
//! instance, docs/architecture/daemon/host-poll.md; CLI commands ask it through `bridges.ensure`) and
//! the forwarder engine (`spawn_for`, or `attach` to a child it spawned).
//! The daemon's bridges also take the container's `devsbd notify` records and
//! serve its control requests (`devsbd ensure|ls|stop|rm|…`: children, their
//! runs, branches, events, threads; `control::Op::ALL`) (docs/architecture/automations/dispatchers.md).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::mux::{self, Conn, Mux};
use super::control::{self, Request, Response, Status};
use super::proto::caps;
use super::agent_filter::{self, Gate};
use super::{notify, proto, relay_mode, BIN};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// What carries a bridge's bytes: a spawned child's stdio (an `exec`), or
/// a stream (a machine link's `TUNNEL`, `commands::tunnel`). Killing it ends the bridge the same way either way:
/// the child dies, or the stream is shut down; the mux's read then ends.
pub enum Transport {
    Child(Child),
    Stream(UnixStream),
}

impl From<Child> for Transport {
    fn from(child: Child) -> Self {
        Transport::Child(child)
    }
}

impl From<UnixStream> for Transport {
    fn from(stream: UnixStream) -> Self {
        Transport::Stream(stream)
    }
}

impl Transport {
    /// Idempotent.
    fn kill(&mut self) {
        match self {
            Transport::Child(child) => {
                let _ = child.kill();
            }
            Transport::Stream(stream) => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// The child's exit code, waiting for it; a stream has none.
    fn exit_code(&mut self) -> Option<i32> {
        match self {
            Transport::Child(child) => child.wait().ok().and_then(|s| s.code()),
            Transport::Stream(_) => None,
        }
    }
}

/// A running bridge; dropping it kills the `exec` (the in-container bridge
/// then sees stdin EOF and exits), or shuts its stream down.
pub struct Bridge {
    // Shared so the keepalive/handshake-timeout threads can kill the child too;
    // `Drop` and those threads race on `kill()`, which is idempotent.
    child: Arc<Mutex<Transport>>,
    done: Arc<AtomicBool>,
    // Set by the handshake thread when the failure is a protocol version
    // mismatch — either detected host-side (`proto::version_mismatch`) or by the
    // bridge exiting `proto::MISMATCH_EXIT` (daemon mismatch). A mismatched
    // bridge is retried only after the long `MISMATCH_RETRY` (or at once on a
    // restart or a `bridges.ensure`, see `retry_decision`): only a helper
    // rewrite can fix it.
    mismatch: Arc<AtomicBool>,
    handshake: mpsc::Receiver<Result<(), String>>,
    // Published by the handshake thread once `Hello` succeeds: the live mux (to
    // open forward streams on) and the daemon's advertised caps. Absent while
    // the handshake is pending or after it failed.
    forward: Arc<OnceLock<Forward>>,
    // What the far end is called in messages ("helper in <label> is
    // outdated"): the container for an `exec` bridge, whatever the caller of
    // `attach` names it otherwise.
    label: String,
}

/// The forwarding side of a healthy bridge: the mux carrying its streams and
/// the peer (daemon) caps negotiated in the handshake.
struct Forward {
    mux: Arc<Mux>,
    peer_caps: u32,
}

impl Bridge {
    /// The handshake result, waiting up to `timeout` for it; `None` while it
    /// is still pending. Yields the result once (it's consumed).
    pub fn outcome(&self, timeout: Duration) -> Option<Result<(), String>> {
        self.handshake.recv_timeout(timeout).ok()
    }

    /// The relay ended (container stopped, daemon gone, handshake failed).
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    /// The relay failed on a protocol version mismatch (host or daemon side).
    pub fn is_mismatch(&self) -> bool {
        self.mismatch.load(Ordering::Relaxed)
    }

    /// The daemon's advertised capabilities, available once the handshake has
    /// succeeded; `None` while it's pending or after it failed.
    #[allow(dead_code)] // tests only: the forwarder learns caps through `connect`'s `Unsupported`
    pub fn peer_caps(&self) -> Option<u32> {
        self.forward.get().map(|f| f.peer_caps)
    }

    /// Open a flow-controlled forward stream to `host:port`, dialed by the
    /// daemon inside the container, relaying `conn` over it. `on_close` runs
    /// once when the stream ends, with the reason (see [`mux::OnClose`]).
    ///
    /// Errors before the handshake is done, when the bridge is dead, or when the
    /// daemon doesn't advertise `TCP_FORWARD` (an outdated helper) — the last as
    /// `Unsupported`, so callers can phrase the restart hint.
    pub fn connect(
        &self,
        conn: impl Into<Conn>,
        host: &str,
        port: u16,
        on_close: impl FnOnce(String) + Send + 'static,
    ) -> std::io::Result<u32> {
        let Some(forward) = self.forward.get() else {
            let kind = if self.is_done() {
                std::io::ErrorKind::BrokenPipe
            } else {
                std::io::ErrorKind::NotConnected
            };
            let msg = if self.is_done() { "bridge is not running" } else { "bridge handshake not done" };
            return Err(std::io::Error::new(kind, msg));
        };
        if self.is_done() {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "bridge is not running"));
        }
        if forward.peer_caps & caps::TCP_FORWARD == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("helper in {} is outdated (no port forwarding): restart the instance", self.label),
            ));
        }
        forward.mux.connect(conn, host, port, on_close)
    }

    /// Ask the daemon which processes listen on TCP `port` in the container
    /// (`proto::Frame::PortQuery`), waiting up to `timeout`: `(pid, comm)` in
    /// pid order, empty when nothing listens. Errors like `connect`: before
    /// the handshake, on a dead bridge, `Unsupported` when the daemon doesn't
    /// advertise `PORT_INFO` (an older helper: the caller falls back to
    /// `lsof`), and `TimedOut` when no answer came.
    pub fn listening(&self, port: u16, timeout: Duration) -> std::io::Result<Vec<(u32, String)>> {
        let Some(forward) = self.forward.get().filter(|_| !self.is_done()) else {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "bridge is not running"));
        };
        if forward.peer_caps & caps::PORT_INFO == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "helper has no port lookup"));
        }
        forward.mux.port_query(port, timeout)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap();
        child.kill();
        let _ = child.exit_code();
    }
}

/// How many client-reported agents [`AgentCandidates`] remembers.
const MAX_AGENTS: usize = 8;

/// The host agent sockets this process knows of, most recent first: what
/// clients reported through `bridges.ensure`, a forwarded verb's env or
/// `machines.get {agent}` (docs/reference/api.md). Only the daemon
/// gets reports; everywhere else the list stays empty and [`pick`](Self::pick)
/// goes on to the relayed agents' sockets in the socket dir (a linking
/// host's `agent-link-<host>.sock`, a control plane's `agent-fleet-<control-plane>.sock`)
/// (docs/architecture/daemon/machines.md, *Agent*), then the own
/// `$SSH_AUTH_SOCK`. The reports are what keep the daemon's relay working
/// when the agent it inherited is gone or rotated (a new `ssh -A` session):
/// the next client command brings the live one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AgentCandidates {
    recent: Vec<PathBuf>,
}

impl AgentCandidates {
    /// `path` becomes the most recent; a repeat moves to the front; the
    /// oldest past [`MAX_AGENTS`] is forgotten.
    pub fn report(&mut self, path: PathBuf) {
        self.recent.retain(|p| *p != path);
        self.recent.insert(0, path);
        self.recent.truncate(MAX_AGENTS);
    }

    /// The first candidate `live` accepts: the reported ones (newest first),
    /// then `relays` (the relayed agents' sockets, newest first:
    /// `endpoint::agent_sockets`), then `fallback` (the process's own
    /// `$SSH_AUTH_SOCK`).
    pub fn pick(
        &self,
        relays: Vec<PathBuf>,
        fallback: Option<PathBuf>,
        live: impl Fn(&Path) -> bool,
    ) -> Option<PathBuf> {
        self.recent.iter().cloned().chain(relays).chain(fallback).find(|p| live(p))
    }
}

static AGENTS: Mutex<AgentCandidates> = Mutex::new(AgentCandidates { recent: Vec::new() });

/// Record a client's agent socket (`bridges.ensure`) for every bridge of this
/// process; see [`AgentCandidates`].
pub fn report_agent(path: PathBuf) {
    AGENTS.lock().unwrap_or_else(|e| e.into_inner()).report(path);
}

/// What [`report_agent`] recorded, most recent first.
#[cfg(test)]
pub fn reported_agents() -> Vec<PathBuf> {
    AGENTS.lock().unwrap_or_else(|e| e.into_inner()).recent.clone()
}

/// This process's own `$SSH_AUTH_SOCK`, unchecked: what a client reports to
/// the daemon.
pub fn own_agent() -> Option<PathBuf> {
    std::env::var_os("SSH_AUTH_SOCK").filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// A socket that accepts a connection right now: a leftover socket file whose
/// agent died fails this, unlike an existence check.
fn agent_live(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// The host agent socket, chosen per stream so agent rotation needs no
/// restart: the first live reported agent, else the first live relayed
/// agent's socket in the socket dir (newest first; a missing or unreadable dir has
/// none), else `$SSH_AUTH_SOCK` when live (see [`AgentCandidates::pick`]).
/// Also gates whether a bridge is worth spawning (`has_host_agent`). The
/// daemon answers its machines' `AGENT` streams with it too
/// (`daemon::agent_for`).
pub fn host_agent() -> Option<PathBuf> {
    let candidates = AGENTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let relays = crate::serve::endpoint::socket_dir()
        .map(|dir| crate::serve::endpoint::agent_sockets(&dir))
        .unwrap_or_default();
    candidates.pick(relays, own_agent(), agent_live)
}

/// Whether the host has a usable ssh-agent right now. Gates bridge spawning
/// (`Bridges::reconcile`, the CLI's `bridges.ensure`) and `SSH_AUTH_SOCK`
/// injection: with no agent a bridge and the env var only cost an extra
/// `exec` that can't help. It counts client-reported agents (in the daemon)
/// and live relayed agents, so a report or a relayed agent coming up replaces
/// the daemon's agent-less bridges (see `reconcile`).
pub fn has_host_agent() -> bool {
    host_agent().is_some()
}

/// A container's outbox message (`devsbd notify` or `devsbd thread put|rm`),
/// tagged with the instance whose bridge delivered it: its state key (for
/// display and the dispatcher check) and its `instance_id` (the owner
/// identity the Inbox store threads by), so the sink needs no state lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub instance: String,
    pub instance_id: String,
    pub message: notify::Message,
}

/// Where a bridge hands the notifications it takes. Called on the stream's
/// handler thread (never the UI thread), *before* the daemon gets its `ok`:
/// an `Err` (the store wouldn't take it) means no reply, so the daemon keeps
/// the record and retries.
pub type Sink = Arc<dyn Fn(Notification) -> Result<(), String> + Send + Sync>;

/// Serves one control request that arrived on instance `key`'s (state key)
/// bridge. Blocking; called on the stream's handler thread.
pub type ControlHandler = Arc<dyn Fn(&str, &Request) -> Response + Send + Sync>;

/// Serves one `events-follow` request from instance `key` (state key) by
/// writing its held-open response to the stream until it ends (`control`'s
/// module doc). Blocking; called on the stream's handler thread.
pub type FollowHandler = Arc<dyn Fn(&str, &Request, &mut dyn Write) + Send + Sync>;

/// The instance an `HTTP_PROXY` stream came from: the one whose bridge
/// carried it, whatever the container says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// Its state key.
    pub instance: String,
    pub instance_id: String,
}

/// Serves the credential-broker connections a daemon bridge carries
/// (docs/plans/active/credential-broker.md, *Shape*).
pub trait Broker: Send + Sync {
    /// One connection, on its own handler thread, for as long as it lasts:
    /// `header` is the helper's hint (already read off `stream`), `stream`
    /// the client's bytes from there on, both ways; shutting its write side
    /// down is the client's EOF.
    fn serve(&self, caller: &Caller, header: proto::ProxyHeader, stream: UnixStream);
}

/// What a long-lived (daemon) bridge serves beyond the agent: notify into its
/// sink, control through its handler, broker connections to its broker, all
/// tagged with the instance it serves.
#[derive(Clone)]
struct Services {
    instance: String,
    instance_id: String,
    sink: Sink,
    control: ControlHandler,
    /// `events-follow` streams ([`dispatch_follow`] outside tests).
    follow: FollowHandler,
    // Live notify/control handler threads of this bridge (see `HandlerSlot`).
    handlers: Arc<AtomicUsize>,
    /// `None`: `HTTP_PROXY` is neither advertised nor served.
    broker: Option<Arc<dyn Broker>>,
    // Live broker handler threads of this bridge, up to `MAX_BROKER_HANDLERS`.
    brokered: Arc<AtomicUsize>,
}

impl Services {
    fn new(instance: String, instance_id: String, sink: Sink, control: ControlHandler) -> Services {
        let follow: FollowHandler = Arc::new(dispatch_follow);
        Services {
            instance,
            instance_id,
            sink,
            control,
            follow,
            handlers: Arc::default(),
            broker: None,
            brokered: Arc::default(),
        }
    }
}

/// Notify/control handler threads one bridge may run at once. The container
/// controls its end of the mux, and a peer `Close` frees the mux entry while
/// our handler keeps going, so `mux::MAX_STREAMS` alone doesn't bound these.
const MAX_HANDLERS: usize = 8;

/// Broker handler threads one bridge may run at once, apart from
/// [`MAX_HANDLERS`]: broker connections last (keep-alive, a model's SSE) and
/// must not starve notify and control. Under `mux::MAX_STREAMS`, the cap on
/// every stream the container opens, so agent streams keep room too.
const MAX_BROKER_HANDLERS: usize = 48;

/// One of a bridge's handler slots, held by a handler thread for its whole
/// life and released on drop.
struct HandlerSlot(Arc<AtomicUsize>);

impl HandlerSlot {
    /// One of `max` slots on `count`, or `None` when all are taken.
    fn acquire(count: &Arc<AtomicUsize>, max: usize) -> Option<HandlerSlot> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1))
            .ok()
            .map(|_| HandlerSlot(Arc::clone(count)))
    }
}

impl Drop for HandlerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The caps a host bridge advertises in its `Caps` frame: `SSH_AGENT` only
/// with an agent socket, `NOTIFY` and `CONTROL` only with a sink (the daemon's
/// bridges) — so short-lived sink-less bridges (forwards)
/// never take notify streams, which would drop the records with them, nor
/// control requests, which must outlive a one-shot command. Every such bridge
/// advertises `CONTROL`, dispatcher or not: `dispatch::handle` re-checks the
/// config on each request and answers `Denied` for the rest. `HTTP_PROXY`
/// only with a broker too, so the helper answers its clients `503` itself
/// rather than routing them to a bridge that refuses the stream.
fn own_caps(has_agent: bool, has_sink: bool, has_broker: bool) -> u32 {
    let agent = if has_agent { caps::SSH_AGENT } else { 0 };
    let sink = if has_sink { caps::NOTIFY | caps::CONTROL } else { 0 };
    let broker = if has_sink && has_broker { caps::HTTP_PROXY } else { 0 };
    agent | sink | broker
}

/// A control stream whose request doesn't end (no `Eof`) within this is
/// dropped. The daemon sends the whole request at once. Only the request:
/// an `events-follow` stream reads nothing after it.
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// An `events-follow` line the mux doesn't take within this ends the
/// follower (a wedged bridge; the keepalive kills it soon anyway). Well over
/// [`control::FOLLOW_PING`], which is the longest a healthy follower is idle
/// between writes, so an idle stream is never cut by it.
const FOLLOW_WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// The production [`ControlHandler`]: `dispatch::handle`, serialized host-wide.
/// Each op is a `devsandbox` subprocess that loads and saves `state.toml`;
/// two at once (two dispatchers, or one script firing in parallel) would race
/// on it and lose a write. Poisoning is ignored: the guard protects no data.
/// Ops that don't write state (`ls`, and every run op: they only exec in a
/// child) and requests from non-dispatchers (denied without running
/// anything) skip the lock, so they never wait behind a long `ensure` — a
/// `run-wait` holds its handler for up to `dispatch::MAX_WAIT`.
fn dispatch_control(key: &str, req: &Request) -> Response {
    use crate::commands::dispatch;
    static LOCK: Mutex<()> = Mutex::new(());
    let _in_flight = InFlight::enter();
    if !writes_state(req.op) || !dispatch::declares_dispatcher(key) {
        return dispatch::handle(key, req);
    }
    let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dispatch::handle(key, req)
}

/// The production [`FollowHandler`]: `dispatch::follow`, counted in
/// [`CONTROL_IN_FLIGHT`] for its whole life (a follower keeps the daemon
/// from idling out), never under the control lock (it only reads).
fn dispatch_follow(key: &str, req: &Request, out: &mut dyn Write) {
    let _in_flight = InFlight::enter();
    crate::commands::dispatch::follow(key, req, out)
}

/// Control requests being handled by [`dispatch_control`] right now, waiting
/// ones (`events --wait`, `run wait`) and `events-follow` streams
/// ([`dispatch_follow`]) included: a daemon holder row.
static CONTROL_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// How many control requests this process is serving (see
/// [`CONTROL_IN_FLIGHT`]).
pub fn control_in_flight() -> usize {
    CONTROL_IN_FLIGHT.load(Ordering::Acquire)
}

/// Counts one request in [`CONTROL_IN_FLIGHT`] until dropped, so a panicking
/// handler still leaves the count right.
struct InFlight;

impl InFlight {
    fn enter() -> InFlight {
        CONTROL_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        InFlight
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        CONTROL_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Whether `op` writes `state.toml`, by a `devsandbox` subprocess or (`done`)
/// in the handler ([`dispatch_control`] serializes those). Run ops,
/// `run-rm`/`run-prune` included, only exec in a child.
fn writes_state(op: crate::devsbd::control::Op) -> bool {
    use crate::devsbd::control::Op;
    matches!(op, Op::Ensure | Op::Stop | Op::Rm | Op::Done)
}

/// Room over `notify::MAX_RECORD` before a notify stream counts as oversized:
/// the helper enforces the cap itself, this just bounds a misbehaving peer.
const NOTIFY_SLACK: usize = 1024;

/// A notify stream whose record doesn't end (no `Eof`) within this is dropped,
/// so a wedged daemon can't pin handler threads.
const NOTIFY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Reply to an undecodable record: anything not starting with
/// `notify::REPLY_OK` makes the daemon keep the file and retry later.
const NOTIFY_REPLY_BAD: &[u8] = b"bad record";

/// The local end for an `Open` on `channel`, or `None` to refuse it: agent
/// streams connect to the host agent (when this bridge has a provider),
/// through `gate`'s key filter on its own thread when set, notify
/// and control streams go to a handler thread (when it has `services` and a
/// free [`HandlerSlot`]), broker streams likewise when its services have a
/// broker, on a slot of their own. Must return at once: it runs on the mux's
/// `serve` thread.
fn open_stream(
    channel: u8,
    agent: Option<&impl Fn() -> Option<PathBuf>>,
    gate: Option<&Arc<Gate>>,
    services: Option<&Services>,
) -> Option<Conn> {
    let (handler, slots, max): (fn(UnixStream, &Services), _, _) = match channel {
        proto::channel::SSH_AGENT => {
            let host = UnixStream::connect(agent?()?).ok()?;
            let Some(gate) = gate else { return Some(host.into()) };
            let (ours, theirs) = UnixStream::pair().ok()?;
            let gate = Arc::clone(gate);
            std::thread::spawn(move || gate.serve(ours, host));
            return Some(theirs.into());
        }
        proto::channel::NOTIFY => (handle_notify, &services?.handlers, MAX_HANDLERS),
        proto::channel::CONTROL => (handle_control, &services?.handlers, MAX_HANDLERS),
        proto::channel::HTTP_PROXY if services?.broker.is_some() => {
            (handle_proxy, &services?.brokered, MAX_BROKER_HANDLERS)
        }
        _ => return None,
    };
    let slot = HandlerSlot::acquire(slots, max)?;
    let to = services?.clone();
    let (ours, theirs) = UnixStream::pair().ok()?;
    std::thread::spawn(move || {
        let _slot = slot;
        handler(ours, &to)
    });
    Some(theirs.into())
}

/// One control stream: read the request to EOF (the daemon's `Eof`
/// half-closes our side), decode, run it through the handler, write the
/// encoded response, close. Oversized or undecodable → a `Usage` response;
/// timed out → close with no reply (the daemon reports the host
/// disconnected). Silent: the bridge's owner does any reporting. An `events-follow`
/// request goes to the follow handler instead, which holds the stream (and
/// this handler's slot) until it ends.
fn handle_control(mut conn: UnixStream, to: &Services) {
    let _ = conn.set_read_timeout(Some(CONTROL_READ_TIMEOUT));
    let mut buf = Vec::new();
    if (&conn).take(control::MAX_REQUEST as u64 + 1).read_to_end(&mut buf).is_err() {
        return;
    }
    let resp = if buf.len() > control::MAX_REQUEST {
        Response::new(Status::Usage, format!("request longer than {} bytes", control::MAX_REQUEST))
    } else {
        match std::str::from_utf8(&buf).map_err(|e| e.to_string()).and_then(control::decode_request) {
            Ok(req) if req.op == control::Op::Subscribe => {
                let _ = conn.set_write_timeout(Some(FOLLOW_WRITE_TIMEOUT));
                return (to.follow)(&to.instance, &req, &mut conn);
            }
            Ok(req) => (to.control)(&to.instance, &req),
            Err(e) => Response::new(Status::Usage, format!("bad request: {e}")),
        }
    };
    let _ = conn.write_all(control::encode_response(&resp).as_bytes());
}

/// A broker stream's header must arrive within this; the helper sends it
/// with the `Open`.
const PROXY_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// One broker stream: read its header, then hand the stream to the broker
/// for as long as the connection lasts. A missing or bad header closes it.
fn handle_proxy(conn: UnixStream, to: &Services) {
    let Some(broker) = &to.broker else { return };
    let _ = conn.set_read_timeout(Some(PROXY_HEADER_TIMEOUT));
    let Ok(header) = proto::ProxyHeader::read(&mut &conn) else { return };
    let _ = conn.set_read_timeout(None);
    let caller = Caller { instance: to.instance.clone(), instance_id: to.instance_id.clone() };
    broker.serve(&caller, header, conn);
}

/// One notify stream: read the record to EOF (the daemon's `Eof` half-closes
/// our side), decode, hand it to the sink, and only then reply `ok`. Applying
/// before acking is what makes delivery durable: a host that dies between the
/// two (or a store that won't take the record) leaves the record in the
/// container's outbox, and the daemon resends it. Oversized or timed-out →
/// close with no reply; undecodable → `NOTIFY_REPLY_BAD`. Silent throughout:
/// the bridge's owner does any reporting.
fn handle_notify(mut conn: UnixStream, to: &Services) {
    let _ = conn.set_read_timeout(Some(NOTIFY_READ_TIMEOUT));
    let limit = notify::MAX_RECORD + NOTIFY_SLACK;
    let mut buf = Vec::new();
    if (&conn).take(limit as u64 + 1).read_to_end(&mut buf).is_err() || buf.len() > limit {
        return;
    }
    let decoded = std::str::from_utf8(&buf).map_err(|e| e.to_string()).and_then(notify::decode);
    let Ok(message) = decoded else {
        let _ = conn.write_all(NOTIFY_REPLY_BAD);
        return;
    };
    let n = Notification {
        instance: to.instance.clone(),
        instance_id: to.instance_id.clone(),
        message,
    };
    if (to.sink)(n).is_ok() {
        let _ = conn.write_all(notify::REPLY_OK);
    }
}

/// A `Bridges` bridge for instance `key`: the host agent only when
/// `with_agent`, key-filtered under a git-host claim (refusals to `log` and
/// the instance's Inbox), notify + control only when `sink` is set, broker
/// streams when `broker` is too. `observe` gets its handshake outcome and
/// its end (see [`BridgeBoard`]).
fn spawn_managed(
    key: &str,
    info: &Instance,
    with_agent: bool,
    sink: Option<&Sink>,
    broker: Option<&Arc<dyn Broker>>,
    log: Option<fn(&str)>,
    observe: Observer,
) -> Option<Bridge> {
    let hash = info.devsbd_arch.and_then(super::hash).unwrap_or_default();
    let notify = sink.map(|s| Services {
        broker: broker.cloned(),
        ..Services::new(key.to_string(), info.instance_id.clone(), Arc::clone(s), Arc::new(dispatch_control))
    });
    if with_agent {
        let gate = Gate::for_instance(info, refusal_report(key, &info.instance_id, sink, log)).map(Arc::new);
        spawn_observed(&info.container, hash, Some(host_agent), gate, notify, Some(observe))
    } else {
        let none: Option<fn() -> Option<PathBuf>> = None;
        spawn_observed(&info.container, hash, none, None, notify, Some(observe))
    }
}

/// Where a filtered agent's refusals go: `serve.log`, and the instance's
/// Inbox as one warning that each new refusal replaces, since the client
/// only ever prints "agent refused operation".
fn refusal_report(key: &str, instance_id: &str, sink: Option<&Sink>, log: Option<fn(&str)>) -> agent_filter::Report {
    let (key, instance_id, sink) = (key.to_string(), instance_id.to_string(), sink.cloned());
    Arc::new(move |msg: String| {
        if let Some(log) = log {
            log(&format!("{key}: {msg}"));
        }
        if let Some(sink) = &sink {
            let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let record = notify::Record { level: notify::Level::Warn, key: Some("ssh-agent".into()), link: None, msg, at };
            let _ = sink(Notification {
                instance: key.clone(),
                instance_id: instance_id.clone(),
                message: notify::Message::Notify(record),
            });
        }
    })
}

/// A daemon-like bridge for instance `key` serving only `broker` (notify
/// dropped, control refused): tests outside this module that need the real
/// broker behind a container's listener.
#[cfg(test)]
pub(crate) fn spawn_brokered(key: &str, info: &Instance, broker: Arc<dyn Broker>) -> Option<Bridge> {
    let hash = info.devsbd_arch.and_then(super::hash).unwrap_or_default();
    let sink: Sink = Arc::new(|_| Ok(()));
    let control: ControlHandler = Arc::new(|_: &str, _: &Request| Response::new(Status::Usage, "not served"));
    let services =
        Services { broker: Some(broker), ..Services::new(key.to_string(), info.instance_id.clone(), sink, control) };
    let none: Option<fn() -> Option<PathBuf>> = None;
    spawn_with(&info.container, hash, none, Some(services))
}


/// Start a bridge for `container` with helper `hash`, optionally serving the
/// host ssh-agent. `with_agent` = true wires the real `host_agent` provider (a
/// forward bridge serves ssh-agent when the host has one, advertising
/// `SSH_AGENT` honestly); false makes an agent-less bridge that the daemon
/// won't route agent clients to. The forwarder (docs/architecture/daemon/forwards.md)
/// uses this so `spawn_with`'s generic agent type stays private.
pub fn spawn_for(container: &str, hash: &str, with_agent: bool) -> Option<Bridge> {
    if with_agent {
        spawn_with(container, hash, Some(host_agent), None)
    } else {
        let none: Option<fn() -> Option<PathBuf>> = None;
        spawn_with(container, hash, none, None)
    }
}

/// Start a bridge without waiting for its handshake (see `outcome`). Fully
/// quiet (stderr captured), since its owner does the reporting; `None` only when
/// the `exec` can't even be spawned. `agent` is the host ssh-agent socket
/// provider, or `None` for an agent-less bridge (a `devsandbox port` forward): it advertises no
/// `SSH_AGENT` cap and refuses agent `Open`s, so the daemon won't route agent
/// clients to it and it can't steal ssh from an older agent bridge. `notify`
/// set → the bridge advertises `NOTIFY` + `CONTROL` and takes the daemon's
/// notify and control streams.
fn spawn_with(
    container: &str,
    hash: &str,
    agent: Option<impl Fn() -> Option<PathBuf> + Send + 'static>,
    notify: Option<Services>,
) -> Option<Bridge> {
    spawn_observed(container, hash, agent, None, notify, None)
}

/// Gets a bridge's news from its handshake thread: its mux and the daemon's
/// caps once the handshake succeeded (`on_link`, just before `Ready`), and its
/// status changes: `Ready` or `Failed` once the handshake is decided (after
/// `peer_caps` is published), then `Failed` again when a healthy bridge ends.
/// Both must return at once.
struct Observer {
    on_status: Box<dyn Fn(BridgeStatus) + Send>,
    on_link: Box<dyn Fn(&Arc<Mux>, u32) + Send>,
}

impl Observer {
    fn status(&self, status: BridgeStatus) {
        (self.on_status)(status)
    }

    fn link(&self, mux: &Arc<Mux>, caps: u32) {
        (self.on_link)(mux, caps)
    }
}

/// A managed bridge's live connection, as [`BridgeBoard::link`] hands it to
/// host → container callers (`serve::helper`): the mux to open streams on,
/// the daemon's caps, and which attach it is (a new bridge for the same
/// container gets a new one, so per-attach caches know to refresh).
#[derive(Clone)]
pub struct Link {
    pub mux: Arc<Mux>,
    pub caps: u32,
    pub attach: u64,
}

impl Link {
    /// Open an `RPC` stream (`Mux::open_host`): `Unsupported` when the
    /// daemon doesn't advertise `caps::RPC` (an older helper: the caller
    /// falls back to `exec`).
    pub fn open_rpc(&self, on_close: impl FnOnce(String) + Send + 'static) -> std::io::Result<(u32, Conn)> {
        if self.caps & caps::RPC == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "helper has no RPC: restart the instance"));
        }
        self.mux.open_host(proto::channel::RPC, on_close)
    }
}

/// [`spawn_with`], plus an [`Observer`] (the daemon's [`BridgeBoard`]):
/// spawns `<runtime> exec -i -u root <container> devsbd bridge` and
/// [`attach_with`]es to it, labelled by the container.
fn spawn_observed(
    container: &str,
    hash: &str,
    agent: Option<impl Fn() -> Option<PathBuf> + Send + 'static>,
    gate: Option<Arc<Gate>>,
    notify: Option<Services>,
    observe: Option<Observer>,
) -> Option<Bridge> {
    let mut child = bridge_command(container)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    Some(attach_with(child.into(), stdin, stdout, stderr, hash, container, None, agent, gate, notify, observe))
}

/// `<runtime> exec -i -u root <container> devsbd bridge`: the in-container end
/// of every bridge, spawned here or on a machine's `TUNNEL` stream
/// (`serve::fleet::tunnel`).
pub fn bridge_command(container: &str) -> Command {
    let mut cmd = Command::new(backend().bin());
    cmd.args(["exec", "-i", "-u", "root", container, BIN, "bridge"]);
    cmd
}

/// Run a bridge over an already-spawned child whose stdio carries an
/// in-container `devsbd bridge` (directly, or behind a transport such as
/// ssh), or over a stream that does (`stdin`/`stdout` its clones): the
/// handshake, then the mux. Agent-less, no notify/control
/// services, no observer: what a forward over a non-`exec` transport needs.
/// `stdout` may already be partly read (a header before the bridge's bytes);
/// the handshake starts at whatever it yields next. `label` names the far
/// end in messages (`helper in <label> is outdated …`); `machine`, when the
/// far end is a machine's, is what an outdated helper's fix names (its
/// devsandbox, not the instance, embeds that helper). Dropping the `Bridge`
/// kills `child` (shuts the stream down).
pub fn attach(
    child: impl Into<Transport>,
    stdin: impl Write + Send + 'static,
    stdout: impl Read + Send + 'static,
    stderr: impl Read + Send + 'static,
    hash: &str,
    label: &str,
    machine: Option<&str>,
) -> Bridge {
    let none: Option<fn() -> Option<PathBuf>> = None;
    attach_with(child.into(), stdin, stdout, stderr, hash, label, machine, none, None, None, None)
}

/// [`attach`] with the full set of options [`spawn_observed`] takes.
#[allow(clippy::too_many_arguments)]
fn attach_with(
    child: Transport,
    mut stdin: impl Write + Send + 'static,
    mut stdout: impl Read + Send + 'static,
    mut stderr: impl Read + Send + 'static,
    hash: &str,
    label: &str,
    machine: Option<&str>,
    agent: Option<impl Fn() -> Option<PathBuf> + Send + 'static>,
    gate: Option<Arc<Gate>>,
    notify: Option<Services>,
    observe: Option<Observer>,
) -> Bridge {
    let observe = observe.unwrap_or_else(|| Observer { on_status: Box::new(|_| {}), on_link: Box::new(|_, _| {}) });
    let child = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));
    let mismatch = Arc::new(AtomicBool::new(false));
    let (tx, handshake) = mpsc::channel();
    let hash = hash.to_string();
    let label = label.to_string();
    let machine = machine.map(str::to_string);
    let finished = Arc::clone(&done);
    let hs_mismatch = Arc::clone(&mismatch);
    let forward = Arc::new(OnceLock::new());
    let hs_forward = Arc::clone(&forward);
    // The handshake thread moves `label` (for `mismatch_message`); keep a
    // copy for the `Bridge`'s own `connect` error message.
    let bridge_label = label.clone();
    // Watchdog: if the handshake hasn't produced a result within the timeout,
    // kill the child, which unblocks the blocking `handshake` read below. The
    // handshake thread then sees the read fail and, because `timed_out` is set,
    // reports the timeout instead of the resulting EOF.
    let timed_out = Arc::new(AtomicBool::new(false));
    let bridged = Arc::new(AtomicBool::new(false));
    {
        let child = Arc::clone(&child);
        let timed_out = Arc::clone(&timed_out);
        let bridged = Arc::clone(&bridged);
        std::thread::spawn(move || {
            std::thread::sleep(HANDSHAKE_TIMEOUT);
            // `bridged` flips once the handshake succeeds and routing starts;
            // past that there's no handshake to time out.
            if !bridged.load(Ordering::Relaxed) {
                timed_out.store(true, Ordering::Relaxed);
                child.lock().unwrap().kill();
            }
        });
    }
    let hs_child = Arc::clone(&child);
    std::thread::spawn(move || {
        // The bridge advertises to us `own & daemon` caps, so `daemon_caps` is
        // what the whole path (daemon included) can serve.
        let mut daemon_caps = 0;
        let result = proto::handshake(&mut stdout, &mut stdin, &hash, 0).map(|peer| daemon_caps = peer.caps).map_err(|e| {
            if timed_out.load(Ordering::Relaxed) {
                return "helper handshake timed out".to_string();
            }
            // A version mismatch is a typed error carrying both versions, so we
            // phrase it with the right direction and label. Otherwise the
            // bridge died before `Hello` (no daemon, no binary) and its stderr
            // says why — but when the *daemon* is the mismatch, the bridge
            // already printed its own direction-aware line, so prefer stderr.
            if let Some(vm) = proto::version_mismatch(&e) {
                hs_mismatch.store(true, Ordering::Relaxed);
                return mismatch_message(&label, machine.as_deref(), vm);
            }
            let mut msg = String::new();
            // `BrokenPipe`: the child exited before our `Hello` went out (a
            // fast failure), the same death seen from the write side.
            if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe) {
                let _ = std::io::Read::read_to_string(&mut stderr, &mut msg);
                // The bridge exits `MISMATCH_EXIT` when *its* handshake with the
                // daemon failed on a version mismatch (it printed the line we
                // just read). Classify that from the exit code, not the text, so
                // reconcile can stop retrying. `wait()` after EOF is prompt.
                if hs_child.lock().unwrap().exit_code() == Some(proto::MISMATCH_EXIT) {
                    hs_mismatch.store(true, Ordering::Relaxed);
                }
            }
            match msg.trim() {
                "" => e.to_string(),
                m => m.trim_start_matches("devsbd: ").to_string(),
            }
        });
        let ok = result.is_ok();
        // Stop the watchdog from racing a kill against a healthy bridge.
        bridged.store(ok, Ordering::Relaxed);
        if let Err(e) = &result {
            let why = e.clone();
            // `done` before the result: a caller that sees the failed
            // `outcome()` must also see `is_done()`.
            finished.store(true, Ordering::Relaxed);
            let _ = tx.send(result);
            // After `done`: a reconcile that sees the failure also sees the
            // bridge dead, so an urgent one respawns it.
            observe.status(BridgeStatus::Failed(why));
            return;
        }
        {
            let mux = Mux::new(stdin);
            // Tell the daemon our caps right after the handshake, on stream 0:
            // `SSH_AGENT` only when this bridge has an agent provider and it
            // yields a socket now; `NOTIFY`/`CONTROL` only with a sink, `HTTP_PROXY`
            // only with a broker too (`own_caps`). An
            // agent-less bridge (a `devsandbox port` forward) advertises no
            // `SSH_AGENT`, so the daemon won't route agent clients to it. Best
            // effort — a send failure just means the connection died.
            let has_agent = agent.as_ref().and_then(|a| a()).is_some();
            let has_broker = notify.as_ref().is_some_and(|s| s.broker.is_some());
            let _ = mux.send(&proto::Frame::Caps(own_caps(has_agent, notify.is_some(), has_broker)));
            // Publish the mux + daemon caps *before* reporting the handshake
            // result, so a caller that sees `outcome() == Ok` can rely on
            // `peer_caps()` / `connect()` being ready (no publish race).
            let _ = hs_forward.set(Forward { mux: Arc::clone(&mux), peer_caps: daemon_caps });
            // Likewise the board's link: a `bridges.ensure` waiter that sees
            // `Ready` finds it.
            observe.link(&mux, daemon_caps);
            observe.status(BridgeStatus::Ready);
            let _ = tx.send(result);
            // Keepalive: a wedged daemon (stopped reading) is caught by inbound
            // silence; `on_dead` kills the child, ending the `serve` read below
            // so `done` flips and `Bridges::reconcile` retries.
            let dead_child = Arc::clone(&hs_child);
            mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
                dead_child.lock().unwrap().kill();
            });
            mux.serve(stdout, move |_, channel| open_stream(channel, agent.as_ref(), gate.as_ref(), notify.as_ref()));
        }
        finished.store(true, Ordering::Relaxed);
        observe.status(BridgeStatus::Failed("the bridge ended (container stopped or helper gone)".into()));
    });
    Bridge { child, done, mismatch, handshake, forward, label: bridge_label }
}

/// How long the host waits for the daemon handshake before killing the child
/// and reporting a timeout, so a container that accepts the `exec` but never
/// completes `Hello` (wedged daemon, stuck runtime) can't block a bridge owner
/// forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A host-side, direction-aware message for a helper/host protocol mismatch.
/// Peer (the helper) older → a local one is fixed by restarting the instance
/// (the binary is rewritten), a `machine`'s by updating devsandbox there (it
/// embeds the helper it installs); peer newer → this `devsandbox` is the
/// stale side. `label` is the bridge's (the container for an `exec` bridge);
/// the instance key isn't on hand here, so the local hint stays generic.
fn mismatch_message(label: &str, machine: Option<&str>, vm: &proto::VersionMismatch) -> String {
    if vm.peer < vm.ours {
        let fix = match machine {
            Some(m) => format!("update devsandbox on {m} (and restart the instance there)"),
            None => "restart the instance".to_string(),
        };
        format!("helper in {label} is outdated (protocol {}, need {}): {fix}", vm.peer, vm.ours)
    } else {
        format!(
            "this devsandbox is older than the helper in {label} (protocol {}, need {})",
            vm.peer, vm.ours
        )
    }
}

/// Minimum gap between bridge attempts for one container, so a container
/// whose bridge keeps failing (daemon down) isn't re-exec'd every tick.
const RETRY: Duration = Duration::from_secs(10);

/// Gap after a version mismatch. Long, since only a helper rewrite fixes it,
/// but not infinite: `devsandbox start` on a still-running container rewrites
/// the helper without the container ever leaving the running set.
const MISMATCH_RETRY: Duration = Duration::from_secs(300);

/// What `reconcile` should do with the current bridge for one container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// No live bridge for this container yet: start one.
    Spawn,
    /// Keep the existing entry untouched (healthy, or dead but still within
    /// its retry gap).
    Keep,
    /// The bridge is dead and past its retry gap: replace it.
    Respawn,
}

/// The retry policy for one container's bridge, factored out so it's testable
/// without a container runtime. `entry` is the current live bridge's state, or
/// `None` when there is none: `(done, mismatch, since_spawn)`. A dead bridge
/// is retried after `RETRY`, or `MISMATCH_RETRY` when it died on a version
/// mismatch (a restart, which drops the entry, retries at once). `urgent`: a
/// client just asked for this container's bridge (`bridges.ensure`, e.g. right
/// after `start` rewrote its helper), so a dead one is retried at once.
fn retry_decision(entry: Option<(bool, bool, Duration)>, urgent: bool) -> Retry {
    match entry {
        None => Retry::Spawn,
        Some((true, ..)) if urgent => Retry::Respawn,
        Some((true, mismatch, since)) => {
            let gap = if mismatch { MISMATCH_RETRY } else { RETRY };
            if since >= gap { Retry::Respawn } else { Retry::Keep }
        }
        Some((false, ..)) => Retry::Keep,
    }
}

/// Whether `reconcile` wants a bridge for one running instance, and if so
/// whether it serves the host agent: `None` = no bridge, `Some(with_agent)`.
/// The agent is relayed (`relay_mode` + a host agent); a sink adds a
/// bridge to every helper-capable instance so its notify outbox drains.
fn wanted(helper: bool, relay: bool, host_agent: bool, has_sink: bool) -> Option<bool> {
    let with_agent = host_agent && relay;
    (with_agent || (has_sink && helper)).then_some(with_agent)
}

/// Where one daemon bridge stands, as [`BridgeBoard`] shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeStatus {
    /// Spawned, handshake not decided yet.
    Pending,
    /// Handshake done (`peer_caps` known), still running.
    Ready,
    /// The handshake failed (its message, e.g. a helper version mismatch),
    /// the `exec` couldn't start, or a healthy bridge ended.
    Failed(String),
}

/// What `bridges.ensure` with `wait` answers: whether the container's
/// bridge is up, else why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    pub ready: bool,
    pub error: Option<String>,
}

impl Readiness {
    fn ready() -> Readiness {
        Readiness { ready: true, error: None }
    }

    fn not(why: impl Into<String>) -> Readiness {
        Readiness { ready: false, error: Some(why.into()) }
    }
}

/// The board's data: each daemon bridge's status by container (tagged with a
/// spawn id so a replaced bridge's late news can't overwrite its
/// successor's), the live ones' [`Link`]s (`attach` = that spawn id), and
/// the newest `bridges.ensure` ticket the worker has reconciled for. One
/// lock for both maps: a link is published only while its spawn id is
/// current, so a dropped bridge's late link can't outlive its successor's
/// arrival.
#[derive(Default)]
struct Board {
    handled: u64,
    next_id: u64,
    bridges: HashMap<String, (u64, BridgeStatus)>,
    links: HashMap<String, Link>,
}

/// The daemon bridges' statuses and links, shared by the worker (spawns,
/// removals, handled tickets), each bridge's handshake thread (its outcome,
/// link and end), the API's `bridges.ensure` (waits on it) and the helper
/// client (`serve::helper`, takes links). Every writer only takes the lock
/// briefly, so a waiting handler never holds up the worker. Cloneable.
#[derive(Clone, Default)]
pub struct BridgeBoard {
    inner: Arc<(Mutex<Board>, Condvar)>,
    tickets: Arc<std::sync::atomic::AtomicU64>,
    /// Called (outside the lock) when a bridge turns `Failed`; see
    /// [`BridgeBoard::on_failed`].
    on_failed: Arc<std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>>,
}

impl BridgeBoard {
    /// A new `bridges.ensure` ticket, to queue with its urgent container.
    pub fn ticket(&self) -> u64 {
        self.tickets.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Set once: called each time a spawned bridge turns `Failed` (its
    /// handshake failed or timed out, a healthy one ended; not `set`). The host
    /// poll wakes on it: a failed bridge unsettles the board, and its retry
    /// is only acted on by a poll pass, which may be sleeping `QUIET_POLL`.
    pub fn on_failed(&self, hook: Box<dyn Fn() + Send + Sync>) {
        let _ = self.on_failed.set(hook);
    }

    fn update(&self, f: impl FnOnce(&mut Board)) {
        let (lock, cvar) = &*self.inner;
        f(&mut lock.lock().unwrap_or_else(|e| e.into_inner()));
        cvar.notify_all();
    }

    /// After an update that made an entry `status` (the lock released).
    fn notify(&self, status_failed: bool) {
        if status_failed && let Some(hook) = self.on_failed.get() {
            hook();
        }
    }

    /// `container` gets a fresh bridge: `Pending`, and the [`Observer`] its
    /// handshake thread reports through.
    /// The previous bridge's link goes (it was just dropped); this one's is
    /// published by `on_link` and removed when it turns `Failed`.
    fn spawning(&self, container: &str) -> Observer {
        let mut id = 0;
        self.update(|b| {
            b.next_id += 1;
            id = b.next_id;
            b.bridges.insert(container.to_string(), (id, BridgeStatus::Pending));
            b.links.remove(container);
        });
        let (board, name) = (self.clone(), container.to_string());
        let on_status = Box::new(move |status: BridgeStatus| {
            let failed = matches!(status, BridgeStatus::Failed(_));
            let mut applied = false;
            board.update(|b| {
                if let Some(entry) = b.bridges.get_mut(&name).filter(|(at, _)| *at == id) {
                    entry.1 = status;
                    applied = true;
                    if failed {
                        b.links.remove(&name);
                    }
                }
            });
            // A replaced bridge's late news changes nothing: no wake.
            board.notify(failed && applied);
        });
        let (board, name) = (self.clone(), container.to_string());
        let on_link = Box::new(move |mux: &Arc<Mux>, caps: u32| {
            board.update(|b| {
                if b.bridges.get(&name).is_some_and(|(at, _)| *at == id) {
                    b.links.insert(name.clone(), Link { mux: Arc::clone(mux), caps, attach: id });
                }
            });
        });
        Observer { on_status, on_link }
    }

    /// `container`'s live link: its bridge finished the handshake and hasn't
    /// ended. `None` while pending, after it failed or ended, or when the
    /// daemon keeps no bridge for it.
    pub fn link(&self, container: &str) -> Option<Link> {
        self.inner.0.lock().unwrap_or_else(|e| e.into_inner()).links.get(container).cloned()
    }

    /// No [`BridgeBoard::on_failed`] wake: `reconcile` calls this itself when
    /// the `exec` can't start, leaving no live entry (so no retry gap), and a
    /// wake would make the next pass retry at once, in a loop.
    fn set(&self, container: &str, status: BridgeStatus) {
        self.update(|b| {
            b.next_id += 1;
            let id = b.next_id;
            b.bridges.insert(container.to_string(), (id, status));
            b.links.remove(container);
        });
    }

    /// Forget every container but `keep` (their bridges were dropped).
    fn retain(&self, keep: &[String]) {
        self.update(|b| {
            b.bridges.retain(|c, _| keep.contains(c));
            b.links.retain(|c, _| keep.contains(c));
        });
    }

    /// The worker reconciled with every ticket up to `ticket`.
    fn handled(&self, ticket: u64) {
        self.update(|b| b.handled = b.handled.max(ticket));
    }

    /// No bridge is `Pending` or `Failed`: nothing waits on a poll pass (a
    /// handshake to see through, a dead bridge's retry), so the host poll may
    /// sleep long (`serve::host::tick`). A bridge the worker hasn't spawned
    /// yet isn't on the board; should it (or any) fail later,
    /// [`BridgeBoard::on_failed`] wakes the poll to see it.
    pub fn settled(&self) -> bool {
        let b = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        b.bridges.values().all(|(_, s)| *s == BridgeStatus::Ready)
    }

    /// Wait up to `timeout` for `container`'s bridge as of ticket `ticket`
    /// (see [`verdict`]); past it, why it isn't ready ([`timed_out`]).
    pub fn wait(&self, container: &str, ticket: u64, timeout: Duration) -> Readiness {
        let (lock, cvar) = &*self.inner;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = cvar
            .wait_timeout_while(guard, timeout, |b| verdict(b, container, ticket).is_none())
            .unwrap_or_else(|e| e.into_inner());
        verdict(&guard, container, ticket).unwrap_or_else(|| timed_out(&guard, ticket, timeout))
    }
}

/// `container`'s readiness once it's decided, `None` while still waiting:
/// until the worker has reconciled with `ticket` (so an urgent respawn of a
/// dead bridge has happened), then until its bridge's handshake is decided.
/// No bridge after that reconcile: the daemon doesn't bridge it.
fn verdict(b: &Board, container: &str, ticket: u64) -> Option<Readiness> {
    if b.handled < ticket {
        return None;
    }
    match b.bridges.get(container).map(|(_, s)| s) {
        None => Some(Readiness::not("the host daemon keeps no bridge for it (not running, or no helper)")),
        Some(BridgeStatus::Pending) => None,
        Some(BridgeStatus::Ready) => Some(Readiness::ready()),
        Some(BridgeStatus::Failed(why)) => Some(Readiness::not(why.clone())),
    }
}

/// Why a bridge isn't ready after waiting `waited` (only called while
/// [`verdict`] is still `None`).
fn timed_out(b: &Board, ticket: u64, waited: Duration) -> Readiness {
    let secs = waited.as_secs_f64();
    if b.handled < ticket {
        Readiness::not(format!("the host daemon didn't get to it within {secs}s (container runtime slow?)"))
    } else {
        Readiness::not(format!("the helper handshake didn't finish within {secs}s"))
    }
}

/// The last `serve.log` reason per key, so a failure that repeats while it
/// lasts (a dead bridge retried every [`RETRY`], a record resent until the
/// store takes it) is one line per distinct reason, not one per attempt.
#[derive(Default)]
struct Dedupe(Mutex<HashMap<String, String>>);

impl Dedupe {
    /// Whether `why` differs from the last reason logged for `key`; it
    /// becomes the last either way.
    fn fresh(&self, key: &str, why: &str) -> bool {
        let mut last = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if last.get(key).is_some_and(|w| w == why) {
            return false;
        }
        last.insert(key.to_string(), why.to_string());
        true
    }

    /// `key` recovered: whether a failure had been logged for it.
    fn clear(&self, key: &str) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(key).is_some()
    }

    fn retain(&self, keep: &[String]) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| keep.contains(k));
    }
}

/// The `serve.log` line for a daemon bridge of `container` turning `status`,
/// if any: a failure whose reason is new, or the first `Ready` after a logged
/// failure.
fn status_line(logged: &Dedupe, container: &str, status: &BridgeStatus) -> Option<String> {
    match status {
        BridgeStatus::Pending => None,
        BridgeStatus::Ready => logged.clear(container).then(|| format!("bridge {container}: ready")),
        BridgeStatus::Failed(why) => logged.fresh(container, why).then(|| format!("bridge {container}: {why}")),
    }
}

/// The daemon's set of bridges (`devsandbox serve`), reconciled against the
/// running instances on each poll. Owned by a worker thread (`spawn_worker`).
#[derive(Default)]
pub struct Bridges {
    // container → (bridge, spawned at, spawned with the agent provider).
    live: HashMap<String, (Bridge, Instant, bool)>,
    // Set → bridges advertise `NOTIFY` and are kept even without a host agent.
    sink: Option<Sink>,
    // Set (with `sink`) → bridges advertise and serve `HTTP_PROXY`.
    broker: Option<Arc<dyn Broker>>,
    // Every live bridge's status, for `bridges.ensure` waits.
    board: BridgeBoard,
    // The daemon's `serve.log`; `None` (tests, no sink) logs nothing.
    log: Option<fn(&str)>,
    logged: Arc<Dedupe>,
    // The last `State::load` error logged, until a load succeeds.
    state_error: Option<String>,
    // Running containers recorded without the helper whose install was
    // retried since they appeared (`heal_now`).
    healing: std::collections::HashSet<String>,
}

/// Whether `reconcile` retries the helper install for a running instance
/// recorded without one: once per appearance in the running list (`tried`
/// forgets a container when it stops), and not while `run` still has it
/// `in_flight` (it installs the helper and records the instance itself).
fn heal_now(tried: &mut std::collections::HashSet<String>, info: &Instance, in_flight: bool) -> bool {
    info.devsbd_arch.is_none() && !in_flight && tried.insert(info.container.clone())
}

fn in_flight(info: &Instance) -> bool {
    let view = info.progress.as_ref().map(crate::state::Progress::current_view);
    matches!(view, Some(crate::state::PhaseView::InFlight(_)))
}

impl Bridges {
    /// Keep one bridge per running container in `running` whose instance
    /// wants one (`wanted`); drop the rest. Dead bridges are retried per
    /// `retry_decision`; a bridge whose agent wiring no longer matches (host
    /// agent came or went) is replaced at once. Every (re)spawn first
    /// `ensure_recorded`s the instance's helper, so a CLI upgrade reaches a
    /// running instance once the daemon bridges it: otherwise only
    /// `run`/`start`/`port` rewrite it, and a running dispatcher never gets new
    /// verbs. Keyed on spawns, not `is_mismatch`: a stale helper with the same
    /// protocol `VERSION` still bridges fine, it just lacks verbs. Spawns are
    /// rare (first sight, or a dead bridge's retry gap), so this is one extra
    /// `exec` per spawn, not per snapshot. `urgent` containers skip a dead
    /// bridge's retry gap (`retry_decision`). An instance recorded without
    /// the helper gets its install retried once per appearance (`heal_now`):
    /// one failed install would otherwise cut it off for good, since only
    /// helper instances are re-checked here.
    pub fn reconcile(&mut self, running: &[&str], urgent: &[String]) {
        self.live.retain(|c, _| running.contains(&c.as_str()));
        self.healing.retain(|c| running.contains(&c.as_str()));
        let host_agent = has_host_agent();
        // No host agent and no sink → nothing to relay; skip the per-instance `exec`.
        if !host_agent && self.sink.is_none() {
            self.live.clear();
            self.board.retain(&[]);
            return;
        }
        let state = match State::load() {
            Ok(state) => {
                self.state_error = None;
                state
            }
            Err(e) => {
                let why = format!("{e:#}");
                if let Some(log) = self.log.filter(|_| self.state_error.as_ref() != Some(&why)) {
                    log(&format!("bridges: cannot read state.toml, no bridge changes: {why}"));
                }
                self.state_error = Some(why);
                return;
            }
        };
        let mut keep = Vec::new();
        for (key, info) in &state.instances {
            if !running.contains(&info.container.as_str()) {
                continue;
            }
            let installed;
            let just_healed = self.sink.is_some() && heal_now(&mut self.healing, info, in_flight(info));
            let info = match just_healed.then(|| crate::devsbd::ensure_recorded_or_reason(key, info, true)) {
                Some(Ok(arch)) => {
                    if let Some(log) = self.log {
                        log(&format!("helper {}: installed (its record had none)", info.container));
                    }
                    installed = Instance { devsbd_arch: Some(arch), devsbd_error: None, ..info.clone() };
                    &installed
                }
                Some(Err(why)) => {
                    if let Some(log) = self.log {
                        log(&format!("helper {}: unavailable: {why}", info.container));
                    }
                    info
                }
                None => info,
            };
            let helper = info.devsbd_arch.is_some();
            let Some(with_agent) = wanted(helper, relay_mode(info), host_agent, self.sink.is_some()) else {
                continue;
            };
            keep.push(info.container.clone());
            let entry = self
                .live
                .get(&info.container)
                .filter(|(.., agent)| *agent == with_agent)
                .map(|(b, at, _)| (b.is_done(), b.is_mismatch(), at.elapsed()));
            if retry_decision(entry, urgent.contains(&info.container)) != Retry::Keep {
                self.live.remove(&info.container);
                let healed;
                let recheck = helper && !just_healed;
                let info = match recheck.then(|| crate::devsbd::ensure_recorded(key, info, true)).flatten() {
                    Some(arch) if Some(arch) != info.devsbd_arch => {
                        healed = Instance { devsbd_arch: Some(arch), ..info.clone() };
                        &healed
                    }
                    // Current, or the install failed: spawn on the recorded
                    // helper as before, so a failing install waits out the
                    // bridge's retry gap instead of re-running every snapshot.
                    _ => info,
                };
                let observe = self.observe(&info.container);
                match spawn_managed(key, info, with_agent, self.sink.as_ref(), self.broker.as_ref(), self.log, observe) {
                    Some(b) => {
                        self.live.insert(info.container.clone(), (b, Instant::now(), with_agent));
                    }
                    None => {
                        let failed =
                            BridgeStatus::Failed(format!("cannot run `{} exec` for the bridge", backend().bin()));
                        self.log_status(&info.container, &failed);
                        self.board.set(&info.container, failed);
                    }
                }
            }
        }
        self.live.retain(|c, _| keep.contains(c));
        self.board.retain(&keep);
        self.logged.retain(&keep);
    }

    /// The board's observer for a fresh bridge of `container`, also logging
    /// its status changes ([`status_line`]).
    fn observe(&self, container: &str) -> Observer {
        let Observer { on_status, on_link } = self.board.spawning(container);
        let Some(log) = self.log else { return Observer { on_status, on_link } };
        let (logged, name) = (Arc::clone(&self.logged), container.to_string());
        let on_status = Box::new(move |status: BridgeStatus| {
            if let Some(line) = status_line(&logged, &name, &status) {
                log(&line);
            }
            on_status(status);
        });
        Observer { on_status, on_link }
    }

    fn log_status(&self, container: &str, status: &BridgeStatus) {
        if let (Some(log), Some(line)) = (self.log, status_line(&self.logged, container, status)) {
            log(&line);
        }
    }

    /// Move a `Bridges` onto its own thread, fed running-container lists (plus
    /// the urgent containers, see `reconcile`) over an mpsc. Reconcile (which does `State::load`, `exec` spawns and a helper
    /// reinstall check before each one) never runs on the caller's thread; it
    /// just `send`s the owned list. Dropping the returned [`BridgeWorker`]
    /// closes the channel and joins the thread, which drops every live
    /// `Bridge` (killing its `exec`) before returning.
    ///
    /// `sink` set → every helper-capable running instance gets a notify-serving
    /// bridge (see [`sink`] for what it does with each message). An `open`
    /// record goes to the hooks' [`Opener`], never the Inbox; any other
    /// message is applied to the shared Inbox store first (that
    /// write is what the daemon's `ok` acknowledges, so a failed one is
    /// retried), then shown on the desktop (from the stream's handler thread)
    /// unless [`RateLimit`](super::desktop::RateLimit) holds it back, and
    /// finally its status line ("<instance>: <msg>") goes to the sink's
    /// [`OnShown`], if any. A put that changed nothing produces neither, so a
    /// dispatcher re-asserting its threads is invisible.
    /// `broker` set (with `sink`) → those bridges serve `HTTP_PROXY` streams
    /// to it, tagged with their instance ([`Broker`]).
    /// `board` gets every bridge's status and, after each reconcile, the
    /// newest ticket it covered (the `urgent` pairs' second half).
    pub fn spawn_worker(sink: Option<SinkHooks>, broker: Option<Arc<dyn Broker>>, board: BridgeBoard) -> BridgeWorker {
        let log = sink.as_ref().map(|hooks| hooks.log);
        let sink = sink.map(|hooks| self::sink(apply_message, hooks));
        let (tx, rx) = mpsc::channel::<(Vec<String>, Vec<(String, u64)>)>();
        let handle = std::thread::spawn(move || {
            let mut bridges = Bridges { sink, broker, board, log, ..Bridges::default() };
            // Block for the next list, then coalesce: drain everything already
            // queued and reconcile only against the newest list (with every
            // urgent container queued meanwhile), so a burst of snapshots
            // costs one reconcile.
            while let Ok((mut running, mut urgent)) = rx.recv() {
                while let Ok((next, more)) = rx.try_recv() {
                    running = next;
                    urgent.extend(more);
                }
                let refs: Vec<&str> = running.iter().map(String::as_str).collect();
                let containers: Vec<String> = urgent.iter().map(|(c, _)| c.clone()).collect();
                bridges.reconcile(&refs, &containers);
                if let Some(ticket) = urgent.iter().map(|(_, t)| *t).max() {
                    bridges.board.handled(ticket);
                }
            }
            // Sender dropped: `bridges` drops here, killing every live bridge.
        });
        BridgeWorker { tx: Some(tx), handle: Some(handle) }
    }
}

/// Gets `(instance, status line)` for every message the sink stored and
/// showed (see [`Bridges::spawn_worker`]). Runs on a bridge's handler thread.
pub type OnShown = Box<dyn Fn(&str, String) + Send + Sync>;

/// Opens `(instance, instance_id, url)` for an `open` record that passed
/// [`crate::open::decide`]; the id finds the instance's `state.toml` entry
/// (its profile) whatever its name now. Runs on a bridge's handler thread before the
/// daemon's `ok`, so it must not wait on the browser (nor on a fleet link):
/// the production one is `serve::opens::opener` (routes it to where the
/// user last attached from, on a thread of its own).
/// Injectable so tests see the URL instead of a browser opening.
pub type Opener = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

/// What a notify-serving [`Bridges`] does besides storing messages.
pub struct SinkHooks {
    /// See [`OnShown`]; `None` = nobody to tell.
    pub on_shown: Option<OnShown>,
    pub opener: Opener,
    /// Gets every `open` record refused or dropped here, at intake
    /// (`open::Outcome::Refused`, no profile: it isn't read yet); what
    /// the opener does with the rest it reports itself. `None` = nobody to
    /// tell.
    pub on_open_done: Option<crate::open::OnDone>,
    /// One `serve.log` line: every refused or dropped `open` record.
    pub log: fn(&str),
}

/// [`apply_message`], injectable so a test can tell an `open` record never
/// reaches the Inbox store.
type Apply = fn(Notification) -> Result<Option<(String, crate::inbox::Shown)>, String>;

/// The notify sink every bridge of one [`Bridges`] shares.
///
/// An `open` record is taken before `apply`: [`crate::open::decide`] (scheme,
/// [`crate::open::OPEN_TTL`], one per-instance limiter of
/// [`crate::open::OPEN_RATE_BURST`] per [`crate::open::OPEN_RATE_REFILL`]),
/// then the opener, or a `log` line and a refused `on_open_done`, and `Ok`
/// either way: the `ok` reply means taken, not opened, and a refused URL
/// isn't worth a resend.
///
/// Anything else is `apply`'d (that store write is what the daemon's `ok`
/// acknowledges, so a failed one is retried), shown on the desktop unless
/// the popup [`RateLimit`](super::desktop::RateLimit) holds it back, and its
/// status line goes to `on_shown`.
fn sink(apply: Apply, hooks: SinkHooks) -> Sink {
    use super::desktop::RateLimit;
    let SinkHooks { on_shown, opener, on_open_done, log } = hooks;
    // One limiter of each for every bridge, keyed by instance inside.
    let popups = Mutex::new(RateLimit::default());
    let unstored = Dedupe::default();
    let opens = Mutex::new(RateLimit::new(crate::open::OPEN_RATE_BURST, crate::open::OPEN_RATE_REFILL));
    Arc::new(move |n: Notification| {
        if let notify::Message::Open { at, url } = &n.message {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let limit =
                || opens.lock().unwrap_or_else(|e| e.into_inner()).allow(&n.instance, None, Instant::now());
            let verdict = crate::open::decide(url, *at, now, limit);
            let (Some(line), Some(why)) = (verdict.log_line(&n.instance, url), verdict.refusal()) else {
                opener(&n.instance, &n.instance_id, url);
                return Ok(());
            };
            log(&line);
            if let Some(done) = &on_open_done {
                let outcome = crate::open::Outcome::Refused(why);
                done(crate::open::OpenDone { instance: n.instance.clone(), url: url.clone(), profile: None, outcome });
            }
            return Ok(());
        }
        let instance = n.instance.clone();
        let shown = match apply(n) {
            Ok(shown) => {
                unstored.clear(&instance);
                shown
            }
            Err(e) => {
                if unstored.fresh(&instance, &e) {
                    let why = crate::open::printable(&e);
                    log(&format!("{instance}: notification not stored (the helper keeps it and resends): {why}"));
                }
                return Err(e);
            }
        };
        let Some((instance, shown)) = shown else { return Ok(()) };
        if let Some(popup) = shown.popup {
            let allowed = popups
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .allow(&instance, popup.key.as_deref(), Instant::now());
            if allowed {
                super::desktop::notify_desktop(&instance, &popup);
            }
        }
        if let Some(on_shown) = &on_shown {
            on_shown(&instance, shown.line);
        }
        Ok(())
    })
}

/// Store one delivered message and report what to show, if anything. Split
/// out of the sink closure so the store write (the thing the daemon's `ok`
/// acknowledges) and the display decision are one read-modify-write.
///
/// Authorization is only evaluated for thread messages: `declares_inbox`
/// loads `state.toml` and the config, which a plain notify must not pay for.
/// An `open` record never gets here ([`sink`] takes it first); one that did
/// is refused rather than stored.
fn apply_message(n: Notification) -> Result<Option<(String, crate::inbox::Shown)>, String> {
    use crate::commands::dispatch;
    if matches!(n.message, notify::Message::Open { .. }) {
        return Err("an `open` record is the opener's, not the Inbox's".into());
    }
    let threaded = !matches!(n.message, notify::Message::Notify(_));
    let declares = threaded && dispatch::declares_inbox(&n.instance);
    let action = crate::inbox::decide(&n.instance, declares, n.message);
    let shown = crate::inbox::store::path()
        .and_then(|path| crate::inbox::ops::sink(&path, &n.instance_id, &n.instance, action))
        .map_err(|e| format!("{e:#}"))?;
    Ok(shown.map(|shown| (n.instance, shown)))
}

/// Handle to the bridge worker thread. Send running-container lists with
/// [`send`](Self::send); on drop the channel closes and the thread is joined,
/// so all bridges are killed before the caller (the daemon) releases its lock.
pub struct BridgeWorker {
    tx: Option<mpsc::Sender<(Vec<String>, Vec<(String, u64)>)>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl BridgeWorker {
    /// Hand the worker the current running-container set, and the containers
    /// whose bridge a client asked for since the last one (`urgent`, each with
    /// its [`BridgeBoard::ticket`]). Never blocks; a dead worker (thread gone)
    /// is silently ignored.
    pub fn send(&self, running: Vec<String>, urgent: Vec<(String, u64)>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((running, urgent));
        }
    }
}

impl Drop for BridgeWorker {
    fn drop(&mut self) {
        // Close the channel first so the worker's `recv` returns and it drops
        // `Bridges` (killing every bridge), then join so that teardown finishes
        // before the owner goes on (the daemon releasing its lock).
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::{hash, install, start_daemon};
    use std::process::Command;

    fn ok(cmd: &mut Command) -> bool {
        matches!(cmd.output(), Ok(o) if o.status.success())
    }

    /// `attach` runs the handshake on a caller-spawned child: one that dies
    /// before `Hello` fails with its stderr (the `devsbd: ` prefix dropped),
    /// and the bridge is done.
    #[test]
    fn attach_reports_a_child_that_dies_before_the_handshake() {
        let attached = |code: i32| {
            let mut child = Command::new("sh")
                // Read the Hello first: a child gone before the host's write
                // fails it with `BrokenPipe`, which skips the stderr read.
                .args(["-c", &format!("head -c 1 >/dev/null; echo 'devsbd: no daemon here' >&2; exit {code}")])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let (stdin, stdout, stderr) =
                (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
            attach(child, stdin, stdout, stderr, "", "box/web", Some("box"))
        };
        let bridge = attached(1);
        assert_eq!(bridge.outcome(Duration::from_secs(5)), Some(Err("no daemon here".into())));
        assert!(bridge.is_done());
        assert!(!bridge.is_mismatch());
        // The in-container bridge's mismatch exit is classified as one.
        let bridge = attached(proto::MISMATCH_EXIT);
        assert!(bridge.outcome(Duration::from_secs(5)).unwrap().is_err());
        assert!(bridge.is_mismatch());
    }

    /// An outdated helper's fix: a local one is a restart, a machine's an
    /// update of devsandbox there; a newer one blames this side either way.
    #[test]
    fn mismatch_messages_name_the_fix() {
        let older = proto::VersionMismatch { peer: 1, ours: 2 };
        assert_eq!(
            mismatch_message("devsandbox-web", None, &older),
            "helper in devsandbox-web is outdated (protocol 1, need 2): restart the instance"
        );
        assert_eq!(
            mismatch_message("box/web", Some("box"), &older),
            "helper in box/web is outdated (protocol 1, need 2): update devsandbox on box (and restart the instance there)"
        );
        let newer = proto::VersionMismatch { peer: 3, ours: 2 };
        assert_eq!(
            mismatch_message("box/web", Some("box"), &newer),
            "this devsandbox is older than the helper in box/web (protocol 3, need 2)"
        );
    }

    #[test]
    fn only_state_writing_ops_take_the_control_lock() {
        use crate::devsbd::control::Op;
        let locked: Vec<Op> = Op::ALL.into_iter().filter(|op| writes_state(*op)).collect();
        assert_eq!(locked, [Op::Ensure, Op::Stop, Op::Rm, Op::Done]);
    }

    #[test]
    fn retry_policy() {
        // No entry: always start one.
        assert_eq!(retry_decision(None, false), Retry::Spawn);
        // Alive (not done): keep, regardless of elapsed time.
        let (zero, day) = (Duration::ZERO, Duration::from_secs(86_400));
        assert_eq!(retry_decision(Some((false, false, zero)), false), Retry::Keep);
        assert_eq!(retry_decision(Some((false, false, day)), false), Retry::Keep);
        // Dead, not a mismatch: retried after RETRY.
        assert_eq!(retry_decision(Some((true, false, zero)), false), Retry::Keep);
        assert_eq!(retry_decision(Some((true, false, RETRY)), false), Retry::Respawn);
        // Dead on a mismatch: only after the long MISMATCH_RETRY.
        assert_eq!(retry_decision(Some((true, true, RETRY)), false), Retry::Keep);
        assert_eq!(retry_decision(Some((true, true, MISMATCH_RETRY)), false), Retry::Respawn);
        // Urgent (a client asked): a dead bridge is retried at once, mismatch
        // or not; a live one is still kept.
        assert_eq!(retry_decision(None, true), Retry::Spawn);
        assert_eq!(retry_decision(Some((true, false, zero)), true), Retry::Respawn);
        assert_eq!(retry_decision(Some((true, true, zero)), true), Retry::Respawn);
        assert_eq!(retry_decision(Some((false, false, zero)), true), Retry::Keep);
    }

    #[test]
    fn board_verdict_waits_for_the_ticket_then_the_handshake() {
        let board = BridgeBoard::default();
        let t = board.ticket();
        let at = |b: &BridgeBoard| verdict(&b.inner.0.lock().unwrap(), "c", t);
        // An old Ready entry doesn't count before the worker handled the ticket.
        board.set("c", BridgeStatus::Ready);
        assert_eq!(at(&board), None);
        let observe = board.spawning("c");
        board.handled(t);
        assert_eq!(at(&board), None, "pending");
        observe.status(BridgeStatus::Failed("helper in c is outdated".into()));
        assert_eq!(at(&board), Some(Readiness::not("helper in c is outdated")));
        // A replacement: the old bridge's late news can't overwrite it.
        let fresh = board.spawning("c");
        observe.status(BridgeStatus::Ready);
        assert_eq!(at(&board), None, "stale observer ignored");
        fresh.status(BridgeStatus::Ready);
        assert_eq!(at(&board), Some(Readiness::ready()));
        // Dropped by a reconcile: no bridge.
        board.retain(&[]);
        assert!(!at(&board).unwrap().ready);
        // Tickets grow; `handled` never goes back.
        let t2 = board.ticket();
        assert!(t2 > t);
        board.handled(t2);
        board.handled(t);
        assert_eq!(board.inner.0.lock().unwrap().handled, t2);
    }

    #[test]
    fn board_is_settled_with_every_bridge_ready() {
        let board = BridgeBoard::default();
        assert!(board.settled(), "no bridges");
        let a = board.spawning("a");
        assert!(!board.settled(), "pending");
        a.status(BridgeStatus::Ready);
        assert!(board.settled());
        let b = board.spawning("b");
        b.status(BridgeStatus::Failed("the bridge ended".into()));
        assert!(!board.settled(), "failed");
        board.retain(&["a".into()]);
        assert!(board.settled(), "the failed one dropped");
    }

    #[test]
    fn board_on_failed_fires_on_failed_only() {
        let board = BridgeBoard::default();
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&fired);
        board.on_failed(Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        let n = || fired.load(Ordering::SeqCst);
        let a = board.spawning("a");
        assert_eq!(n(), 0, "pending");
        a.status(BridgeStatus::Ready);
        assert_eq!(n(), 0, "ready");
        a.status(BridgeStatus::Failed("the bridge ended".into()));
        assert_eq!(n(), 1, "failed via the observer");
        // `reconcile`'s own `set` (the exec couldn't start): no wake, or the
        // woken pass would retry at once, in a loop.
        board.set("b", BridgeStatus::Failed("cannot run exec".into()));
        assert_eq!(n(), 1, "no wake via set");
        // A replaced bridge's late failure: no wake.
        let _fresh = board.spawning("a");
        a.status(BridgeStatus::Failed("late".into()));
        assert_eq!(n(), 1);
        // The hook runs outside the lock: it may read the board.
        let other = BridgeBoard::default();
        let seen = Arc::new(Mutex::new(None));
        let out = Arc::clone(&seen);
        let inner = other.clone();
        other.on_failed(Box::new(move || *out.lock().unwrap() = Some(inner.settled())));
        other.spawning("c").status(BridgeStatus::Failed("x".into()));
        assert_eq!(*seen.lock().unwrap(), Some(false));
    }

    /// The registry: a bridge's link is there from its handshake until it
    /// ends, is replaced, or a reconcile drops it; a replaced bridge's late
    /// link or failure doesn't touch its successor's.
    #[test]
    fn board_publishes_a_link_from_handshake_to_end() {
        let board = BridgeBoard::default();
        let mux = Mux::new(std::io::sink());
        let a = board.spawning("c");
        assert!(board.link("c").is_none(), "pending");
        a.link(&mux, caps::RPC);
        a.status(BridgeStatus::Ready);
        let link = board.link("c").unwrap();
        assert!(Arc::ptr_eq(&link.mux, &mux));
        assert_eq!(link.caps, caps::RPC);
        a.status(BridgeStatus::Failed("the bridge ended".into()));
        assert!(board.link("c").is_none(), "ended");

        let b = board.spawning("c");
        b.link(&mux, caps::RPC);
        let first = board.link("c").unwrap().attach;
        let fresh = board.spawning("c");
        assert!(board.link("c").is_none(), "the replaced bridge's link went with it");
        b.link(&mux, caps::RPC);
        assert!(board.link("c").is_none(), "a late link from the replaced bridge");
        fresh.link(&mux, caps::RPC);
        assert_ne!(board.link("c").unwrap().attach, first, "a new attach");
        b.status(BridgeStatus::Failed("late".into()));
        assert!(board.link("c").is_some(), "a late failure from the replaced bridge");

        let other = board.spawning("d");
        other.link(&mux, 0);
        board.set("d", BridgeStatus::Failed("cannot run exec".into()));
        assert!(board.link("d").is_none());
        board.retain(&[]);
        assert!(board.link("c").is_none(), "dropped by a reconcile");
    }

    #[test]
    fn a_link_without_the_rpc_cap_is_unsupported() {
        let link = Link { mux: Mux::new(std::io::sink()), caps: caps::TCP_FORWARD, attach: 1 };
        let err = link.open_rpc(|_| {}).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn board_wait_wakes_on_news_and_times_out_with_a_reason() {
        let board = BridgeBoard::default();
        let t = board.ticket();
        let short = Duration::from_millis(30);
        assert!(board.wait("c", t, short).error.unwrap().contains("didn't get to it"));
        let observe = board.spawning("c");
        board.handled(t);
        assert!(board.wait("c", t, short).error.unwrap().contains("handshake didn't finish"));
        let started = Instant::now();
        let waiter = {
            let board = board.clone();
            std::thread::spawn(move || board.wait("c", t, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(50));
        observe.status(BridgeStatus::Ready);
        assert_eq!(waiter.join().unwrap(), Readiness::ready());
        assert!(started.elapsed() < Duration::from_secs(5), "woken, not timed out");
    }

    #[test]
    fn agent_candidates_are_newest_first_deduped_and_bounded() {
        let mut c = AgentCandidates::default();
        c.report("/a".into());
        c.report("/b".into());
        c.report("/a".into());
        assert_eq!(c.recent, [PathBuf::from("/a"), PathBuf::from("/b")], "a repeat moves to the front");
        for i in 0..20 {
            c.report(format!("/n{i}").into());
        }
        assert_eq!(c.recent.len(), MAX_AGENTS);
        assert_eq!(c.recent[0], PathBuf::from("/n19"));
        assert!(!c.recent.contains(&PathBuf::from("/a")), "the oldest is forgotten");
    }

    #[test]
    fn agent_pick_takes_the_first_live_one_then_the_fallback() {
        let live = |names: &'static [&'static str]| move |p: &Path| names.iter().any(|n| p == Path::new(n));
        let mut c = AgentCandidates::default();
        // Nothing reported: the own `$SSH_AUTH_SOCK`, when live.
        assert_eq!(c.pick(vec![], Some("/env".into()), live(&["/env"])), Some("/env".into()));
        assert_eq!(c.pick(vec![], Some("/env".into()), live(&[])), None);
        assert_eq!(c.pick(vec![], None, live(&["/env"])), None);
        c.report("/old".into());
        c.report("/new".into());
        // The newest live report wins over older ones and the fallback.
        assert_eq!(c.pick(vec![], Some("/env".into()), live(&["/old", "/new", "/env"])), Some("/new".into()));
        // A dead newest (the `ssh -A` session ended) falls through in order.
        assert_eq!(c.pick(vec![], Some("/env".into()), live(&["/old", "/env"])), Some("/old".into()));
        assert_eq!(c.pick(vec![], Some("/env".into()), live(&["/env"])), Some("/env".into()));
        assert_eq!(c.pick(vec![], Some("/env".into()), live(&[])), None);
    }

    #[test]
    fn agent_pick_puts_relays_between_reports_and_the_fallback() {
        let live = |names: &'static [&'static str]| move |p: &Path| names.iter().any(|n| p == Path::new(n));
        let relays = || vec![PathBuf::from("/r-new"), PathBuf::from("/r-old")];
        let mut c = AgentCandidates::default();
        // No report: the newest live relay, over older ones and the fallback.
        assert_eq!(c.pick(relays(), Some("/env".into()), live(&["/r-new", "/r-old", "/env"])), Some("/r-new".into()));
        assert_eq!(c.pick(relays(), Some("/env".into()), live(&["/r-old", "/env"])), Some("/r-old".into()));
        // An exited relay (its socket refuses) drops out.
        assert_eq!(c.pick(relays(), Some("/env".into()), live(&["/env"])), Some("/env".into()));
        // A live report comes first.
        c.report("/rep".into());
        assert_eq!(c.pick(relays(), Some("/env".into()), live(&["/rep", "/r-new", "/env"])), Some("/rep".into()));
        assert_eq!(c.pick(relays(), Some("/env".into()), live(&["/r-new", "/env"])), Some("/r-new".into()));
    }

    #[test]
    fn agent_live_needs_a_listening_socket() {
        let dir = std::env::temp_dir().join(format!("devsbd-agentlive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("agent.sock");
        assert!(!agent_live(&sock), "missing");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(agent_live(&sock), "listening");
        drop(listener);
        // Polled: another test forking a child (`pre_exec` forces fork) can
        // hold the listener fd for the moment until that child execs.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while agent_live(&sock) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!agent_live(&sock), "a leftover socket file is dead");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caps_advertise_notify_and_control_only_with_a_sink() {
        const SERVED: u32 = caps::NOTIFY | caps::CONTROL;
        assert_eq!(own_caps(false, false, false), 0);
        assert_eq!(own_caps(true, false, false), caps::SSH_AGENT);
        assert_eq!(own_caps(false, true, false), SERVED);
        assert_eq!(own_caps(true, true, false), caps::SSH_AGENT | SERVED);
        assert_eq!(own_caps(false, true, true), SERVED | caps::HTTP_PROXY);
        assert_eq!(own_caps(true, false, true), caps::SSH_AGENT, "a broker serves only daemon bridges");
        // `RPC` is a daemon cap: the host opens those streams, never serves them.
        for (agent, sink) in [(false, false), (true, false), (false, true), (true, true)] {
            assert_eq!(own_caps(agent, sink, true) & caps::RPC, 0);
        }
    }

    /// A [`Broker`] that reports each connection's caller and header, then
    /// echoes the client's bytes upper-cased until it half-closes.
    struct EchoBroker(Mutex<mpsc::Sender<(Caller, proto::ProxyHeader)>>);

    impl Broker for EchoBroker {
        fn serve(&self, caller: &Caller, header: proto::ProxyHeader, mut stream: UnixStream) {
            let _ = self.0.lock().unwrap().send((caller.clone(), header));
            let mut body = Vec::new();
            if stream.read_to_end(&mut body).is_ok() {
                let _ = stream.write_all(&body.to_ascii_uppercase());
            }
        }
    }

    fn echo_broker() -> (Arc<dyn Broker>, mpsc::Receiver<(Caller, proto::ProxyHeader)>) {
        let (tx, rx) = mpsc::channel();
        (Arc::new(EchoBroker(Mutex::new(tx))), rx)
    }

    /// What the helper's broker listener does with a client: `Open(HTTP_PROXY)`
    /// with `header` first, then `bytes` and its half-close; returns everything
    /// the host wrote back before closing.
    fn send_proxied(daemon: &Arc<Mux>, stream: u32, header: &proto::ProxyHeader, bytes: &[u8]) -> Vec<u8> {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        daemon.attach_with_header(stream, theirs, proto::channel::HTTP_PROXY, &header.encode()).unwrap();
        ours.write_all(bytes).unwrap();
        ours.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        ours.read_to_end(&mut reply).unwrap();
        reply
    }

    #[test]
    fn proxy_stream_reaches_the_broker_as_its_bridges_instance() {
        let (broker, calls) = echo_broker();
        let (to, _rx) = services("web-1", no_control());
        let daemon = pair_with(Services { broker: Some(broker), ..to });
        let header = proto::ProxyHeader::BaseUrl { upstream: "sentry.io".into(), listener: 41001 };
        // Past one window, so the header's credit and the flow both count.
        let body: Vec<u8> = (0..proto::INITIAL_WINDOW as usize * 2).map(|i| b'a' + (i % 26) as u8).collect();
        let reply = send_proxied(&daemon, 1, &header, &body);
        assert_eq!(reply, body.to_ascii_uppercase());
        let (caller, got) = calls.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(caller, Caller { instance: "web-1".into(), instance_id: "web-1-id".into() });
        assert_eq!(got, header);
    }

    #[test]
    fn proxy_stream_is_refused_without_a_broker_or_with_a_bad_header() {
        // No body: a refusal can close our peer before a write, failing it
        // (EPIPE) or resetting the read (ECONNRESET, unread data dropped).
        fn refused(daemon: &Arc<Mux>, header: &[u8]) -> Vec<u8> {
            let (mut ours, theirs) = UnixStream::pair().unwrap();
            ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            daemon.attach_with_header(1, theirs, proto::channel::HTTP_PROXY, header).unwrap();
            let _ = ours.shutdown(std::net::Shutdown::Write);
            let mut reply = Vec::new();
            ours.read_to_end(&mut reply).unwrap();
            reply
        }

        let (to, _rx) = services("web-1", no_control());
        let daemon = pair_with(to);
        let header = proto::ProxyHeader::BaseUrl { upstream: "sentry.io".into(), listener: 41001 };
        assert!(refused(&daemon, &header.encode()).is_empty());

        let (broker, calls) = echo_broker();
        let (to, _rx) = services("web-1", no_control());
        let daemon = pair_with(Services { broker: Some(broker), ..to });
        assert!(refused(&daemon, b"socks5 sentry.io 1\n").is_empty());
        assert!(calls.try_recv().is_err(), "the broker never sees a bad header");
    }

    /// Past `MAX_BROKER_HANDLERS` live broker handlers an `HTTP_PROXY` `Open`
    /// is refused; notify and control keep their own slots.
    #[test]
    fn broker_slots_bound_proxy_opens_apart_from_notify() {
        let (broker, _calls) = echo_broker();
        let (to, _rx) = services("web-1", no_control());
        let to = Services { broker: Some(broker), ..to };
        let none: Option<fn() -> Option<PathBuf>> = None;
        let held: Vec<_> = (0..MAX_BROKER_HANDLERS)
            .map(|_| HandlerSlot::acquire(&to.brokered, MAX_BROKER_HANDLERS).unwrap())
            .collect();
        assert!(open_stream(proto::channel::HTTP_PROXY, none.as_ref(), None, Some(&to)).is_none());
        assert!(open_stream(proto::channel::NOTIFY, none.as_ref(), None, Some(&to)).is_some());
        drop(held);
        assert!(open_stream(proto::channel::HTTP_PROXY, none.as_ref(), None, Some(&to)).is_some());
    }

    /// `Services` for instance `instance` (owner id `<instance>-id`) whose
    /// notifications go to the returned channel and whose control handler is
    /// `control`. `accept` = false makes the sink fail, as a store write that
    /// won't go through does.
    fn services_with(
        instance: &str,
        control: ControlHandler,
        accept: bool,
    ) -> (Services, mpsc::Receiver<Notification>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |n| {
            if !accept {
                return Err("store is unhappy".into());
            }
            let _ = tx.lock().unwrap().send(n);
            Ok(())
        });
        (Services::new(instance.into(), format!("{instance}-id"), sink, control), rx)
    }

    fn services(instance: &str, control: ControlHandler) -> (Services, mpsc::Receiver<Notification>) {
        services_with(instance, control, true)
    }

    /// A control handler that must not be called.
    fn no_control() -> ControlHandler {
        Arc::new(|_: &str, _: &Request| panic!("control handler called"))
    }

    #[test]
    fn wanted_bridges() {
        // No sink: exactly the agent relay (relay mode + host agent).
        for helper in [false, true] {
            assert_eq!(wanted(helper, false, true, false), None);
            assert_eq!(wanted(helper, false, false, false), None);
        }
        assert_eq!(wanted(true, true, true, false), Some(true));
        assert_eq!(wanted(true, true, false, false), None);
        // Sink: every helper-capable instance, agent only where relayed.
        assert_eq!(wanted(true, false, false, true), Some(false));
        assert_eq!(wanted(true, true, false, true), Some(false));
        assert_eq!(wanted(true, false, true, true), Some(false));
        assert_eq!(wanted(true, true, true, true), Some(true));
        assert_eq!(wanted(false, false, true, true), None);
    }

    /// A daemon-side mux and a host bridge's `open_stream` joined by pipes;
    /// the host has a notify sink for instance `web-1` and no agent.
    fn notify_pair() -> (Arc<Mux>, mpsc::Receiver<Notification>) {
        service_pair_with(no_control(), true)
    }

    /// `notify_pair` with control requests going to `control`.
    fn service_pair(control: ControlHandler) -> (Arc<Mux>, mpsc::Receiver<Notification>) {
        service_pair_with(control, true)
    }

    /// `notify_pair` whose sink refuses every record when `accept` is false.
    fn service_pair_with(
        control: ControlHandler,
        accept: bool,
    ) -> (Arc<Mux>, mpsc::Receiver<Notification>) {
        let (to, rx) = services_with("web-1", control, accept);
        (pair_with(to), rx)
    }

    /// A daemon-side mux and a host bridge serving `to`, joined by pipes.
    fn pair_with(to: Services) -> Arc<Mux> {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || d.serve(d_r, |_, _| None));
        std::thread::spawn(move || {
            let none: Option<fn() -> Option<PathBuf>> = None;
            host.serve(h_r, move |_, ch| open_stream(ch, none.as_ref(), None, Some(&to)))
        });
        daemon
    }

    /// What the daemon's `send_record` does: `Open(NOTIFY)`, the bytes as one
    /// `Data`, `Eof`; returns everything the host wrote back before closing.
    fn send_notify(daemon: &Arc<Mux>, stream: u32, bytes: &[u8]) -> Vec<u8> {
        send_on(daemon, stream, proto::channel::NOTIFY, bytes)
    }

    /// `send_notify` on any channel.
    fn send_on(daemon: &Arc<Mux>, stream: u32, channel: u8, bytes: &[u8]) -> Vec<u8> {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        daemon.attach(stream, theirs, Some(channel)).unwrap();
        daemon.send(&proto::Frame::Data { stream, bytes: bytes.to_vec() }).unwrap();
        daemon.send(&proto::Frame::Eof { stream }).unwrap();
        let mut reply = Vec::new();
        ours.read_to_end(&mut reply).unwrap();
        reply
    }

    #[test]
    fn notify_stream_replies_ok_and_delivers() {
        let (daemon, rx) = notify_pair();
        let message = notify::Message::Notify(notify::Record {
            level: notify::Level::Warn,
            key: Some("pr-1".into()),
            link: None,
            msg: "PR 1\nneeds you".into(),
            at: 7,
        });
        let reply = send_notify(&daemon, 1, notify::encode(&message).as_bytes());
        assert!(reply.starts_with(notify::REPLY_OK), "reply: {reply:?}");
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            got,
            Notification { instance: "web-1".into(), instance_id: "web-1-id".into(), message }
        );
    }

    /// A thread verb reaches the sink as a `ThreadPut`, undecoded: the bridge
    /// never parses the body, `inbox::decide` does.
    #[test]
    fn thread_stream_replies_ok_and_delivers() {
        let (daemon, rx) = notify_pair();
        let message = notify::Message::ThreadPut {
            at: 7,
            key: "pr-1".into(),
            body: r#"{"key":"pr-1"}"#.into(),
        };
        let reply = send_notify(&daemon, 1, notify::encode(&message).as_bytes());
        assert!(reply.starts_with(notify::REPLY_OK), "reply: {reply:?}");
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got.message, message);
    }

    #[test]
    fn bad_or_oversized_notify_gets_no_ok_and_is_not_delivered() {
        let (daemon, rx) = notify_pair();
        let reply = send_notify(&daemon, 1, b"garbage");
        assert!(!reply.starts_with(notify::REPLY_OK), "reply: {reply:?}");
        let big = vec![b'x'; notify::MAX_RECORD + NOTIFY_SLACK + 1];
        let reply = send_notify(&daemon, 3, &big);
        assert!(reply.is_empty(), "oversized: closed without a reply");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "nothing delivered");
    }

    /// The production sink takes an `open` record before the Inbox: a fresh
    /// `https` one reaches the opener, an `ssh` one and a stale one only the
    /// log and a refused `on_open_done`, and each gets `ok` (taken). A plain
    /// notify still goes to `apply`.
    #[test]
    fn open_records_reach_the_opener_never_the_inbox() {
        static APPLIED: Mutex<Vec<notify::Message>> = Mutex::new(Vec::new());
        static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn apply(n: Notification) -> Result<Option<(String, crate::inbox::Shown)>, String> {
            APPLIED.lock().unwrap().push(n.message);
            Ok(None)
        }
        let (tx, opened) = mpsc::channel();
        let tx = Mutex::new(tx);
        let dones: Arc<Mutex<Vec<crate::open::OpenDone>>> = Arc::default();
        let d = Arc::clone(&dones);
        let hooks = SinkHooks {
            on_shown: None,
            opener: Arc::new(move |instance: &str, instance_id: &str, url: &str| {
                let _ = tx.lock().unwrap().send((instance.to_string(), instance_id.to_string(), url.to_string()));
            }),
            on_open_done: Some(Arc::new(move |done| d.lock().unwrap().push(done))),
            log: |line| LOGGED.lock().unwrap().push(line.to_string()),
        };
        let to = Services::new("web-1".into(), "web-1-id".into(), sink(apply, hooks), no_control());
        let daemon = pair_with(to);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let send = |stream: u32, m: notify::Message| {
            let reply = send_notify(&daemon, stream, notify::encode(&m).as_bytes());
            assert!(reply.starts_with(notify::REPLY_OK), "reply: {reply:?}");
        };
        send(1, notify::Message::Open { at: now, url: "https://example.test/a".into() });
        assert_eq!(
            opened.recv_timeout(Duration::from_secs(5)).unwrap(),
            ("web-1".to_string(), "web-1-id".to_string(), "https://example.test/a".to_string())
        );
        send(3, notify::Message::Open { at: now, url: "ssh://example.test".into() });
        send(5, notify::Message::Open { at: now - 3600, url: "https://example.test/old".into() });
        assert!(opened.recv_timeout(Duration::from_millis(200)).is_err(), "refused ones aren't opened");
        let logged = LOGGED.lock().unwrap().clone();
        assert_eq!(logged.len(), 2, "{logged:?}");
        assert!(logged[0].starts_with("web-1: open refused, scheme `ssh`") && logged[0].ends_with("ssh://example.test"));
        assert!(logged[1].starts_with("web-1: open dropped") && logged[1].ends_with("https://example.test/old"));
        // Refusals at intake are reported; the opened one is the opener's to
        // report.
        let dones = dones.lock().unwrap().clone();
        assert_eq!(dones.len(), 2, "{dones:?}");
        assert_eq!((dones[0].url.as_str(), dones[0].kind()), ("ssh://example.test", "refused"));
        assert_eq!(dones[0].line(), "web-1: open refused: scheme `ssh` (http and https only)");
        assert_eq!((dones[1].url.as_str(), dones[1].kind(), dones[1].profile.as_deref()), ("https://example.test/old", "refused", None));
        assert!(dones[1].line().starts_with("web-1: open refused: queued 360"), "{}", dones[1].line());
        assert!(APPLIED.lock().unwrap().is_empty(), "no open reached the Inbox");
        let note = notify::Message::Notify(notify::Record {
            level: notify::Level::Info,
            key: None,
            link: None,
            msg: "m".into(),
            at: now,
        });
        send(7, note.clone());
        assert_eq!(*APPLIED.lock().unwrap(), vec![note]);
    }

    /// A record the store won't take is one log line per distinct error
    /// while the helper resends it, and a success resets that.
    #[test]
    fn a_failing_store_is_logged_once_per_reason() {
        static FAIL: Mutex<Option<String>> = Mutex::new(None);
        static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn apply(_: Notification) -> Result<Option<(String, crate::inbox::Shown)>, String> {
            FAIL.lock().unwrap().clone().map_or(Ok(None), Err)
        }
        let hooks = SinkHooks {
            on_shown: None,
            opener: Arc::new(|_: &str, _: &str, _: &str| {}),
            on_open_done: None,
            log: |line| LOGGED.lock().unwrap().push(line.to_string()),
        };
        let sink = sink(apply, hooks);
        let note = || Notification {
            instance: "web-1".into(),
            instance_id: "web-1-id".into(),
            message: notify::Message::Notify(notify::Record {
                level: notify::Level::Info,
                key: None,
                link: None,
                msg: "m".into(),
                at: 1,
            }),
        };
        *FAIL.lock().unwrap() = Some("disk full".into());
        assert!(sink(note()).is_err());
        assert!(sink(note()).is_err());
        *FAIL.lock().unwrap() = Some("locked\nout".into());
        assert!(sink(note()).is_err());
        *FAIL.lock().unwrap() = None;
        assert!(sink(note()).is_ok());
        *FAIL.lock().unwrap() = Some("disk full".into());
        assert!(sink(note()).is_err());
        let logged = LOGGED.lock().unwrap().clone();
        let why = "web-1: notification not stored (the helper keeps it and resends): ";
        assert_eq!(
            logged,
            vec![format!("{why}disk full"), format!("{why}{}", crate::open::printable("locked\nout")), format!("{why}disk full")]
        );
    }

    /// A bridge failing on the same reason at every retry is logged once; a
    /// new reason is logged, and so is the first recovery after a failure.
    #[test]
    fn bridge_status_lines_skip_repeats() {
        let logged = Dedupe::default();
        let failed = |why: &str| BridgeStatus::Failed(why.into());
        let line = |s: &BridgeStatus| status_line(&logged, "c", s);
        assert_eq!(line(&BridgeStatus::Ready), None, "no failure logged yet");
        assert_eq!(line(&BridgeStatus::Pending), None);
        assert_eq!(line(&failed("no daemon")).as_deref(), Some("bridge c: no daemon"));
        assert_eq!(line(&BridgeStatus::Pending), None);
        assert_eq!(line(&failed("no daemon")), None);
        assert_eq!(line(&failed("timed out")).as_deref(), Some("bridge c: timed out"));
        assert_eq!(line(&BridgeStatus::Ready).as_deref(), Some("bridge c: ready"));
        assert_eq!(line(&BridgeStatus::Ready), None);
        assert_eq!(line(&failed("timed out")).as_deref(), Some("bridge c: timed out"), "a new failure");
        logged.retain(&[]);
        assert_eq!(line(&BridgeStatus::Ready), None, "a gone container's failure is forgotten");
    }

    #[test]
    fn a_missing_helper_is_retried_once_per_appearance() {
        let state: crate::state::State = toml::from_str(
            "[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
        )
        .unwrap();
        let mut info = state.instances["a"].clone();
        let mut tried = std::collections::HashSet::new();
        assert!(heal_now(&mut tried, &info, false));
        assert!(!heal_now(&mut tried, &info, false), "not again while it stays up");
        tried.clear(); // what `reconcile` does once `c` leaves the running list
        assert!(!heal_now(&mut tried, &info, true), "`run` has it in flight");
        info.devsbd_arch = Some(crate::devsbd::Arch::X86_64);
        assert!(!heal_now(&mut tried, &info, false), "recorded with the helper");
        info.devsbd_arch = None;
        assert!(heal_now(&mut tried, &info, false), "back after a stop, and once `run` is done");
    }

    /// `apply_message` refuses an `open` record outright (the sink never
    /// hands it one), before touching the store.
    #[test]
    fn apply_message_refuses_an_open() {
        let n = Notification {
            instance: "web-1".into(),
            instance_id: "web-1-id".into(),
            message: notify::Message::Open { at: 1, url: "https://x".into() },
        };
        assert!(apply_message(n).unwrap_err().contains("opener"));
    }

    /// Apply, then ack: a record the sink won't take (the Inbox store failed
    /// to write) gets no `ok`, so the daemon keeps it and resends.
    #[test]
    fn a_failing_sink_gets_no_ok() {
        let (daemon, _rx) = service_pair_with(no_control(), false);
        let message = notify::Message::Notify(notify::Record {
            level: notify::Level::Info,
            key: None,
            link: None,
            msg: "m".into(),
            at: 1,
        });
        let reply = send_notify(&daemon, 1, notify::encode(&message).as_bytes());
        assert!(reply.is_empty(), "reply: {reply:?}");
    }

    /// A sink-less bridge refuses notify and control streams (it never
    /// advertises either, but an old/confused daemon must still get no reply).
    #[test]
    fn sink_less_refuses_notify_and_control() {
        let none: Option<fn() -> Option<PathBuf>> = None;
        assert!(open_stream(proto::channel::NOTIFY, none.as_ref(), None, None).is_none());
        assert!(open_stream(proto::channel::CONTROL, none.as_ref(), None, None).is_none());
        assert!(open_stream(proto::channel::SSH_AGENT, none.as_ref(), None, None).is_none());
        assert!(open_stream(proto::channel::HTTP_PROXY, none.as_ref(), None, None).is_none());
    }

    /// Past `MAX_HANDLERS` live handler threads a notify/control `Open` is
    /// refused without spawning; a finished handler frees its slot. Agent
    /// streams don't take slots.
    #[test]
    fn handler_slots_bound_notify_and_control_opens() {
        let (to, _rx) = services("web-1", no_control());
        let held: Vec<_> = (0..MAX_HANDLERS).map(|_| HandlerSlot::acquire(&to.handlers, MAX_HANDLERS).unwrap()).collect();
        let none: Option<fn() -> Option<PathBuf>> = None;
        assert!(open_stream(proto::channel::NOTIFY, none.as_ref(), None, Some(&to)).is_none());
        assert!(open_stream(proto::channel::CONTROL, none.as_ref(), None, Some(&to)).is_none());
        assert_eq!(to.handlers.load(Ordering::Acquire), MAX_HANDLERS);
        drop(held);
        assert_eq!(to.handlers.load(Ordering::Acquire), 0);
        // Accepted again; dropping our end ends the handler, freeing the slot.
        let conn = open_stream(proto::channel::NOTIFY, none.as_ref(), None, Some(&to));
        assert!(conn.is_some());
        drop(conn);
        let mut freed = false;
        for _ in 0..100 {
            if to.handlers.load(Ordering::Acquire) == 0 {
                freed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(freed, "handler thread released its slot");
    }

    /// A control stream reaches the handler with this bridge's instance key
    /// and the decoded request; its response comes back encoded. A bad
    /// request never reaches the handler and gets `Usage`.
    #[test]
    fn control_stream_runs_the_handler_and_replies() {
        let (tx, calls) = mpsc::channel();
        let tx = Mutex::new(tx);
        let control: ControlHandler = Arc::new(move |key: &str, req: &Request| {
            let _ = tx.lock().unwrap().send((key.to_string(), req.clone()));
            Response::new(Status::Ok, "web-pr-1")
        });
        let (daemon, _notes) = service_pair(control);
        let req = Request {
            sandbox: Some("web".into()),
            key: Some("pr-1".into()),
            ..Request::new(control::Op::Ensure)
        };
        let reply = send_on(&daemon, 1, proto::channel::CONTROL, control::encode_request(&req).as_bytes());
        assert_eq!(reply, b"status ok\nbody web-pr-1\n");
        assert_eq!(calls.recv_timeout(Duration::from_secs(5)).unwrap(), ("web-1".to_string(), req));

        let reply = send_on(&daemon, 3, proto::channel::CONTROL, b"op boot\n");
        let resp = control::decode_response(std::str::from_utf8(&reply).unwrap()).unwrap();
        assert_eq!(resp.status, Status::Usage);
        assert!(resp.body.contains("bad op"), "{}", resp.body);
        let big = vec![b'x'; control::MAX_REQUEST + 1];
        let reply = send_on(&daemon, 5, proto::channel::CONTROL, &big);
        assert!(std::str::from_utf8(&reply).unwrap().starts_with("status usage\n"));
        assert!(calls.recv_timeout(Duration::from_millis(200)).is_err(), "handler not called");
    }

    /// The worker reconciles against only the newest queued list. With no host
    /// agent every reconcile is a cheap no-op (no `exec`), so this exercises the
    /// send/coalesce/join path without docker.
    #[test]
    fn worker_coalesces_and_joins() {
        let worker = Bridges::spawn_worker(None, None, BridgeBoard::default());
        for i in 0..100 {
            worker.send(vec![format!("devsandbox-c{i}")], vec![(format!("devsandbox-c{i}"), i)]);
        }
        // Dropping joins the thread; it must have drained without deadlock.
        drop(worker);
    }

    /// Docker-gated end to end: a real `ssh-add -l` in an alpine container
    /// lists a key held by a host `ssh-agent`, through daemon + bridge.
    /// Also checks the daemon falls back to the older bridge when the newest
    /// one ends. Skips (fails on CI) without docker, an embedded helper, host
    /// ssh-agent/ssh-keygen, or network for `apk add`.
    #[test_utils::docker_test(helper)]
    fn relays_host_agent_into_container_with_docker() -> Result<(), &'static str> {
        if !ok(Command::new("ssh-agent").arg("-h")) && !ok(Command::new("which").arg("ssh-agent")) {
            return Err("no host ssh-agent");
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("devsbd-relay-{stamp}"));
        std::fs::create_dir_all(&tmp).unwrap();
        let sock = tmp.join("agent.sock");
        let key = tmp.join("id");
        let name = format!("devsandbox-relay-test-{stamp}");

        let mut agent = Command::new("ssh-agent").arg("-D").arg("-a").arg(&sock).stdout(Stdio::null()).spawn().unwrap();
        let cleanup = || {
            let _ = agent.kill();
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
            let _ = std::fs::remove_dir_all(&tmp);
        };
        crate::test_support::with_cleanup(cleanup, || {
            for _ in 0..100 {
                if sock.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(ok(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "devsbd-relay-test", "-f"])
                .arg(&key)));
            assert!(ok(Command::new("ssh-add").arg(&key).env("SSH_AUTH_SOCK", &sock)));

            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            if !ok(crate::test_support::runtime_cmd().args(["exec", &name, "apk", "add", "--no-cache", "-q", "openssh-client-default"])) {
                return Err("apk add failed (no network?)");
            }
            let arch = install(&name, None).unwrap();
            let bridge = || {
                let agent_path = sock.clone();
                spawn_with(&name, hash(arch).unwrap(), Some(move || Some(agent_path.clone())), None).unwrap()
            };
            let ssh_add = || {
                crate::test_support::runtime_cmd()
                    .args(["exec", "-e", "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock", &name, "ssh-add", "-l"])
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
            };
            let list = || String::from_utf8_lossy(&ssh_add().wait_with_output().unwrap().stdout).into_owned();
            let pidfile = || {
                let out = crate::test_support::runtime_cmd().args(["exec", &name, "cat", "/run/devsandbox/devsbd.pid"]).output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };

            // No daemon yet: the bridge self-heals by starting one, so the
            // handshake succeeds and the key lists — no `start_daemon` first.
            let boot = bridge();
            assert_eq!(boot.outcome(Duration::from_secs(10)), Some(Ok(())), "self-started daemon");
            assert!(list().contains("devsbd-relay-test"), "via self-started daemon");
            drop(boot);
            // The daemon outlives the bridge that started it (own process
            // group), so it's still routing once `boot` is gone.

            // start_daemon on the running daemon is a no-op takeover (same
            // build → pidfile lock held → the new process exits).
            start_daemon(&name);
            // Optimistic start: the client connects before any bridge is
            // attached and is held until one attaches.
            let early = ssh_add();
            std::thread::sleep(Duration::from_millis(300));
            let long = bridge();
            let out = String::from_utf8_lossy(&early.wait_with_output().unwrap().stdout).into_owned();
            assert!(out.contains("devsbd-relay-test"), "held client: {out}");
            assert_eq!(long.outcome(Duration::from_secs(5)), Some(Ok(())));

            let short = bridge();
            assert_eq!(short.outcome(Duration::from_secs(5)), Some(Ok(())));
            assert!(list().contains("devsbd-relay-test"), "via newest bridge");
            drop(short);
            // The daemon notices the dropped bridge asynchronously.
            let mut fell_back = false;
            for _ in 0..50 {
                if list().contains("devsbd-relay-test") {
                    fell_back = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(fell_back, "daemon fell back to the older bridge");
            assert!(!long.is_done());

            // Takeover: pretend the running daemon is another build; a new
            // daemon makes it quit (its bridges end) and replaces it.
            let old = pidfile();
            let old_pid = old.split_whitespace().next().unwrap().to_string();
            let fake = format!("printf '{old_pid} outdated\\n' > /run/devsandbox/devsbd.pid");
            assert!(ok(crate::test_support::runtime_cmd().args(["exec", &name, "sh", "-c", &fake])));
            start_daemon(&name);
            let mut replaced = false;
            for _ in 0..50 {
                let now = pidfile();
                if !now.is_empty() && !now.starts_with(&format!("{old_pid} ")) {
                    assert!(now.ends_with(hash(arch).unwrap()), "{now}");
                    replaced = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(replaced, "new daemon took over");
            for _ in 0..50 {
                if long.is_done() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(long.is_done(), "old daemon's bridge ended");
            let fresh = bridge();
            assert_eq!(fresh.outcome(Duration::from_secs(5)), Some(Ok(())));
            assert!(list().contains("devsbd-relay-test"), "via new daemon");
            // Same build again: a second daemon leaves the running one alone.
            let before = pidfile();
            start_daemon(&name);
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(pidfile(), before);

            // Kill the daemon (as a container restart would) with no
            // start_daemon: the next bridge revives it and the key lists.
            drop(fresh);
            let killed = "kill $(cut -d' ' -f1 /run/devsandbox/devsbd.pid)";
            assert!(ok(crate::test_support::runtime_cmd().args(["exec", &name, "sh", "-c", killed])));
            let revived = bridge();
            assert_eq!(revived.outcome(Duration::from_secs(10)), Some(Ok(())), "revived daemon");
            assert!(list().contains("devsbd-relay-test"), "via revived daemon");
            Ok(())
        })
    }

    /// Docker-gated: an agent-less bridge (a `devsandbox port` forward) spawned
    /// *after* an agent bridge must not steal agent routing — `ssh-add -l` still
    /// lists the key via the older agent bridge. Regression for the routing fix
    /// in docs/architecture/helper/ssh-agent.md (a shell with no agent starting a forward
    /// would otherwise become the newest bridge and break ssh in the container).
    #[test_utils::docker_test(helper)]
    fn agent_less_bridge_does_not_steal_agent_routing_with_docker() -> Result<(), &'static str> {
        if !ok(Command::new("ssh-agent").arg("-h")) && !ok(Command::new("which").arg("ssh-agent")) {
            return Err("no host ssh-agent");
        }
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let tmp = std::env::temp_dir().join(format!("devsbd-agentroute-{stamp}"));
        std::fs::create_dir_all(&tmp).unwrap();
        let sock = tmp.join("agent.sock");
        let key = tmp.join("id");
        let name = format!("devsandbox-agentroute-test-{stamp}");
        let mut agent = Command::new("ssh-agent").arg("-D").arg("-a").arg(&sock).stdout(Stdio::null()).spawn().unwrap();
        let cleanup = || {
            let _ = agent.kill();
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
            let _ = std::fs::remove_dir_all(&tmp);
        };
        crate::test_support::with_cleanup(cleanup, || {
            for _ in 0..100 {
                if sock.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(ok(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "devsbd-agentroute", "-f"])
                .arg(&key)));
            assert!(ok(Command::new("ssh-add").arg(&key).env("SSH_AUTH_SOCK", &sock)));
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            if !ok(crate::test_support::runtime_cmd().args(["exec", &name, "apk", "add", "--no-cache", "-q", "openssh-client-default"])) {
                return Err("apk add failed (no network?)");
            }
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let list = || {
                let out = crate::test_support::runtime_cmd()
                    .args(["exec", "-e", "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock", &name, "ssh-add", "-l"])
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).into_owned()
            };

            // The agent bridge attaches first and lists the key.
            let agent_path = sock.clone();
            let agent_bridge = spawn_with(&name, hash(arch).unwrap(), Some(move || Some(agent_path.clone())), None).unwrap();
            assert_eq!(agent_bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "agent bridge");
            assert!(list().contains("devsbd-agentroute"), "via agent bridge");

            // Newer agent-less bridges attach while `ssh-add -l` clients keep
            // arriving. The dangerous window is between the bridge registering
            // with the daemon and its host's `Caps(0)` landing (the bridge↔host
            // handshake sits in between): the daemon must treat it as pending,
            // not capable, or those clients hit a host that refuses them. Each
            // round fires clients staggered across the spawn; every one must
            // list the key via the older agent bridge.
            let mut port_bridges = Vec::new();
            for round in 0..6 {
                let outputs = std::thread::scope(|s| {
                    let clients: Vec<_> = (0..12)
                        .map(|i| {
                            let list = &list;
                            s.spawn(move || {
                                std::thread::sleep(Duration::from_millis(15 * i));
                                list()
                            })
                        })
                        .collect();
                    let none: Option<fn() -> Option<PathBuf>> = None;
                    let port_bridge = spawn_with(&name, hash(arch).unwrap(), none, None).unwrap();
                    assert_eq!(port_bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "agent-less bridge");
                    port_bridges.push(port_bridge);
                    clients.into_iter().map(|c| c.join().unwrap()).collect::<Vec<_>>()
                });
                for (i, out) in outputs.iter().enumerate() {
                    assert!(out.contains("devsbd-agentroute"), "round {round} client {i}: agent-less bridge stole routing: {out:?}");
                }
            }
            // Settled (every `Caps(0)` in): still routed to the agent bridge.
            assert!(list().contains("devsbd-agentroute"), "agent-less bridge stole routing");
            Ok(())
        })
    }

    /// Docker-gated end to end: `devsbd notify` in a container reaches a
    /// sink-bearing (agent-less) host bridge, and the outbox is emptied once
    /// the host replied `ok`.
    #[test_utils::docker_test(helper)]
    fn delivers_container_notify_to_the_sink_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-notify-test-{stamp}");
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let (to, rx) = services("notify-test", no_control());
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, Some(to)).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            assert!(ok(crate::test_support::runtime_cmd().args(["exec", &name, BIN, "notify", "--key", "k", "hello"])));
            let got = rx.recv_timeout(Duration::from_secs(15)).expect("notification delivered");
            assert_eq!(got.instance, "notify-test");
            assert_eq!(
                got.message,
                notify::Message::Notify(notify::Record {
                    level: notify::Level::Info,
                    key: Some("k".into()),
                    link: None,
                    msg: "hello".into(),
                    at: got.message.at(),
                })
            );
            // The daemon deletes the file after reading the `ok` the sink
            // already earned, which races this check: poll briefly.
            let list = format!("ls -A {}", notify::OUTBOX);
            let mut empty = false;
            for _ in 0..50 {
                let out = crate::test_support::runtime_cmd().args(["exec", &name, "sh", "-c", &list]).output().unwrap();
                if out.status.success() && out.stdout.is_empty() {
                    empty = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(empty, "outbox drained");
            Ok(())
        })
    }

    /// Docker-gated end to end: `xdg-open <url>` (the `/usr/local/bin` link,
    /// found on the container's own `PATH`) and `$BROWSER <url>` (the exec
    /// env's) in a container reach the production sink's opener; a path is
    /// refused in the container with `xdg-open`'s exit 3.
    #[test_utils::docker_test(helper)]
    fn container_opens_reach_the_opener_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-open-test-{stamp}");
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            fn apply(n: Notification) -> Result<Option<(String, crate::inbox::Shown)>, String> {
                panic!("an open reached the Inbox: {:?}", n.message)
            }
            let (tx, opened) = mpsc::channel();
            let tx = Mutex::new(tx);
            let hooks = SinkHooks {
                on_shown: None,
                opener: Arc::new(move |instance: &str, _: &str, url: &str| {
                    let _ = tx.lock().unwrap().send((instance.to_string(), url.to_string()));
                }),
                on_open_done: None,
                log: |line| panic!("open refused or dropped: {line}"),
            };
            let to = Services::new("open-test".into(), "open-test-id".into(), sink(apply, hooks), no_control());
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, Some(to)).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            let exec = |env: &[&str], script: &str| {
                let mut cmd = crate::test_support::runtime_cmd();
                cmd.arg("exec");
                for e in env {
                    cmd.args(["-e", e]);
                }
                cmd.args([name.as_str(), "sh", "-c", script]).output().unwrap()
            };

            let link = exec(&[], "readlink /usr/local/bin/xdg-open");
            assert_eq!(String::from_utf8_lossy(&link.stdout).trim(), BIN, "linked where nothing was");
            assert!(exec(&[], "xdg-open https://example.test/a").status.success());
            assert_eq!(
                opened.recv_timeout(Duration::from_secs(15)).expect("xdg-open delivered"),
                ("open-test".to_string(), "https://example.test/a".to_string())
            );
            let browser = format!("BROWSER={}", crate::devsbd::OPENER);
            assert!(exec(&[&browser], "$BROWSER 'https://example.test/b?x=1'").status.success());
            assert_eq!(
                opened.recv_timeout(Duration::from_secs(15)).expect("$BROWSER delivered"),
                ("open-test".to_string(), "https://example.test/b?x=1".to_string())
            );
            let file = exec(&[], "xdg-open report.pdf");
            assert_eq!(file.status.code(), Some(3));
            assert_eq!(
                String::from_utf8_lossy(&file.stderr).trim(),
                "devsbd: opening files isn't supported; open URLs only"
            );
            assert_eq!(exec(&[], "xdg-open").status.code(), Some(1), "usage");
            assert!(opened.recv_timeout(Duration::from_millis(300)).is_err(), "nothing else opened");
            Ok(())
        })
    }

    /// Docker-gated: an image's own `/usr/local/bin/xdg-open` (a file, or a
    /// link even when it dangles) survives the install; the opener's names in
    /// `/run/devsandbox/bin` are linked all the same.
    #[test_utils::docker_test(helper)]
    fn install_keeps_an_images_own_xdg_open_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let names = [format!("devsandbox-own-open-test-{stamp}"), format!("devsandbox-own-open-link-test-{stamp}")];
        let cleanup = || {
            for name in &names {
                let _ = crate::test_support::runtime_cmd().args(["rm", "-f", name]).output();
            }
        };
        crate::test_support::with_cleanup(cleanup, || {
            let sh = |name: &str, script: &str| {
                let out = crate::test_support::runtime_cmd().args(["exec", name, "sh", "-c", script]).output().unwrap();
                assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            let mine = "printf '#!/bin/sh\\necho mine\\n' > /usr/local/bin/xdg-open && chmod 755 /usr/local/bin/xdg-open";
            let dangling = "ln -s /opt/nowhere/xdg-open /usr/local/bin/xdg-open";
            for (name, setup) in names.iter().zip([mine, dangling]) {
                assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", name, "alpine:3.20", "sleep", "300"])));
                sh(name, setup);
                install(name, None).unwrap();
                let opener = format!("readlink {}", crate::devsbd::OPENER);
                assert_eq!(sh(name, &opener), "devsbd", "the BIN_DIR names are linked");
            }
            assert_eq!(sh(&names[0], "xdg-open https://example.test"), "mine", "the image's file kept");
            assert_eq!(sh(&names[1], "readlink /usr/local/bin/xdg-open"), "/opt/nowhere/xdg-open", "the image's link kept");
            Ok(())
        })
    }

    /// Docker-gated end to end: with no host attached, a second `devsbd thread
    /// put` for the same key replaces the queued first one, so a dispatcher
    /// re-asserting its threads for hours doesn't deliver a backlog when a
    /// dashboard finally opens. A keyed `notify` queued alongside is a log
    /// entry and survives.
    #[test_utils::docker_test(helper)]
    fn coalesces_queued_thread_puts_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-thread-test-{stamp}");
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);

            // No bridge yet, so nothing can drain: both puts sit in the outbox.
            let put = |title: &str| {
                let json = format!(r#"{{"key":"pr-1","title":"{title}","state":"active"}}"#);
                ok(crate::test_support::runtime_cmd().args(["exec", &name, BIN, "thread", "put", "--json", &json]))
            };
            assert!(put("first"));
            assert!(put("second"));
            assert!(ok(crate::test_support::runtime_cmd().args(["exec", &name, BIN, "notify", "--key", "pr-1", "hello"])));
            let count = format!("ls -A {} | wc -l", notify::OUTBOX);
            let out = crate::test_support::runtime_cmd().args(["exec", &name, "sh", "-c", &count]).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "2", "the first put was replaced");

            let (to, rx) = services("thread-test", no_control());
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, Some(to)).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");

            let first = rx.recv_timeout(Duration::from_secs(15)).expect("put delivered");
            match first.message {
                notify::Message::ThreadPut { key, body, .. } => {
                    assert_eq!(key, "pr-1");
                    assert!(body.contains("second"), "the newest put won: {body}");
                }
                other => panic!("expected a thread put, got {other:?}"),
            }
            let second = rx.recv_timeout(Duration::from_secs(15)).expect("notify delivered");
            assert!(matches!(second.message, notify::Message::Notify(_)), "{:?}", second.message);
            assert!(rx.recv_timeout(Duration::from_secs(2)).is_err(), "exactly one put arrived");
            Ok(())
        })
    }

    /// Docker-gated: `devsbd thread send|withdraw` queue with no host, coalesce
    /// per `(thread, id)` in place (cross-id order kept), and arrive after the
    /// put they need; a bad id is a usage error that never queues; `devsbd
    /// features` advertises the verbs.
    #[test_utils::docker_test(helper)]
    fn coalesces_queued_thread_sends_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-send-test-{stamp}");
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let devsbd = |args: &[&str]| crate::test_support::runtime_cmd().args(["exec", &name, BIN]).args(args).output().unwrap();

            let features = devsbd(&["features"]);
            assert!(String::from_utf8_lossy(&features.stdout).lines().any(|l| l == "thread-send"), "{features:?}");

            let put = r#"{"key":"pr-1","title":"PR 1","state":"active"}"#;
            assert!(devsbd(&["thread", "put", "--json", put]).status.success());
            let send = |id: &str, text: &str| {
                let json = format!(r#"{{"thread":"pr-1","id":"{id}","blocks":[{{"type":"markdown","text":"{text}"}}]}}"#);
                devsbd(&["thread", "send", "--json", &json])
            };
            assert!(send("a", "a1").status.success());
            assert!(send("b", "b1").status.success());
            assert!(send("a", "a2").status.success());
            assert!(devsbd(&["thread", "withdraw", "pr-1", "b"]).status.success());
            let bad = send("Bad", "x");
            assert_eq!(bad.status.code(), Some(2), "{bad:?}");
            assert!(String::from_utf8_lossy(&bad.stderr).contains("bad id `Bad`"), "{bad:?}");
            let count = format!("ls -A {} | wc -l", notify::OUTBOX);
            let out = crate::test_support::runtime_cmd().args(["exec", &name, "sh", "-c", &count]).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "3", "put, send a, withdraw b");

            let (to, rx) = services("send-test", no_control());
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, Some(to)).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            let mut got = Vec::new();
            for _ in 0..3 {
                got.push(rx.recv_timeout(Duration::from_secs(15)).expect("record delivered").message);
            }
            assert!(matches!(&got[0], notify::Message::ThreadPut { key, .. } if key == "pr-1"), "{got:?}");
            assert!(
                matches!(&got[1], notify::Message::ThreadSend { key, id, body, .. } if key == "pr-1" && id == "a" && body.contains("a2")),
                "a's newest body, ahead of b: {got:?}"
            );
            assert!(matches!(&got[2], notify::Message::ThreadWithdraw { id, .. } if id == "b"), "{got:?}");
            assert!(rx.recv_timeout(Duration::from_secs(2)).is_err(), "nothing else arrived");
            Ok(())
        })
    }

    /// Docker-gated end to end: `devsbd ensure` in a container reaches a
    /// sink-bearing bridge's control handler (a stub, so no real state is
    /// touched) and prints its answer; with no bridge it exits 75.
    #[test_utils::docker_test(helper)]
    fn serves_container_control_requests_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-control-test-{stamp}");
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let ensure = || crate::test_support::runtime_cmd().args(["exec", &name, BIN, "ensure", "web", "--key", "pr-1"]).output().unwrap();

            // Daemon up, no bridge: fail fast with the no-host code.
            let out = ensure();
            assert_eq!(out.status.code(), Some(control::EXIT_NO_HOST), "{out:?}");
            assert!(String::from_utf8_lossy(&out.stderr).contains("no host connected"), "{out:?}");

            let (tx, calls) = mpsc::channel();
            let tx = Mutex::new(tx);
            let control: ControlHandler = Arc::new(move |key: &str, req: &Request| {
                let _ = tx.lock().unwrap().send((key.to_string(), req.clone()));
                Response::new(Status::Ok, "web-pr-1")
            });
            let (to, _notes) = services("control-test", control);
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, Some(to)).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            // The host's `Caps` lands just after the handshake: retry briefly.
            let mut out = ensure();
            for _ in 0..50 {
                if out.status.code() != Some(control::EXIT_NO_HOST) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
                out = ensure();
            }
            assert_eq!(out.status.code(), Some(0), "{out:?}");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "web-pr-1\n");
            let (key, req) = calls.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(key, "control-test");
            assert_eq!((req.op, req.sandbox.as_deref(), req.key.as_deref()), (control::Op::Ensure, Some("web"), Some("pr-1")));

            // A usage error never reaches the host.
            let out = crate::test_support::runtime_cmd().args(["exec", &name, BIN, "ensure", "web"]).output().unwrap();
            assert_eq!(out.status.code(), Some(control::EXIT_USAGE), "{out:?}");
            assert!(calls.recv_timeout(Duration::from_millis(200)).is_err());

            // Bridge gone: back to 75 once the daemon notices.
            drop(bridge);
            let mut code = None;
            for _ in 0..50 {
                code = ensure().status.code();
                if code == Some(control::EXIT_NO_HOST) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert_eq!(code, Some(control::EXIT_NO_HOST), "after the bridge ended");
            Ok(())
        })
    }

    /// An `events-follow` stream goes to the follow handler (never the control
    /// handler), which writes the held-open response itself.
    #[test]
    fn follow_stream_runs_the_follow_handler() {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let (mut to, _rx) = services("web-1", no_control());
        to.follow = Arc::new(|key: &str, req: &Request, out: &mut dyn Write| {
            let _ = write!(out, "status ok\nbody \n{key} {}\n", req.key.as_deref().unwrap_or("-"));
        });
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || d.serve(d_r, |_, _| None));
        std::thread::spawn(move || {
            let none: Option<fn() -> Option<PathBuf>> = None;
            host.serve(h_r, move |_, ch| open_stream(ch, none.as_ref(), None, Some(&to)))
        });
        let reply = send_on(&daemon, 1, proto::channel::CONTROL, b"op events-follow\nkey pr-1\n");
        assert_eq!(reply, b"status ok\nbody \nweb-1 pr-1\n");
    }

    /// The user replies `text` on `owner`'s thread `pr-1` in the store at
    /// `path`, putting the thread first: an event enqueued from the host.
    fn host_reply(path: &Path, owner: &str, text: &str) {
        use crate::inbox::{store, Compose, State as TState, ThreadPut};
        store::update_at(path, |i| {
            let find = |i: &crate::inbox::Inbox| i.threads.iter().find(|t| t.owner == owner).map(|t| t.id);
            if find(i).is_none() {
                let put = ThreadPut {
                    key: "pr-1".into(),
                    title: "PR 1".into(),
                    state: TState::NeedsYou,
                    compose: Some(Compose::default()),
                    ..ThreadPut::default()
                };
                i.put(owner, owner, 1, put);
            }
            let id = find(i).unwrap();
            i.apply(&crate::inbox::Op::Reply { thread: id, text: text.into() }, 2, "tui");
        })
        .unwrap();
    }

    /// A `devsbd events --follow` running in container `name`: its stdout
    /// lines arrive on the receiver; the child is returned for its exit code.
    fn follower(name: &str) -> (std::process::Child, mpsc::Receiver<String>) {
        use std::io::BufRead;
        let mut child = crate::test_support::runtime_cmd()
            .args(["exec", name, BIN, "events", "--follow"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        (child, rx)
    }

    /// The next line on `rx` that isn't a ping (`None` on timeout).
    fn next_event(rx: &mpsc::Receiver<String>) -> Option<String> {
        loop {
            let line = rx.recv_timeout(Duration::from_secs(15)).ok()?;
            if line != control::FOLLOW_PING_LINE {
                return Some(line);
            }
        }
    }

    /// `child`'s exit code, waiting up to `secs`.
    fn exit_within(child: &mut std::process::Child, secs: u64) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        None
    }

    /// Docker-gated end to end: `devsbd events --follow` in a container gets
    /// the pending event, then one enqueued on the host as it lands, and
    /// pings; a second follower replaces the first (`replaced`, exit 0); the
    /// bridge ending, or the daemon dying, makes the follower exit 75.
    #[test_utils::docker_test(helper)]
    fn follows_events_with_docker() -> Result<(), &'static str> {
        use crate::commands::dispatch::{follow_events, Followers};
        const OWNER: &str = "follow-owner";
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-follow-test-{stamp}");
        let dir = std::env::temp_dir().join(format!("devsandbox-follow-docker-{stamp}"));
        let path = dir.join("inbox.json");
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
            let _ = std::fs::remove_dir_all(&dir);
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            host_reply(&path, OWNER, "before");

            let followers = Arc::new(Followers::new());
            let bridge_for = || {
                let control: ControlHandler = Arc::new(|_: &str, _: &Request| Response::new(Status::Ok, "[]"));
                let (mut to, _notes) = services("follow-test", control);
                let (path, followers) = (path.clone(), Arc::clone(&followers));
                to.follow = Arc::new(move |_: &str, req: &Request, out: &mut dyn Write| {
                    let ping = Duration::from_secs(1);
                    if let Err(resp) = follow_events(&path, OWNER, req.key.as_deref(), out, &followers, ping) {
                        let _ = out.write_all(control::encode_response(&resp).as_bytes());
                    }
                });
                let none: Option<fn() -> Option<PathBuf>> = None;
                let bridge = spawn_with(&name, hash(arch).unwrap(), none, Some(to)).unwrap();
                assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
                // The host's `Caps` lands just after the handshake: wait for a
                // routable control op before following.
                let ls = || crate::test_support::runtime_cmd().args(["exec", &name, BIN, "ls"]).output().unwrap().status.code();
                let mut code = ls();
                for _ in 0..50 {
                    if code == Some(0) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    code = ls();
                }
                assert_eq!(code, Some(0), "control routable");
                bridge
            };
            let text = |line: &str| -> String {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                v["text"].as_str().unwrap_or_default().to_string()
            };

            let bridge = bridge_for();
            let (mut first, first_rx) = follower(&name);
            assert_eq!(next_event(&first_rx).map(|l| text(&l)).as_deref(), Some("before"), "pending first");
            host_reply(&path, OWNER, "after");
            assert_eq!(next_event(&first_rx).map(|l| text(&l)).as_deref(), Some("after"), "enqueued on the host");
            let ping = first_rx.recv_timeout(Duration::from_secs(5)).expect("a ping");
            assert_eq!(ping, control::FOLLOW_PING_LINE);

            let (mut second, second_rx) = follower(&name);
            assert_eq!(next_event(&first_rx).as_deref(), Some(control::FOLLOW_REPLACED_LINE));
            assert_eq!(exit_within(&mut first, 10), Some(0), "replaced exits 0");
            // Nothing acked: the new stream gets both again.
            assert_eq!(next_event(&second_rx).map(|l| text(&l)).as_deref(), Some("before"));
            assert_eq!(next_event(&second_rx).map(|l| text(&l)).as_deref(), Some("after"));

            drop(bridge);
            assert_eq!(exit_within(&mut second, 20), Some(control::EXIT_NO_HOST), "bridge gone");

            let _bridge = bridge_for();
            let (mut third, third_rx) = follower(&name);
            assert_eq!(next_event(&third_rx).map(|l| text(&l)).as_deref(), Some("before"));
            let kill = "kill $(cut -d' ' -f1 /run/devsandbox/devsbd.pid)";
            assert!(ok(crate::test_support::runtime_cmd().args(["exec", &name, "sh", "-c", kill])));
            assert_eq!(exit_within(&mut third, 20), Some(control::EXIT_NO_HOST), "daemon killed");
            Ok(())
        })
    }

    /// A [`Broker`] answering each connection's first request with a 200
    /// naming its caller and header, then closing.
    struct HttpBroker;

    impl Broker for HttpBroker {
        fn serve(&self, caller: &Caller, header: proto::ProxyHeader, mut stream: UnixStream) {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                head.push(byte[0]);
            }
            let body = format!("{} {:?}\n", caller.instance, header);
            let answer = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = stream.write_all(answer.as_bytes());
        }
    }

    /// Docker-gated end to end: a broker file written into a container with
    /// `sync_broker`, read by a daemon started after it: its phantom file is
    /// written as its owner, a file that isn't a phantom is left alone, and
    /// an HTTP request to the loopback listener inside reaches the host's
    /// broker with this bridge's instance and the listener's header.
    #[test_utils::docker_test(helper)]
    fn brokers_a_loopback_listener_and_writes_phantoms_with_docker() -> Result<(), &'static str> {
        use super::super::brokerfile::{BrokerSpec, Listener, Mode, PhantomFile};
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-broker-test-{stamp}");
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        let sh = |script: &str| {
            let out = crate::test_support::runtime_cmd().args(["exec", &name, "sh", "-c", script]).output().unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            assert!(sh("adduser -D -u 1234 u && printf real > /home/u/.taken && chown u /home/u/.taken").0);
            let arch = install(&name, None).unwrap();
            let file = |path: &str, phantom: &str| PhantomFile {
                owner: "u".into(),
                path: path.into(),
                phantom: phantom.into(),
                content: format!("token={phantom}\n"),
            };
            let spec = BrokerSpec {
                listeners: vec![Listener { port: 41001, mode: Mode::BaseUrl { upstream: "sentry.io".into() } }],
                proxy_listeners: Vec::new(),
                files: vec![file("/home/u/.config/sentry/rc", "p1"), file("/home/u/.taken", "p2")],
                proxy_env: Vec::new(),
                credentials: Vec::new(),
            };
            super::super::sync_broker(&name, Some(&spec)).unwrap();
            start_daemon(&name);

            let broker: Arc<dyn Broker> = Arc::new(HttpBroker);
            let (to, _notes) = services("broker-test", no_control());
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, Some(Services { broker: Some(broker), ..to })).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");

            // The daemon starts detached: retry until its listener is up.
            let mut got = (false, String::new());
            for _ in 0..50 {
                got = sh("wget -qO- http://127.0.0.1:41001/api/0/");
                if got.0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert_eq!(got.1, "broker-test BaseUrl { upstream: \"sentry.io\", listener: 41001 }\n");

            assert_eq!(sh("stat -c '%U %a' /home/u/.config/sentry/rc && cat /home/u/.config/sentry/rc").1, "u 600\ntoken=p1\n");
            assert_eq!(sh("stat -c '%U' /home/u/.config").1, "u\n", "parent dirs made as the owner");
            assert_eq!(sh("cat /home/u/.taken").1, "real", "a file that isn't a phantom is left alone");
            assert_eq!(sh("stat -c '%a' /run/devsandbox/broker").1, "644\n");
            Ok(())
        })
    }

    /// Docker-gated: a `Connect` over a host `Bridge` reaches a server bound to
    /// **127.0.0.1 inside the container** (only the daemon shares its network
    /// namespace) and round-trips data; a `Connect` to a closed port yields an
    /// `on_close` reason mentioning "refused". No ssh-agent needed — the bridge
    /// is agent-less, exercising the forwarding path on its own.
    #[test_utils::docker_test(helper)]
    fn forwards_a_connect_to_a_loopback_server_with_docker() -> Result<(), &'static str> {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;

        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-forward-test-{stamp}");
        // busybox nc echoes stdin back to the client (-e cat) on loopback only,
        // so the port is reachable only from inside the netns — where the daemon
        // dials. `-lk` keeps it serving across connects.
        let serve = "busybox nc -lk -s 127.0.0.1 -p 8080 -e cat";
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sh", "-c", serve])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, None).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            assert_eq!(
                bridge.peer_caps(),
                Some(caps::TCP_FORWARD | caps::PORT_INFO | caps::RPC),
                "daemon advertises TCP_FORWARD, PORT_INFO and RPC"
            );

            // Round-trip through the loopback echo server. Retry the connect a
            // few times: `nc -lk` may not have bound its listener the instant
            // the container starts.
            let mut round_tripped = false;
            for _ in 0..30 {
                let (mut ours, theirs) = UnixStream::pair().unwrap();
                ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let (tx, closed) = mpsc::channel();
                let Ok(_id) = bridge.connect(theirs, "127.0.0.1", 8080, move |r| {
                    let _ = tx.send(r);
                }) else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                ours.write_all(b"ping\n").unwrap();
                let mut buf = [0u8; 5];
                match ours.read_exact(&mut buf) {
                    Ok(()) if &buf == b"ping\n" => {
                        round_tripped = true;
                        break;
                    }
                    _ => {
                        // Server not up yet: this stream closed, try again.
                        let _ = closed.recv_timeout(Duration::from_secs(1));
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
            assert!(round_tripped, "loopback echo did not round-trip through the forward");

            // The daemon names the listener (no lsof in alpine); a closed port
            // has none.
            let procs = bridge.listening(8080, Duration::from_secs(2)).unwrap();
            assert!(!procs.is_empty() && procs.iter().all(|(pid, comm)| *pid > 0 && !comm.is_empty()), "{procs:?}");
            assert_eq!(bridge.listening(9, Duration::from_secs(2)).unwrap(), []);

            // A Connect to a closed port comes back refused.
            let (_ours, theirs) = UnixStream::pair().unwrap();
            let (tx, closed) = mpsc::channel();
            bridge.connect(theirs, "127.0.0.1", 9, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
            let reason = closed.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(reason.contains("refused"), "closed port reason: {reason:?}");
            Ok(())
        })
    }

    /// Docker-gated: a managed bridge publishes its link on the board once
    /// the handshake is done, an `RPC` call (`rpc.methods`) round-trips over
    /// it to the real daemon, and the link goes when the bridge does.
    #[test_utils::docker_test(helper)]
    fn a_managed_bridge_publishes_its_link_with_docker() -> Result<(), &'static str> {
        use super::super::rpc::{self, Call, Record};

        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-rpc-link-test-{stamp}");
        let cleanup = || {
            let _ = crate::test_support::runtime_cmd().args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(crate::test_support::runtime_cmd().args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let board = BridgeBoard::default();
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_observed(&name, hash(arch).unwrap(), none, None, None, Some(board.spawning(&name))).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            let link = board.link(&name).expect("published by the handshake");
            assert_ne!(link.caps & caps::RPC, 0);

            let (tx, closed) = mpsc::channel();
            let (_, mut conn) = link
                .open_rpc(move |r| {
                    let _ = tx.send(r);
                })
                .unwrap();
            let Conn::Unix(s) = &conn else { unreachable!() };
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            rpc::write_record(&mut conn, &Record::Call(Call { method: "rpc.methods".into(), ..Call::default() })).unwrap();
            conn.shutdown(std::net::Shutdown::Write).unwrap();
            let Ok(Some(Record::Result(o))) = rpc::read_record(&mut conn) else { panic!("no result") };
            assert!(o.strs("methods").unwrap().contains(&"rpc.methods@1".to_string()), "{o:?}");
            assert_eq!(rpc::read_record(&mut conn).unwrap(), None);
            assert_eq!(closed.recv_timeout(Duration::from_secs(10)).unwrap(), "", "clean both-Eof end");

            drop(bridge);
            let deadline = Instant::now() + Duration::from_secs(10);
            while board.link(&name).is_some() {
                assert!(Instant::now() < deadline, "the link outlived its bridge");
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        })
    }
}
