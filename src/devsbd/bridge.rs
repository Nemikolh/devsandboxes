//! Host half of the ssh-agent relay (docs/sandbox-helper.md): runs
//! `exec -i <c> devsbd bridge` and serves the streams it carries, connecting
//! each `Open` to the host agent. Owners: CLI `exec` (for the command's
//! lifetime) and the TUI (one per running instance while open). TUI bridges
//! also take the container's `devsbd notify` records and serve its
//! `devsbd ensure|ls|stop|rm` control requests (docs/automations.md).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::mux::{self, Conn, Mux};
use super::control::{self, Request, Response, Status};
use super::proto::caps;
use super::{notify, proto, relay_mode, BIN};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// A running bridge; dropping it kills the `exec` (the in-container bridge
/// then sees stdin EOF and exits).
pub struct Bridge {
    // Shared so the keepalive/handshake-timeout threads can kill the child too;
    // `Drop` and those threads race on `kill()`, which is idempotent.
    child: Arc<Mutex<Child>>,
    done: Arc<AtomicBool>,
    // Set by the handshake thread when the failure is a protocol version
    // mismatch — either detected host-side (`proto::version_mismatch`) or by the
    // bridge exiting `proto::MISMATCH_EXIT` (daemon mismatch). A mismatched
    // bridge is not retried while its container stays running: only a restart,
    // which rewrites the helper, can fix it.
    mismatch: Arc<AtomicBool>,
    handshake: mpsc::Receiver<Result<(), String>>,
    // Published by the handshake thread once `Hello` succeeds: the live mux (to
    // open forward streams on) and the daemon's advertised caps. Absent while
    // the handshake is pending or after it failed.
    forward: Arc<OnceLock<Forward>>,
    // The instance's container, for the "outdated helper" message.
    container: String,
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
    #[allow(dead_code)] // host forwarder engine (docs/port-forwarding.md, step 5)
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
    #[allow(dead_code)] // host forwarder engine (docs/port-forwarding.md, step 5)
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
                format!("helper in {} is outdated (no port forwarding): restart the instance", self.container),
            ));
        }
        forward.mux.connect(conn, host, port, on_close)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The host agent socket, read per stream so agent rotation needs no restart.
/// `$SSH_AUTH_SOCK` set and its path present on the host; the existence check
/// also gates whether a bridge is worth spawning (`has_host_agent`).
fn host_agent() -> Option<PathBuf> {
    let sock = PathBuf::from(std::env::var_os("SSH_AUTH_SOCK")?);
    std::fs::metadata(&sock).is_ok().then_some(sock)
}

/// Whether the host has a usable ssh-agent right now. Gates bridge spawning
/// (`exec_status`, `Bridges::reconcile`) and `SSH_AUTH_SOCK` injection: with no
/// agent a bridge and the env var only cost an extra `exec` that can't help.
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

/// What a long-lived (TUI) bridge serves beyond the agent: notify into its
/// sink, control through its handler, both tagged with the instance it serves.
#[derive(Clone)]
struct Services {
    instance: String,
    instance_id: String,
    sink: Sink,
    control: ControlHandler,
    // Live notify/control handler threads of this bridge (see `HandlerSlot`).
    handlers: Arc<AtomicUsize>,
}

impl Services {
    fn new(instance: String, instance_id: String, sink: Sink, control: ControlHandler) -> Services {
        Services { instance, instance_id, sink, control, handlers: Arc::default() }
    }
}

/// Notify/control handler threads one bridge may run at once. The container
/// controls its end of the mux, and a peer `Close` frees the mux entry while
/// our handler keeps going, so `mux::MAX_STREAMS` alone doesn't bound these.
const MAX_HANDLERS: usize = 8;

/// One of a bridge's [`MAX_HANDLERS`] slots, held by a handler thread for its
/// whole life and released on drop.
struct HandlerSlot(Arc<AtomicUsize>);

