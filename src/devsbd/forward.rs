//! Host forwarder engine (docs/port-forwarding.md, step 5): binds a host TCP
//! listener and tunnels each accepted connection into an instance through a
//! `devsbd bridge`, whose daemon dials the target inside the container. Owns
//! one bridge per forward, respawned on death; the route is re-resolved on
//! every (re)spawn so a container restart or a service rebuild heals without
//! restarting the forward. Unix-only, like `bridge.rs`.
//!
//! Everything but the initial bind runs on background threads (a supervisor
//! that keeps the bridge alive, an accept loop, and a per-connection relay via
//! the mux): `start` never blocks the caller on docker. Nothing is written to
//! stderr — the TUI owns the screen — so errors surface through `status` and
//! `drain_notes`.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::bridge::{self, Bridge};
use super::hash;
use crate::devsbd::Arch;

/// Where a connection should be dialed, resolved fresh for each (re)spawn of
/// the bridge. `container` is the instance whose helper carries the tunnel;
/// the daemon there dials `host:port` (loopback for an instance forward, a
/// service alias otherwise). `arch` is the instance's recorded helper arch, or
/// `None` when unknown (a pre-embedding instance) — only the Hello's
/// informational hash uses it. `label` is what the UI shows, e.g. `api:3000`
/// or `postgres:5432 (via instance api)`. `process_container` is where the
/// forwarded port actually lives — for the listening-process lookup (`lsof`):
/// the dialed `container` for an instance/injection forward, but the *service*
/// container for a via-instance route (the instance can't see the service's
/// processes).
pub struct Route {
    pub container: String,
    pub arch: Option<Arch>,
    pub host: String,
    pub port: u16,
    pub label: String,
    pub process_container: String,
}

/// The host port to bind. `Fixed` fails if the port is taken; `Prefer` falls
/// back to an OS-assigned port (bind 0) when its preferred port is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPort {
    Fixed(u16),
    Prefer(u16),
}

/// Everything `Forward::start` needs. `resolve` is called on the supervisor
/// thread for each bridge (re)spawn, so it may touch config/state/docker; it
/// must not be called on the caller's thread.
pub struct ForwardSpec {
    pub bind: IpAddr,
    pub host_port: HostPort,
    pub resolve: Box<dyn Fn() -> Result<Route, String> + Send + Sync>,
    /// Listening-process lookup for the forwarded port, run in the route's
    /// `process_container` (docs/port-forwarding.md, _Listening process_).
    /// Called only on the forward's own (supervisor) thread, never the caller's;
    /// any failure yields `None`. Passed in so `forward.rs` stays runtime- and
    /// `commands`-generic and the pure lifecycle tests can inject a stub.
    pub probe: Box<dyn Fn(&str, u16) -> Option<String> + Send + Sync>,
}

/// Retry gap after a bridge dies before its route is re-resolved and a new one
/// spawned. Short so a transient outage (daemon restart) heals quickly; a
/// version mismatch waits longer, since only a helper rewrite (instance
/// restart) can fix it, and re-exec'ing every gap is pointless.
const RETRY: Duration = Duration::from_secs(2);
const MISMATCH_RETRY: Duration = Duration::from_secs(30);

/// How long the supervisor waits for a fresh bridge's handshake before
/// declaring it failed. Matches the bridge's own handshake watchdog: past this
/// the bridge has already killed its `exec`, so `outcome` yields the error.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(10);

/// Poll cadence for the nonblocking accept loop, so `Drop` can stop it
/// promptly without `accept` blocking forever on a quiet listener.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Minimum gap between listening-process (`lsof`) probes while a forward is
/// Active. Runs on the supervisor thread, folded into its bridge-liveness poll;
/// an exec is ~100ms, so this keeps the poll's overhead negligible while still
/// catching a dev server that restarts under a new pid within ~10s.
const PROBE_REFRESH: Duration = Duration::from_secs(10);

/// The forwarder's coarse state, for the UI. `Connecting` = no live bridge yet
/// (initial spawn or a retry in flight); `Active` = a bridge handshook and is
/// carrying (or ready to carry) connections; `Error` = the last resolve/spawn
/// failed and we're in the retry gap, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardState {
    Connecting,
    Active,
    Error(String),
}