impl HandlerSlot {
    /// A slot on `count`, or `None` when all are taken.
    fn acquire(count: &Arc<AtomicUsize>) -> Option<HandlerSlot> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < MAX_HANDLERS).then_some(n + 1))
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
/// with an agent socket, `NOTIFY` and `CONTROL` only with a sink (the TUI's
/// bridges) — so short-lived sink-less bridges (`exec`, lifecycle, forwards)
/// never take notify streams, which would drop the records with them, nor
/// control requests, which must outlive a one-shot command. Every such bridge
/// advertises `CONTROL`, dispatcher or not: `dispatch::handle` re-checks the
/// config on each request and answers `Denied` for the rest.
fn own_caps(has_agent: bool, has_sink: bool) -> u32 {
    let agent = if has_agent { caps::SSH_AGENT } else { 0 };
    let sink = if has_sink { caps::NOTIFY | caps::CONTROL } else { 0 };
    agent | sink
}

/// A control stream whose request doesn't end (no `Eof`) within this is
/// dropped. The daemon sends the whole request at once.
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(30);

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
    if !writes_state(req.op) || !dispatch::declares_dispatcher(key) {
        return dispatch::handle(key, req);
    }
    let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dispatch::handle(key, req)
}

/// Whether `op` runs a `devsandbox` subprocess that writes `state.toml`
/// ([`dispatch_control`] serializes those). Run ops, `run-rm`/`run-prune`
/// included, only exec in a child.
fn writes_state(op: crate::devsbd::control::Op) -> bool {
    use crate::devsbd::control::Op;
    matches!(op, Op::Ensure | Op::Stop | Op::Rm)
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
/// streams connect to the host agent (when this bridge has a provider), notify
/// and control streams go to a handler thread (when it has `services` and a
/// free [`HandlerSlot`]). Must return at once: it runs on the mux's `serve`
/// thread.
fn open_stream(
    channel: u8,
    agent: Option<&impl Fn() -> Option<PathBuf>>,
    services: Option<&Services>,
) -> Option<Conn> {
    let handler: fn(UnixStream, &Services) = match channel {
        proto::channel::SSH_AGENT => return UnixStream::connect(agent?()?).ok().map(Into::into),
        proto::channel::NOTIFY => handle_notify,
        proto::channel::CONTROL => handle_control,
        _ => return None,
    };
    let to = services?.clone();
    let slot = HandlerSlot::acquire(&to.handlers)?;
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
/// disconnected). Silent: the TUI owns the terminal.
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
            Ok(req) => (to.control)(&to.instance, &req),
            Err(e) => Response::new(Status::Usage, format!("bad request: {e}")),
        }
    };
    let _ = conn.write_all(control::encode_response(&resp).as_bytes());
}

/// One notify stream: read the record to EOF (the daemon's `Eof` half-closes
/// our side), decode, hand it to the sink, and only then reply `ok`. Applying
/// before acking is what makes delivery durable: a host that dies between the
/// two (or a store that won't take the record) leaves the record in the
/// container's outbox, and the daemon resends it. Oversized or timed-out →
/// close with no reply; undecodable → `NOTIFY_REPLY_BAD`. Silent throughout:
/// the TUI owns the terminal.
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

/// Start a bridge for `info` without waiting for its handshake (see
/// `outcome`). Fully quiet (stderr captured), since the TUI owns the
/// screen; `None` only when the `exec` can't even be spawned.
pub fn spawn(info: &Instance) -> Option<Bridge> {
    let hash = info.devsbd_arch.and_then(super::hash).unwrap_or_default();
    spawn_with(&info.container, hash, Some(host_agent), None)
}

/// A `Bridges` bridge for instance `key`: the host agent only when
/// `with_agent`, notify + control only when `sink` is set.
fn spawn_managed(key: &str, info: &Instance, with_agent: bool, sink: Option<&Sink>) -> Option<Bridge> {
    let hash = info.devsbd_arch.and_then(super::hash).unwrap_or_default();
    let notify = sink.map(|s| {
        Services::new(
            key.to_string(),
            info.instance_id.clone(),
            Arc::clone(s),
            Arc::new(dispatch_control),
        )
    });
    if with_agent {
        spawn_with(&info.container, hash, Some(host_agent), notify)
    } else {
        let none: Option<fn() -> Option<PathBuf>> = None;
        spawn_with(&info.container, hash, none, notify)
    }
}