/// A snapshot of a forward, cheap to poll from the CLI loop or the TUI worker.
#[derive(Debug, Clone)]
pub struct ForwardStatus {
    pub local_addr: SocketAddr,
    pub route_label: String,
    pub state: ForwardState,
    // Read by the TUI Ports tab worker (docs/port-forwarding.md, step 10); the
    // CLI doesn't surface it.
    pub open_conns: usize,
    /// The process listening on the forwarded port, e.g. `node (pid 412)`, or
    /// `None` when unknown (no `lsof`, no match, or not yet probed). Refreshed
    /// on the forward's own thread; cleared whenever the bridge isn't Active.
    pub process: Option<String>,
}

/// A live forward. Dropping it stops accepting, kills the bridge (which ends
/// the mux, shutting every open connection), and joins the threads, so nothing
/// outlives it.
pub struct Forward {
    local_addr: SocketAddr,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
    supervisor: Option<std::thread::JoinHandle<()>>,
}

/// State shared between the supervisor (writer of `bridge`/`state`), the accept
/// loop (reader of both), and `status`/`drain_notes` callers.
struct Shared {
    /// The current bridge and its coarse state; `bridge` is `Some` only once a
    /// handshake succeeds. A single lock so a connection sees a consistent
    /// (state, bridge) pair.
    inner: Mutex<Inner>,
    /// Open connections currently relayed through the bridge; also surfaced in
    /// the status. An `AtomicUsize` so `on_close` (which may run on the mux
    /// reader thread) can decrement it without taking the lock.
    open_conns: AtomicUsize,
    /// Per-connection error notes for the UI, drained by `drain_notes`.
    notes: Mutex<Notes>,
    route_label: Mutex<String>,
    /// Where the daemon should dial for this bridge (`host`, `port`), set by
    /// the supervisor on each (re)spawn and read by `handle_conn`.
    dial: Mutex<(String, u16)>,
    /// Listening-process lookup; run only on the supervisor thread.
    probe: Box<dyn Fn(&str, u16) -> Option<String> + Send + Sync>,
    /// Where to probe for the listening process (`process_container`, port), set
    /// by the supervisor on each (re)spawn.
    probe_target: Mutex<(String, u16)>,
    /// The last known listening process, refreshed on the supervisor thread and
    /// read by `status`. `None` whenever the forward isn't Active.
    process: Mutex<Option<String>>,
    /// Per-connection handler threads (each does the handshake wait + hands the
    /// socket to the mux, then exits). Kept so `Drop` joins them; finished ones
    /// are pruned as new connections arrive.
    handlers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

struct Inner {
    // `Bridge` is `Send` but not `Sync` (its handshake channel), so it can't be
    // shared as `&Bridge` across threads. A `Mutex` makes the handle `Sync`;
    // `connect` under it is quick (register a stream, send one frame), so the
    // brief hold doesn't stall concurrent clients meaningfully.
    bridge: Option<Arc<Mutex<Bridge>>>,
    state: ForwardState,
}

/// Notes with consecutive-duplicate suppression: a broken bridge closing many
/// connections with the same reason (or a storm of `""`/`bridge closed`)
/// mustn't flood the UI.
#[derive(Default)]
struct Notes {
    pending: Vec<String>,
    last: Option<String>,
}

impl Notes {
    /// Record `note` unless it's empty, a bare `bridge closed` (routine
    /// teardown), or identical to the previous one.
    fn push(&mut self, note: String) {
        if note.is_empty() || note == "bridge closed" {
            return;
        }
        if self.last.as_deref() == Some(note.as_str()) {
            return;
        }
        self.last = Some(note.clone());
        self.pending.push(note);
    }
}

impl Forward {
    /// Bind the listener (errors here are immediate and precise), then start
    /// the accept loop and the bridge supervisor. The bridge is resolved and
    /// spawned eagerly, so a bad route or a dead daemon shows up in `status`
    /// without waiting for a first client.
    pub fn start(spec: ForwardSpec) -> io::Result<Forward> {
        let ForwardSpec { bind, host_port, resolve, probe } = spec;
        let listener = bind_listener(bind, host_port)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;

        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner { bridge: None, state: ForwardState::Connecting }),
            open_conns: AtomicUsize::new(0),
            notes: Mutex::new(Notes::default()),
            route_label: Mutex::new(String::new()),
            dial: Mutex::new((String::new(), 0)),
            probe,
            probe_target: Mutex::new((String::new(), 0)),
            process: Mutex::new(None),
            handlers: Mutex::new(Vec::new()),
        });
        let stop = Arc::new(AtomicBool::new(false));

        let supervisor = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || supervise(resolve, shared, stop))
        };
        let accept = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || accept_loop(listener, shared, stop))
        };

        Ok(Forward { local_addr, shared, stop, accept: Some(accept), supervisor: Some(supervisor) })
    }

    /// A consistent snapshot for the UI/CLI.
    pub fn status(&self) -> ForwardStatus {
        let inner = self.shared.inner.lock().unwrap();
        ForwardStatus {
            local_addr: self.local_addr,
            route_label: self.shared.route_label.lock().unwrap().clone(),
            state: inner.state.clone(),
            open_conns: self.shared.open_conns.load(Ordering::Relaxed),
            process: self.shared.process.lock().unwrap().clone(),
        }
    }

    /// Take the per-connection error notes accumulated since the last drain.
    pub fn drain_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.notes.lock().unwrap().pending)
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        // Stop accepting and end the supervisor's retry sleeps.
        self.stop.store(true, Ordering::Relaxed);
        // Drop the bridge: ends its mux, so every open connection's local
        // socket is shut and its relay threads finish.
        self.shared.inner.lock().unwrap().bridge = None;
        for handle in [self.accept.take(), self.supervisor.take()].into_iter().flatten() {
            let _ = handle.join();
        }
        // Per-connection handlers: `stop` breaks any handshake wait, the
        // dropped bridge unblocks any in-flight `connect`. Join so none
        // outlives the Forward.
        for handle in std::mem::take(&mut *self.shared.handlers.lock().unwrap()) {
            let _ = handle.join();
        }
    }
}

/// Bind the requested host port, honouring the `Prefer` fallback. `Fixed` maps
/// any bind error straight through (a clear "address already in use"); `Prefer`
/// retries on port 0 when its port is taken, so a busy default port doesn't
/// abort the forward.
fn bind_listener(bind: IpAddr, host_port: HostPort) -> io::Result<TcpListener> {
    match host_port {
        HostPort::Fixed(port) => TcpListener::bind(SocketAddr::new(bind, port)),
        HostPort::Prefer(port) => match TcpListener::bind(SocketAddr::new(bind, port)) {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => TcpListener::bind(SocketAddr::new(bind, 0)),
            other => other,
        },
    }
}

/// Accept connections until `stop`. Each accepted socket is dialed through the
/// current bridge; a nonblocking listener + short sleep lets `Drop` stop the
/// loop promptly (std `accept` would otherwise block on a quiet port).
fn accept_loop(listener: TcpListener, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((sock, _)) => {
                // macOS (BSD) sockets inherit the listener's O_NONBLOCK on
                // accept, Linux ones don't; the relay reads blocking, so clear
                // it or every read fails with WouldBlock.
                if sock.set_nonblocking(false).is_err() {
                    continue;
                }
                // A handler may wait (bounded) for a Connecting bridge; run it
                // off the accept loop so one client's wait can't stall the next
                // accept. Track it so `Drop` joins it, pruning finished ones.
                let handle = {
                    let (shared, stop) = (Arc::clone(&shared), Arc::clone(&stop));
                    std::thread::spawn(move || handle_conn(sock, &shared, &stop))
                };
                let mut handlers = shared.handlers.lock().unwrap();
                handlers.retain(|h| !h.is_finished());
                handlers.push(handle);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            // Interrupted: retry. Any other error is the listener itself
            // failing; stop rather than spin.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Dial one accepted socket through the bridge. While the bridge is
/// `Connecting`, wait (bounded) for the handshake rather than dropping the
/// client; in `Error`/retry-gap, close it at once with a note so a broken
/// bridge doesn't re-exec per connection.
fn handle_conn(sock: TcpStream, shared: &Arc<Shared>, stop: &Arc<AtomicBool>) {
    let _ = sock.set_nodelay(true);
    let deadline = Instant::now() + HANDSHAKE_WAIT;
    let bridge = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let (bridge, state) = {
            let inner = shared.inner.lock().unwrap();
            (inner.bridge.clone(), inner.state.clone())
        };
        match (bridge, state) {
            (Some(bridge), _) => break bridge,
            // Retry gap / hard error: don't hold the client waiting.
            (None, ForwardState::Error(reason)) => {
                shared.notes.lock().unwrap().push(format!("connection dropped: {reason}"));
                return;
            }
            // Connecting: the supervisor is spawning a bridge; wait for it.
            (None, _) => {
                if Instant::now() >= deadline {
                    shared.notes.lock().unwrap().push("connection dropped: bridge not ready".into());
                    return;
                }
                std::thread::sleep(ACCEPT_POLL);
            }
        }
    };

    shared.open_conns.fetch_add(1, Ordering::Relaxed);
    let (host, port) = { shared.dial.lock().unwrap().clone() };
    let on_close = {
        let shared = Arc::clone(shared);
        move |reason: String| {
            shared.open_conns.fetch_sub(1, Ordering::Relaxed);
            shared.notes.lock().unwrap().push(reason);
        }
    };
    let result = bridge.lock().unwrap().connect(sock, &host, port, on_close);
    if let Err(e) = result {
        // connect didn't take ownership of the callback, so undo the increment
        // and record why (a dead bridge, an outdated helper).
        shared.open_conns.fetch_sub(1, Ordering::Relaxed);
        shared.notes.lock().unwrap().push(format!("connect failed: {e}"));
    }
}

/// Keep a bridge alive for the forward. Resolve + spawn eagerly; when the
/// bridge dies, wait a retry gap (longer for a version mismatch), re-resolve
/// (so route changes heal), and spawn again. Never re-exec per connection.
fn supervise(resolve: Box<dyn Fn() -> Result<Route, String> + Send + Sync>, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let gap = match try_spawn(&resolve, &shared, &stop) {
            SpawnOutcome::Serving(bridge) => {
                // Just became Active: probe the listening process once now, then
                // periodically inside `wait_until_dead` (docs, _Listening
                // process_). On the supervisor thread, never the caller's.
                refresh_process(&shared);
                wait_until_dead(&bridge, &shared, &stop);
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                // Bridge died: drop it, mark connecting, clear the process (it's
                // no longer known), retry per its cause.
                let mismatch = bridge.lock().unwrap().is_mismatch();
                {
                    let mut inner = shared.inner.lock().unwrap();
                    inner.bridge = None;
                    inner.state = ForwardState::Connecting;
                }
                *shared.process.lock().unwrap() = None;
                drop(bridge);
                retry_gap(mismatch)
            }
            SpawnOutcome::Failed { mismatch } => retry_gap(mismatch),
        };
        sleep_interruptible(gap, &stop);
    }
}