/// Start a bridge for `container` with helper `hash`, optionally serving the
/// host ssh-agent. `with_agent` = true wires the real `host_agent` provider (a
/// forward bridge serves ssh-agent when the host has one, advertising
/// `SSH_AGENT` honestly); false makes an agent-less bridge that the daemon
/// won't route agent clients to. The forwarder (docs/port-forwarding.md, step
/// 5) uses this so `spawn_with`'s generic agent type stays private.
pub fn spawn_for(container: &str, hash: &str, with_agent: bool) -> Option<Bridge> {
    if with_agent {
        spawn_with(container, hash, Some(host_agent), None)
    } else {
        let none: Option<fn() -> Option<PathBuf>> = None;
        spawn_with(container, hash, none, None)
    }
}

/// Start a bridge. `agent` is the host ssh-agent socket provider, or `None` for
/// an agent-less bridge (a `devsandbox port` forward): it advertises no
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
    let mut child = Command::new(backend().bin())
        .args(["exec", "-i", "-u", "root", container, BIN, "bridge"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let child = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));
    let mismatch = Arc::new(AtomicBool::new(false));
    let (tx, handshake) = mpsc::channel();
    let hash = hash.to_string();
    let container = container.to_string();
    let finished = Arc::clone(&done);
    let hs_mismatch = Arc::clone(&mismatch);
    let forward = Arc::new(OnceLock::new());
    let hs_forward = Arc::clone(&forward);
    // The handshake thread moves `container` (for `mismatch_message`); keep a
    // copy for the `Bridge`'s own `connect` error message.
    let bridge_container = container.clone();
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
                let _ = child.lock().unwrap().kill();
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
            // phrase it with the right direction and container. Otherwise the
            // bridge died before `Hello` (no daemon, no binary) and its stderr
            // says why — but when the *daemon* is the mismatch, the bridge
            // already printed its own direction-aware line, so prefer stderr.
            if let Some(vm) = proto::version_mismatch(&e) {
                hs_mismatch.store(true, Ordering::Relaxed);
                return mismatch_message(&container, vm);
            }
            let mut msg = String::new();
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                let _ = std::io::Read::read_to_string(&mut stderr, &mut msg);
                // The bridge exits `MISMATCH_EXIT` when *its* handshake with the
                // daemon failed on a version mismatch (it printed the line we
                // just read). Classify that from the exit code, not the text, so
                // reconcile can stop retrying. `wait()` after EOF is prompt.
                if let Ok(status) = hs_child.lock().unwrap().wait() {
                    if status.code() == Some(proto::MISMATCH_EXIT) {
                        hs_mismatch.store(true, Ordering::Relaxed);
                    }
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
        if !ok {
            let _ = tx.send(result);
            finished.store(true, Ordering::Relaxed);
            return;
        }
        {
            let mux = Mux::new(stdin);
            // Tell the daemon our caps right after the handshake, on stream 0:
            // `SSH_AGENT` only when this bridge has an agent provider and it
            // yields a socket now; `NOTIFY`/`CONTROL` only with a sink (`own_caps`). An
            // agent-less bridge (a `devsandbox port` forward) advertises no
            // `SSH_AGENT`, so the daemon won't route agent clients to it. Best
            // effort — a send failure just means the connection died.
            let has_agent = agent.as_ref().and_then(|a| a()).is_some();
            let _ = mux.send(&proto::Frame::Caps(own_caps(has_agent, notify.is_some())));
            // Publish the mux + daemon caps *before* reporting the handshake
            // result, so a caller that sees `outcome() == Ok` can rely on
            // `peer_caps()` / `connect()` being ready (no publish race).
            let _ = hs_forward.set(Forward { mux: Arc::clone(&mux), peer_caps: daemon_caps });
            let _ = tx.send(result);
            // Keepalive: a wedged daemon (stopped reading) is caught by inbound
            // silence; `on_dead` kills the child, ending the `serve` read below
            // so `done` flips and `Bridges::reconcile` retries.
            let dead_child = Arc::clone(&hs_child);
            mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
                let _ = dead_child.lock().unwrap().kill();
            });
            mux.serve(stdout, move |_, channel| open_stream(channel, agent.as_ref(), notify.as_ref()));
        }
        finished.store(true, Ordering::Relaxed);
    });
    Some(Bridge { child, done, mismatch, handshake, forward, container: bridge_container })
}

/// How long the host waits for the daemon handshake before killing the child
/// and reporting a timeout, so a container that accepts the `exec` but never
/// completes `Hello` (wedged daemon, stuck runtime) can't block a bridge owner
/// forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A host-side, direction-aware message for a helper/host protocol mismatch.
/// Peer (the helper) older → tell the user to restart the instance so the
/// binary is rewritten; peer newer → this `devsandbox` is the stale side.
/// The instance key isn't on hand here (only the container), so the fix hint
/// stays generic ("restart the instance").
fn mismatch_message(container: &str, vm: &proto::VersionMismatch) -> String {
    if vm.peer < vm.ours {
        format!(
            "helper in {container} is outdated (protocol {}, need {}): restart the instance",
            vm.peer, vm.ours
        )
    } else {
        format!(
            "this devsandbox is older than the helper in {container} (protocol {}, need {})",
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
/// mismatch (a restart, which drops the entry, retries at once).
fn retry_decision(entry: Option<(bool, bool, Duration)>) -> Retry {
    match entry {
        None => Retry::Spawn,
        Some((true, mismatch, since)) => {
            let gap = if mismatch { MISMATCH_RETRY } else { RETRY };
            if since >= gap { Retry::Respawn } else { Retry::Keep }
        }
        Some((false, ..)) => Retry::Keep,
    }
}

/// Whether `reconcile` wants a bridge for one running instance, and if so
/// whether it serves the host agent: `None` = no bridge, `Some(with_agent)`.
/// The agent is relayed as ever (`relay_mode` + a host agent); a sink adds a
/// bridge to every helper-capable instance so its notify outbox drains.
fn wanted(helper: bool, relay: bool, host_agent: bool, has_sink: bool) -> Option<bool> {
    let with_agent = host_agent && relay;
    (with_agent || (has_sink && helper)).then_some(with_agent)
}

/// The TUI's set of bridges, reconciled against the running instances on each
/// snapshot. Owned by a worker thread (`spawn_worker`), never touched on the UI
/// thread.
#[derive(Default)]
pub struct Bridges {
    // container → (bridge, spawned at, spawned with the agent provider).
    live: HashMap<String, (Bridge, Instant, bool)>,
    // Set → bridges advertise `NOTIFY` and are kept even without a host agent.
    sink: Option<Sink>,
}

impl Bridges {
    /// Keep one bridge per running container in `running` whose instance
    /// wants one (`wanted`); drop the rest. Dead bridges are retried per
    /// `retry_decision`; a bridge whose agent wiring no longer matches (host
    /// agent came or went) is replaced at once. Every (re)spawn first
    /// `ensure_recorded`s the instance's helper, so a CLI upgrade reaches a
    /// running instance once a dashboard is open: otherwise only
    /// `run`/`start`/`port` rewrite it, and a running dispatcher never gets new
    /// verbs. Keyed on spawns, not `is_mismatch`: a stale helper with the same
    /// protocol `VERSION` still bridges fine, it just lacks verbs. Spawns are
    /// rare (first sight, or a dead bridge's retry gap), so this is one extra
    /// `exec` per spawn, not per snapshot.
    pub fn reconcile(&mut self, running: &[&str]) {
        self.live.retain(|c, _| running.contains(&c.as_str()));
        let host_agent = has_host_agent();
        // No host agent and no sink → nothing to relay; skip the per-instance `exec`.
        if !host_agent && self.sink.is_none() {
            self.live.clear();
            return;
        }
        let Ok(state) = State::load() else { return };
        let mut keep = Vec::new();
        for (key, info) in &state.instances {
            if !running.contains(&info.container.as_str()) {
                continue;
            }
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
            if retry_decision(entry) != Retry::Keep {
                self.live.remove(&info.container);
                let healed;
                let info = match helper.then(|| crate::devsbd::ensure_recorded(key, info, true)).flatten() {
                    Some(arch) if Some(arch) != info.devsbd_arch => {
                        healed = Instance { devsbd_arch: Some(arch), ..info.clone() };
                        &healed
                    }
                    // Current, or the install failed: spawn on the recorded
                    // helper as before, so a failing install waits out the
                    // bridge's retry gap instead of re-running every snapshot.
                    _ => info,
                };
                if let Some(b) = spawn_managed(key, info, with_agent, self.sink.as_ref()) {
                    self.live.insert(info.container.clone(), (b, Instant::now(), with_agent));
                }
            }
        }
        self.live.retain(|c, _| keep.contains(c));
    }

    /// Move a `Bridges` onto its own thread, fed running-container lists over an
    /// mpsc. Reconcile (which does `State::load`, `exec` spawns and a helper
    /// reinstall check before each one) never runs
    /// on the UI thread; the snapshot arm just `send`s the owned list. Dropping
    /// the returned [`BridgeWorker`] closes the channel and joins the thread,
    /// which drops every live `Bridge` (killing its `exec`) before returning.
    ///
    /// `sink` set → every helper-capable running instance gets a notify-serving
    /// bridge; each message is applied to the shared Inbox store first (that
    /// write is what the daemon's `ok` acknowledges, so a failed one is
    /// retried), then shown on the desktop (from the stream's handler thread)
    /// unless [`RateLimit`](super::desktop::RateLimit) holds it back, and
    /// finally the status line is sent on `sink` as a "the store changed" poke
    /// the TUI drains. A put that changed nothing produces neither, so a
    /// dispatcher re-asserting its threads is invisible.
    pub fn spawn_worker(sink: Option<mpsc::Sender<String>>) -> BridgeWorker {
        let sink = sink.map(|tx| -> Sink {
            // One limiter for every bridge, keyed by instance inside.
            let limit = Mutex::new(super::desktop::RateLimit::default());
            Arc::new(move |n: Notification| {
                let shown = apply_message(n)?;
                let Some((instance, shown)) = shown else { return Ok(()) };
                if let Some(popup) = shown.popup {
                    let allowed = limit
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .allow(&instance, popup.key.as_deref(), Instant::now());
                    if allowed {
                        super::desktop::notify_desktop(&instance, &popup);
                    }
                }
                let _ = tx.send(shown.line);
                Ok(())
            })
        });
        let (tx, rx) = mpsc::channel::<Vec<String>>();
        let handle = std::thread::spawn(move || {
            let mut bridges = Bridges { sink, ..Bridges::default() };
            // Block for the next list, then coalesce: drain everything already
            // queued and reconcile only against the newest, so a burst of
            // snapshots costs one reconcile.
            while let Ok(mut running) = rx.recv() {
                while let Ok(next) = rx.try_recv() {
                    running = next;
                }
                let refs: Vec<&str> = running.iter().map(String::as_str).collect();
                bridges.reconcile(&refs);
            }
            // Sender dropped: `bridges` drops here, killing every live bridge.
        });
        BridgeWorker { tx: Some(tx), handle: Some(handle) }
    }
}

/// Store one delivered message and report what to show, if anything. Split
/// out of the sink closure so the store write (the thing the daemon's `ok`
/// acknowledges) and the display decision are one read-modify-write.
///
/// Authorization is only evaluated for thread messages: `declares_dispatcher`
/// loads `state.toml` and the config, which a plain notify must not pay for.
fn apply_message(n: Notification) -> Result<Option<(String, crate::inbox::Shown)>, String> {
    use crate::commands::dispatch;
    let threaded = !matches!(n.message, notify::Message::Notify(_));
    let declares = threaded && dispatch::declares_dispatcher(&n.instance);
    let action = crate::inbox::decide(&n.instance, declares, n.message);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let shown = crate::inbox::store::update(|inbox| {
        inbox.apply_sink(&n.instance_id, &n.instance, now, action)
    })
    .map_err(|e| format!("{e:#}"))?;
    Ok(shown.map(|shown| (n.instance, shown)))
}

/// Handle to the bridge worker thread. Send running-container lists with
/// [`send`](Self::send); on drop the channel closes and the thread is joined,
/// so all bridges are killed before the caller (the TUI) restores the terminal.
pub struct BridgeWorker {
    tx: Option<mpsc::Sender<Vec<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl BridgeWorker {
    /// Hand the worker the current running-container set. Never blocks the UI
    /// thread; a dead worker (thread gone) is silently ignored.
    pub fn send(&self, running: Vec<String>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(running);
        }
    }
}

impl Drop for BridgeWorker {
    fn drop(&mut self) {
        // Close the channel first so the worker's `recv` returns and it drops
        // `Bridges` (killing every bridge), then join so that teardown finishes
        // before the terminal is restored.
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

    #[test]
    fn only_state_writing_ops_take_the_control_lock() {
        use crate::devsbd::control::Op;
        let locked: Vec<Op> = Op::ALL.into_iter().filter(|op| writes_state(*op)).collect();
        assert_eq!(locked, [Op::Ensure, Op::Stop, Op::Rm]);
    }

    #[test]
    fn retry_policy() {
        // No entry: always start one.
        assert_eq!(retry_decision(None), Retry::Spawn);
        // Alive (not done): keep, regardless of elapsed time.
        let (zero, day) = (Duration::ZERO, Duration::from_secs(86_400));
        assert_eq!(retry_decision(Some((false, false, zero))), Retry::Keep);
        assert_eq!(retry_decision(Some((false, false, day))), Retry::Keep);
        // Dead, not a mismatch: retried after RETRY.
        assert_eq!(retry_decision(Some((true, false, zero))), Retry::Keep);
        assert_eq!(retry_decision(Some((true, false, RETRY))), Retry::Respawn);
        // Dead on a mismatch: only after the long MISMATCH_RETRY.
        assert_eq!(retry_decision(Some((true, true, RETRY))), Retry::Keep);
        assert_eq!(retry_decision(Some((true, true, MISMATCH_RETRY))), Retry::Respawn);
    }

    #[test]
    fn caps_advertise_notify_and_control_only_with_a_sink() {
        const SERVED: u32 = caps::NOTIFY | caps::CONTROL;
        assert_eq!(own_caps(false, false), 0);
        assert_eq!(own_caps(true, false), caps::SSH_AGENT);
        assert_eq!(own_caps(false, true), SERVED);
        assert_eq!(own_caps(true, true), caps::SSH_AGENT | SERVED);
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
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let (to, rx) = services_with("web-1", control, accept);
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || d.serve(d_r, |_, _| None));
        std::thread::spawn(move || {
            let none: Option<fn() -> Option<PathBuf>> = None;
            host.serve(h_r, move |_, ch| open_stream(ch, none.as_ref(), Some(&to)))
        });
        (daemon, rx)
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
        assert!(open_stream(proto::channel::NOTIFY, none.as_ref(), None).is_none());
        assert!(open_stream(proto::channel::CONTROL, none.as_ref(), None).is_none());
        assert!(open_stream(proto::channel::SSH_AGENT, none.as_ref(), None).is_none());
    }

    /// Past `MAX_HANDLERS` live handler threads a notify/control `Open` is
    /// refused without spawning; a finished handler frees its slot. Agent
    /// streams don't take slots.
    #[test]
    fn handler_slots_bound_notify_and_control_opens() {
        let (to, _rx) = services("web-1", no_control());
        let held: Vec<_> = (0..MAX_HANDLERS).map(|_| HandlerSlot::acquire(&to.handlers).unwrap()).collect();
        let none: Option<fn() -> Option<PathBuf>> = None;
        assert!(open_stream(proto::channel::NOTIFY, none.as_ref(), Some(&to)).is_none());
        assert!(open_stream(proto::channel::CONTROL, none.as_ref(), Some(&to)).is_none());
        assert_eq!(to.handlers.load(Ordering::Acquire), MAX_HANDLERS);
        drop(held);
        assert_eq!(to.handlers.load(Ordering::Acquire), 0);
        // Accepted again; dropping our end ends the handler, freeing the slot.
        let conn = open_stream(proto::channel::NOTIFY, none.as_ref(), Some(&to));
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
        let worker = Bridges::spawn_worker(None);
        for i in 0..100 {
            worker.send(vec![format!("devsandbox-c{i}")]);
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
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
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

            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            if !ok(Command::new("docker").args(["exec", &name, "apk", "add", "--no-cache", "-q", "openssh-client-default"])) {
                return Err("apk add failed (no network?)");
            }
            let arch = install(&name, None).unwrap();
            let bridge = || {
                let agent_path = sock.clone();
                spawn_with(&name, hash(arch).unwrap(), Some(move || Some(agent_path.clone())), None).unwrap()
            };
            let ssh_add = || {
                Command::new("docker")
                    .args(["exec", "-e", "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock", &name, "ssh-add", "-l"])
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
            };
            let list = || String::from_utf8_lossy(&ssh_add().wait_with_output().unwrap().stdout).into_owned();
            let pidfile = || {
                let out = Command::new("docker").args(["exec", &name, "cat", "/run/devsandbox/devsbd.pid"]).output().unwrap();
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
            assert!(ok(Command::new("docker").args(["exec", &name, "sh", "-c", &fake])));
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
            assert!(ok(Command::new("docker").args(["exec", &name, "sh", "-c", killed])));
            let revived = bridge();
            assert_eq!(revived.outcome(Duration::from_secs(10)), Some(Ok(())), "revived daemon");
            assert!(list().contains("devsbd-relay-test"), "via revived daemon");
            Ok(())
        })
    }

    /// Docker-gated: an agent-less bridge (a `devsandbox port` forward) spawned
    /// *after* an agent bridge must not steal agent routing — `ssh-add -l` still
    /// lists the key via the older agent bridge. Regression for the routing fix
    /// in docs/port-forwarding.md (a shell with no agent starting a forward
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
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
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
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            if !ok(Command::new("docker").args(["exec", &name, "apk", "add", "--no-cache", "-q", "openssh-client-default"])) {
                return Err("apk add failed (no network?)");
            }
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let list = || {
                let out = Command::new("docker")
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
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let (to, rx) = services("notify-test", no_control());
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, Some(to)).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            assert!(ok(Command::new("docker").args(["exec", &name, BIN, "notify", "--key", "k", "hello"])));
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
                let out = Command::new("docker").args(["exec", &name, "sh", "-c", &list]).output().unwrap();
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
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);

            // No bridge yet, so nothing can drain: both puts sit in the outbox.
            let put = |title: &str| {
                let json = format!(r#"{{"key":"pr-1","title":"{title}","state":"active"}}"#);
                ok(Command::new("docker").args(["exec", &name, BIN, "thread", "put", "--json", &json]))
            };
            assert!(put("first"));
            assert!(put("second"));
            assert!(ok(Command::new("docker").args(["exec", &name, BIN, "notify", "--key", "pr-1", "hello"])));
            let count = format!("ls -A {} | wc -l", notify::OUTBOX);
            let out = Command::new("docker").args(["exec", &name, "sh", "-c", &count]).output().unwrap();
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

    /// Docker-gated end to end: `devsbd ensure` in a container reaches a
    /// sink-bearing bridge's control handler (a stub, so no real state is
    /// touched) and prints its answer; with no bridge it exits 75.
    #[test_utils::docker_test(helper)]
    fn serves_container_control_requests_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-control-test-{stamp}");
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let ensure = || Command::new("docker").args(["exec", &name, BIN, "ensure", "web", "--key", "pr-1"]).output().unwrap();

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
            let out = Command::new("docker").args(["exec", &name, BIN, "ensure", "web"]).output().unwrap();
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
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sh", "-c", serve])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none, None).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            assert_eq!(bridge.peer_caps(), Some(caps::TCP_FORWARD), "daemon advertises TCP_FORWARD");

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
}