enum SpawnOutcome {
    /// A live, handshook bridge is now serving.
    Serving(Arc<Mutex<Bridge>>),
    /// Resolve or the handshake failed; `mismatch` picks the retry gap.
    Failed { mismatch: bool },
}

/// Gap before the next spawn after a bridge died or failed: short normally, so
/// a transient outage heals fast, but long after a version mismatch, which only
/// a helper rewrite (instance restart) can fix — re-exec'ing sooner is wasted.
fn retry_gap(mismatch: bool) -> Duration {
    if mismatch { MISMATCH_RETRY } else { RETRY }
}

/// Resolve the route and spawn one bridge, waiting for its handshake. Publishes
/// the result into `shared` (state + label + bridge). On any failure records a
/// note and leaves the forward in `Error`.
fn try_spawn(resolve: &(dyn Fn() -> Result<Route, String> + Send + Sync), shared: &Arc<Shared>, stop: &Arc<AtomicBool>) -> SpawnOutcome {
    let route = match resolve() {
        Ok(route) => route,
        Err(reason) => {
            set_error(shared, reason);
            return SpawnOutcome::Failed { mismatch: false };
        }
    };
    *shared.route_label.lock().unwrap() = route.label.clone();
    *shared.dial.lock().unwrap() = (route.host.clone(), route.port);
    *shared.probe_target.lock().unwrap() = (route.process_container.clone(), route.port);

    let hash = route.arch.and_then(hash).unwrap_or_default();
    let Some(bridge) = bridge::spawn_for(&route.container, hash, true) else {
        set_error(shared, format!("could not exec into {}", route.container));
        return SpawnOutcome::Failed { mismatch: false };
    };
    // The handshake channel is consumed here (still sole owner), before the
    // bridge is wrapped for sharing across accept threads. Wait in short slices
    // so a `Drop` mid-handshake bails within a slice instead of blocking up to
    // HANDSHAKE_WAIT (~10s): dropping `bridge` here kills its `exec`.
    match wait_handshake(&bridge, stop) {
        Handshake::Ok => {
            let bridge = Arc::new(Mutex::new(bridge));
            {
                let mut inner = shared.inner.lock().unwrap();
                inner.bridge = Some(Arc::clone(&bridge));
                inner.state = ForwardState::Active;
            }
            SpawnOutcome::Serving(bridge)
        }
        Handshake::Err(reason) => {
            let mismatch = bridge.is_mismatch();
            set_error(shared, reason);
            SpawnOutcome::Failed { mismatch }
        }
        Handshake::TimedOut => {
            let mismatch = bridge.is_mismatch();
            set_error(shared, "helper handshake timed out".into());
            SpawnOutcome::Failed { mismatch }
        }
        // `Drop` set `stop` mid-handshake: bail silently (no note). Dropping
        // `bridge` here kills its `exec`, so the supervisor exits at once.
        Handshake::Stopped => SpawnOutcome::Failed { mismatch: false },
    }
}

/// Slice of the handshake wait, so a `stop` set by `Drop` is noticed within one
/// slice instead of blocking the whole `HANDSHAKE_WAIT`.
const HANDSHAKE_SLICE: Duration = Duration::from_millis(100);

/// Outcome of waiting for a fresh bridge's handshake.
enum Handshake {
    /// The handshake succeeded.
    Ok,
    /// The handshake failed with this reason.
    Err(String),
    /// No result within `HANDSHAKE_WAIT` (the bridge's own watchdog has already
    /// killed the `exec`).
    TimedOut,
    /// `stop` was set (a `Drop`) before the handshake resolved.
    Stopped,
}

/// Wait for `bridge`'s handshake in `HANDSHAKE_SLICE` slices up to
/// `HANDSHAKE_WAIT`, checking `stop` between slices so a `Drop` mid-handshake
/// returns promptly (the caller then drops the bridge, killing its `exec`)
/// rather than hanging for seconds. `outcome` yields its result at most once, so
/// a slice that returns `Some(..)` is the final answer.
fn wait_handshake(bridge: &Bridge, stop: &Arc<AtomicBool>) -> Handshake {
    let deadline = Instant::now() + HANDSHAKE_WAIT;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Handshake::Stopped;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Handshake::TimedOut;
        }
        match bridge.outcome(HANDSHAKE_SLICE.min(remaining)) {
            Some(Ok(())) => return Handshake::Ok,
            Some(Err(reason)) => return Handshake::Err(reason),
            // Slice elapsed with no result yet: loop to re-check `stop`.
            None => {}
        }
    }
}

fn set_error(shared: &Arc<Shared>, reason: String) {
    let mut inner = shared.inner.lock().unwrap();
    inner.state = ForwardState::Error(reason.clone());
    inner.bridge = None;
    drop(inner);
    // Leaving Active: the listening process is no longer known.
    *shared.process.lock().unwrap() = None;
    shared.notes.lock().unwrap().push(reason);
}

/// Run the listening-process probe in the route's `process_container` and store
/// the result. Only ever called on the supervisor thread; any failure (no
/// `lsof`, no match, container gone) leaves `None` (the probe returns `None`).
fn refresh_process(shared: &Arc<Shared>) {
    let (container, port) = shared.probe_target.lock().unwrap().clone();
    let found = (shared.probe)(&container, port);
    *shared.process.lock().unwrap() = found;
}

/// Poll the bridge until it reports done or `stop` is set, so the supervisor
/// can respawn promptly without a keepalive of its own. Folds in the periodic
/// listening-process refresh (at most every `PROBE_REFRESH`) so it stays on
/// this thread — an exec of ~100ms every 10s is a negligible add to the poll.
fn wait_until_dead(bridge: &Arc<Mutex<Bridge>>, shared: &Arc<Shared>, stop: &Arc<AtomicBool>) {
    let mut next_probe = Instant::now() + PROBE_REFRESH;
    while !stop.load(Ordering::Relaxed) && !bridge.lock().unwrap().is_done() {
        if Instant::now() >= next_probe {
            refresh_process(shared);
            next_probe = Instant::now() + PROBE_REFRESH;
        }
        std::thread::sleep(ACCEPT_POLL);
    }
}

/// Sleep `dur` but wake as soon as `stop` is set, so `Drop` doesn't wait out a
/// full retry gap.
fn sleep_interruptible(dur: Duration, stop: &Arc<AtomicBool>) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(ACCEPT_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::Ipv4Addr;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    // ---- pure: retry gap ----

    #[test]
    fn retry_gap_is_short_unless_a_mismatch() {
        assert_eq!(retry_gap(false), RETRY);
        assert_eq!(retry_gap(true), MISMATCH_RETRY);
        assert!(MISMATCH_RETRY > RETRY);
    }

    // ---- pure: note dedup ----

    #[test]
    fn notes_drop_empty_bridge_closed_and_consecutive_dupes() {
        let mut n = Notes::default();
        n.push(String::new()); // empty: dropped
        n.push("bridge closed".into()); // routine teardown: dropped
        n.push("connection refused (127.0.0.1:3000)".into());
        n.push("connection refused (127.0.0.1:3000)".into()); // consecutive dupe
        n.push("no such host db".into());
        n.push("connection refused (127.0.0.1:3000)".into()); // not consecutive: kept
        assert_eq!(
            n.pending,
            vec![
                "connection refused (127.0.0.1:3000)".to_string(),
                "no such host db".into(),
                "connection refused (127.0.0.1:3000)".into(),
            ]
        );
    }

    // ---- HostPort bind fallback, against real loopback listeners ----

    #[test]
    fn prefer_falls_back_to_an_os_port_when_taken() {
        // Occupy a port, then Prefer(that port) must bind a *different* one.
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        let listener = bind_listener(LOOPBACK, HostPort::Prefer(port)).unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), port, "fell back off the busy port");
    }

    #[test]
    fn prefer_uses_the_requested_port_when_free() {
        // Reserve then release a port so it's free. Parallel tests bind
        // ephemeral loopback ports too, so one can grab it before we rebind and
        // Prefer (correctly) falls back; retry with a fresh port. A Prefer that
        // ignored a free port would miss on every attempt, so this still fails.
        for _ in 0..20 {
            let port = {
                let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                l.local_addr().unwrap().port()
            };
            let listener = bind_listener(LOOPBACK, HostPort::Prefer(port)).unwrap();
            if listener.local_addr().unwrap().port() == port {
                return;
            }
        }
        panic!("Prefer never bound the requested port in 20 attempts");
    }

    #[test]
    fn fixed_errors_when_the_port_is_taken() {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        let err = bind_listener(LOOPBACK, HostPort::Fixed(port)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    // ---- Forward lifecycle without docker: a resolve that always fails ----

    /// `start` binds synchronously (so this never blocks on docker), the
    /// supervisor drives the failing resolve into `Error` with a note, and
    /// `Drop` joins cleanly. A client that connects during the outage is closed
    /// at once (a note), not held.
    #[test]
    fn failing_resolve_surfaces_error_and_drops_cleanly() {
        let forward = Forward::start(ForwardSpec {
            bind: LOOPBACK,
            host_port: HostPort::Prefer(0),
            resolve: Box::new(|| Err("no running container for service redis".into())),
            probe: no_probe(),
        })
        .unwrap();
        let addr = forward.status().local_addr;
        assert_eq!(addr.ip(), LOOPBACK);

        // Wait for the supervisor's first resolve to land us in Error.
        let mut errored = false;
        for _ in 0..200 {
            if let ForwardState::Error(reason) = forward.status().state {
                assert!(reason.contains("redis"), "{reason}");
                errored = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(errored, "state never reached Error");

        // A client during the outage is closed promptly with a note.
        let mut c = TcpStream::connect(addr).unwrap();
        let mut buf = [0u8; 1];
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(c.read(&mut buf).unwrap(), 0, "connection closed during outage");
        let mut noted = false;
        for _ in 0..200 {
            if forward.drain_notes().iter().any(|n| n.contains("redis")) {
                noted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(noted, "no note for the dropped connection");
        drop(forward); // joins without deadlock
    }

    // ---- docker-gated end-to-end ----

    use crate::devsbd::{install, Arch};
    use std::process::Command;

    fn ok(cmd: &mut Command) -> bool {
        matches!(cmd.output(), Ok(o) if o.status.success())
    }

    /// A `Route` closure targeting `container`'s loopback `port` (step 6 does
    /// the real resolution; here a fixed route is enough).
    fn loopback_route(container: String, arch: Arch, port: u16) -> Box<dyn Fn() -> Result<Route, String> + Send + Sync> {
        Box::new(move || {
            Ok(Route {
                container: container.clone(),
                arch: Some(arch),
                host: "127.0.0.1".into(),
                port,
                label: format!("test:{port}"),
                process_container: container.clone(),
            })
        })
    }

    /// A probe stub that never finds a process — the lifecycle/docker echo
    /// tests don't care about the listening-process column.
    fn no_probe() -> Box<dyn Fn(&str, u16) -> Option<String> + Send + Sync> {
        Box::new(|_, _| None)
    }

    fn wait_active(forward: &Forward) -> bool {
        for _ in 0..300 {
            if forward.status().state == ForwardState::Active {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Docker-gated: forward to a loopback-only echo server inside the
    /// container. Round-trips a small message, then 20 MiB (written and read
    /// concurrently so the echo drains both ways), then several concurrent
    /// clients. The server (`nc -lk … -e cat`) is the container's main command
    /// so a later `docker restart` brings it back.
    #[test_utils::docker_test(helper)]
    fn forwards_to_a_loopback_echo_server_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-fwd-echo-{stamp}");
        // Loopback-only echo, reachable only from inside the netns (where the
        // daemon dials). `-lk` keeps serving across connects.
        let serve = "busybox nc -lk -s 127.0.0.1 -p 8080 -e cat";
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sh", "-c", serve])));
            let arch = install(&name, None).unwrap();

            let forward = Forward::start(ForwardSpec {
                bind: LOOPBACK,
                host_port: HostPort::Prefer(0),
                resolve: loopback_route(name.clone(), arch, 8080),
                probe: no_probe(),
            })
            .unwrap();
            assert!(wait_active(&forward), "forward never became Active: {:?}", forward.status().state);
            let addr = forward.status().local_addr;

            // Small round-trip. `nc -lk` may not have bound yet the instant the
            // container starts, so retry the whole client.
            let small = || -> io::Result<bool> {
                let mut c = TcpStream::connect(addr)?;
                c.set_read_timeout(Some(Duration::from_secs(5)))?;
                c.write_all(b"ping\n")?;
                let mut buf = [0u8; 5];
                c.read_exact(&mut buf)?;
                Ok(&buf == b"ping\n")
            };
            let mut round_tripped = false;
            for _ in 0..40 {
                if matches!(small(), Ok(true)) {
                    round_tripped = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            assert!(round_tripped, "small echo did not round-trip");

            // 20 MiB, writing and reading concurrently (echo needs both drained).
            const N: usize = 20 << 20;
            let mut c = TcpStream::connect(addr).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let mut w = c.try_clone().unwrap();
            let writer = std::thread::spawn(move || {
                let mut off = 0;
                let mut buf = vec![0u8; 64 * 1024];
                while off < N {
                    let len = buf.len().min(N - off);
                    for (j, b) in buf[..len].iter_mut().enumerate() {
                        *b = (off + j) as u8;
                    }
                    w.write_all(&buf[..len]).unwrap();
                    off += len;
                }
                w.shutdown(std::net::Shutdown::Write).unwrap();
            });
            let mut got = 0;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = c.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                for (j, &b) in buf[..n].iter().enumerate() {
                    assert_eq!(b, (got + j) as u8, "byte {} differs", got + j);
                }
                got += n;
            }
            writer.join().unwrap();
            assert_eq!(got, N, "20 MiB echoed intact");

            // Several concurrent clients each round-trip a distinct message.
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    std::thread::spawn(move || {
                        let msg = format!("hello-{i}\n");
                        let mut c = TcpStream::connect(addr).unwrap();
                        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                        c.write_all(msg.as_bytes()).unwrap();
                        let mut buf = vec![0u8; msg.len()];
                        c.read_exact(&mut buf).unwrap();
                        assert_eq!(buf, msg.as_bytes());
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            drop(forward);
            Ok(())
        })
    }

    /// Docker-gated: a forward to a *closed* container port. The client
    /// connection closes and a note mentions "refused".
    #[test_utils::docker_test(helper)]
    fn forward_to_a_closed_port_notes_refused_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-fwd-closed-{stamp}");
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            let arch = install(&name, None).unwrap();
            // Port 9: nothing listening → the daemon's dial is refused.
            let forward = Forward::start(ForwardSpec {
                bind: LOOPBACK,
                host_port: HostPort::Prefer(0),
                resolve: loopback_route(name.clone(), arch, 9),
                probe: no_probe(),
            })
            .unwrap();
            assert!(wait_active(&forward), "forward never became Active: {:?}", forward.status().state);
            let addr = forward.status().local_addr;

            let mut c = TcpStream::connect(addr).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut buf = [0u8; 1];
            assert_eq!(c.read(&mut buf).unwrap(), 0, "refused dial closes the client");

            let mut refused = false;
            for _ in 0..200 {
                if forward.drain_notes().iter().any(|n| n.contains("refused")) {
                    refused = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(refused, "no note mentioning refused");
            drop(forward);
            Ok(())
        })
    }

    /// Docker-gated heal: `docker restart` the container (its main command is
    /// the echo server, so it comes back). After the retry gap the forward
    /// re-resolves, respawns a bridge (the helper binary survives in the
    /// writable layer; the bridge self-starts the daemon), and a new client
    /// works again.
    #[test_utils::docker_test(helper)]
    fn forward_heals_after_a_container_restart_with_docker() -> Result<(), &'static str> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-fwd-heal-{stamp}");
        let serve = "busybox nc -lk -s 127.0.0.1 -p 8080 -e cat";
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        let echo_ok = |addr: SocketAddr| -> bool {
            for _ in 0..40 {
                if let Ok(mut c) = TcpStream::connect(addr) {
                    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
                    if c.write_all(b"ping\n").is_ok() {
                        let mut buf = [0u8; 5];
                        if matches!(c.read_exact(&mut buf), Ok(())) && &buf == b"ping\n" {
                            return true;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            false
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sh", "-c", serve])));
            let arch = install(&name, None).unwrap();
            let forward = Forward::start(ForwardSpec {
                bind: LOOPBACK,
                host_port: HostPort::Prefer(0),
                resolve: loopback_route(name.clone(), arch, 8080),
                probe: no_probe(),
            })
            .unwrap();
            assert!(wait_active(&forward), "forward never became Active: {:?}", forward.status().state);
            let addr = forward.status().local_addr;
            assert!(echo_ok(addr), "echo before restart");

            // Restart the container: the bridge's exec dies, the daemon is gone.
            assert!(ok(Command::new("docker").args(["restart", "-t", "1", &name])));
            // The supervisor notices, waits the retry gap, re-resolves and
            // respawns; the helper survives the restart so the bridge revives.
            assert!(echo_ok(addr), "echo after restart (healed)");
            assert_eq!(forward.status().state, ForwardState::Active, "back to Active");
            drop(forward);
            Ok(())
        })
    }
}
