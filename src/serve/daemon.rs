//! The daemon side of `devsandbox serve`: start lock, accept loop, version
//! handoff and idle exit (docs/serve.md).
//!
//! Threading: the calling thread runs the accept loop, waking every [`TICK`]
//! to re-check the idle countdown and the stop request (handoff, `shutdown`);
//! each connection gets its own thread reading JSON lines and answering them
//! (the API methods are in `api.rs`). Exiting (idle, handoff or shutdown) closes the listener and removes
//! the socket first, sends subscribers a `closing` notification, then shuts
//! down the read side of every connection, so idle ones end at once while a
//! request in flight still writes its response, and waits up to
//! [`DRAIN_TIMEOUT`] for them. Then the live host side ([`Host`]: bridges,
//! autostart) stops, killing every bridge, and so does the forward
//! [`Registry`], closing every forward's listener. The start lock is released
//! last, so a successor never binds (or bridges) while this one drains.
//!
//! Notifications (docs/api.md): one daemon-wide `serve-watch` thread notices
//! changes (store writes by this process at once, by others within [`WATCH`]
//! via the file stamp; `state.toml` by its stamp; the host's container poll)
//! and only *flags* them on
//! each subscribed connection's [`Outbox`]; it never does I/O, so a slow
//! client can't hold up the others. A connection's first `subscribe` starts
//! its `serve-notify-<n>` thread, which writes the flagged notifications.
//! Flags coalesce (one pending `inbox.changed` however many writes; the
//! host's sink posts `inbox.shown` the same way, latest line only, and the
//! forward registry `forwards.changed` / `forwards.status`), so the
//! queue is bounded by construction; every write to a connection, response
//! or notification, goes through its one writer lock, and a client that
//! doesn't drain its socket for [`WRITE_TIMEOUT`] is disconnected.
//!
//! A verb request (`verbs.rs`) runs on its connection's thread until the
//! verb's answer, writing `output`/`progress` through the same writer lock
//! and watching the socket for a hang-up ([`ConnPeer`]). The drain's
//! read-side shutdown ends it too, but sets `draining` first, so the verb
//! is left running rather than cancelled, and doesn't hold the drain; the
//! successor adopts it by its spool files and holds until it exits.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::api::{self, Topic};
use super::endpoint::{self, Listener, Stream};
use super::forwards::{self, Registry};
use super::host::{Host, Waker};
use super::idle::{self, Decision, Holders};
use super::proto::{self, HelloParams, HelloResult, Notification, Request, Response, Version};
use super::verbs;
use crate::inbox::ops;

/// How often the accept loop wakes without a connection.
const TICK: Duration = Duration::from_millis(100);
/// How long an exiting daemon waits for in-flight requests.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a starting daemon waits for a lock whose holder doesn't answer
/// (starting up, or draining after a handoff). Longer than [`DRAIN_TIMEOUT`].
const LOCK_WAIT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(50);
/// How often the watcher re-checks the store file for writes by other
/// processes (the dashboard, `devsandbox rm`); this process's own writes
/// wake it at once.
const WATCH: Duration = Duration::from_millis(500);
/// A client that doesn't drain its socket for this long is disconnected.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Options {
    pub version: Version,
    /// No idle exit: `--keep-alive` (the unit `serve install` writes passes
    /// it) or `[serve] keep-alive = true` in the per-user global config
    /// ([`crate::daemon_config`]), resolved once by [`serve`].
    pub keep_alive: bool,
    /// [`idle::IDLE_TIMEOUT`] outside tests.
    pub idle_timeout: Duration,
    /// Run the live host side ([`Host`]: bridges, popups, autostart). Off in
    /// tests, which must not touch the user's state or containers.
    pub host: bool,
    /// The Inbox store the API serves; `None` is the default
    /// (`inbox::store::path`). Tests point it at their own file.
    pub inbox: Option<PathBuf>,
    /// How verbs run; `None` is the default ([`verbs::Runner::new`]: this
    /// binary, the user's state). Tests run scripts on scratch files.
    pub verbs: Option<verbs::Runner>,
    /// The `state.toml` the watcher stamps for `instances.changed`; `None`
    /// is the default (`State::path`). Tests point it at their own file.
    pub state: Option<PathBuf>,
}

impl Options {
    pub fn new(keep_alive: bool) -> Self {
        Self {
            version: Version::current(),
            keep_alive,
            idle_timeout: idle::IDLE_TIMEOUT,
            host: true,
            inbox: None,
            verbs: None,
            state: None,
        }
    }
}

/// Why [`run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Another daemon holds the lock and answers: nothing to do.
    AlreadyRunning,
    Idle,
    /// A newer client asked; it starts the successor.
    Handoff,
    /// A client sent `shutdown` (`serve install`, before the service manager
    /// starts its own daemon).
    Shutdown,
}

/// `devsandbox serve`: run the daemon in the foreground until it idles out or
/// hands off. `socket_dir` overrides [`endpoint::socket_dir`] (the lazy start
/// passes the dir its client resolved, so both agree). The global config is
/// read once here; a bad one is logged and ignored so the daemon still starts.
pub fn serve(socket_dir: Option<PathBuf>, keep_alive: bool) -> Result<()> {
    let dir = match socket_dir {
        Some(dir) => dir,
        None => endpoint::socket_dir()?,
    };
    let config = crate::daemon_config::load().unwrap_or_else(|e| {
        log(&format!("warning: {e:#}; using defaults"));
        Default::default()
    });
    let opts = Options::new(keep_alive || config.serve.keep_alive);
    match run(&dir, &opts)? {
        Exit::AlreadyRunning => {}
        Exit::Idle => log("idle, exiting"),
        Exit::Handoff => log("handed off to a newer version, exiting"),
        Exit::Shutdown => log("shutdown requested, exiting"),
    }
    Ok(())
}

/// Run a daemon on `dir` (created 0700 if needed). Returns at once with
/// [`Exit::AlreadyRunning`] when another daemon owns the dir.
pub fn run(dir: &Path, opts: &Options) -> Result<Exit> {
    endpoint::ensure_private_dir(dir)?;
    let Some(_lock) = acquire_lock(dir)? else {
        return Ok(Exit::AlreadyRunning);
    };
    let inbox = match &opts.inbox {
        Some(path) => path.clone(),
        None => crate::inbox::store::path()?,
    };
    let state = match &opts.state {
        Some(path) => path.clone(),
        None => crate::state::State::path()?,
    };
    let listener = Listener::bind(dir)?;
    log(&format!(
        "listening on {} (pid {}, version {} build {}{})",
        endpoint::socket_path(dir).display(),
        std::process::id(),
        opts.version.semver,
        opts.version.build,
        if opts.keep_alive { ", keep-alive" } else { "" },
    ));

    let shared = Arc::new(Shared::new(opts.version.clone(), inbox, state));
    let _ = shared.runner.set(match &opts.verbs {
        Some(runner) => runner.clone(),
        None => verbs::Runner::new()?,
    });
    // Under the start lock, so a predecessor's verbs are orphans by now:
    // they hold this daemon until they exit.
    if let Some(runner) = shared.runner.get() {
        shared.verbs.adopt(&runner.spool, log);
    }
    let registry = if opts.host {
        let shared = Arc::clone(&shared);
        let store = super::forward_store::path()?;
        Some(Registry::spawn(log, store, Box::new(move |event| shared.publish_forwards(event))))
    } else {
        None
    };
    if let Some(registry) = &registry {
        let _ = shared.forwards.set(registry.handle());
    }
    let host = opts.host.then(|| {
        let shared = Arc::clone(&shared);
        // Configured `forwardPorts` follow the running containers.
        let sync = registry.as_ref().map(Registry::handle);
        Host::start(
            log,
            Box::new(move |instance: &str, line: String| shared.publish_shown(instance, line)),
            Box::new(move |running: &[String]| {
                if let Some(sync) = &sync {
                    sync.sync(running.to_vec());
                }
            }),
        )
    });
    if let Some(host) = &host {
        let _ = shared.bridges.set(host.waker());
    }
    let watcher = Watcher::spawn(&shared, host.as_ref().map(Host::changes));
    let mut next_conn = 0u64;
    let exit = loop {
        if let Some(exit) = *shared.stop.lock().unwrap() {
            break exit;
        }
        if let Some(runner) = shared.runner.get() {
            shared.verbs.check_adopted(&runner.spool, log);
        }
        let now = Instant::now();
        let holders = shared.holders(host.as_ref(), registry.as_ref());
        if holders.any() {
            *shared.idle_since.lock().unwrap() = now;
        }
        let idle_since = *shared.idle_since.lock().unwrap();
        if idle::decide(&holders, idle_since, now, opts.keep_alive, opts.idle_timeout) == Decision::Exit {
            break Exit::Idle;
        }
        match listener.accept_timeout(TICK) {
            Ok(Some(stream)) => {
                next_conn += 1;
                shared.spawn_conn(next_conn, stream);
            }
            Ok(None) => {}
            Err(e) => {
                // EMFILE and friends: don't spin, the next tick retries.
                log(&format!("accept failed: {e}"));
                std::thread::sleep(TICK);
            }
        }
    };

    // Stop accepting before anything else, then release the handoff and
    // shutdown replies: once a client reads one, nothing answers on the
    // socket, so the successor it (or the service manager) starts waits for
    // this lock instead of seeing a live daemon and giving up.
    listener.remove();
    shared.close();
    shared.publish_closing(match exit {
        Exit::Handoff => "handoff",
        Exit::Shutdown => "shutdown",
        _ => "idle",
    });
    shared.drain(DRAIN_TIMEOUT);
    drop(watcher);
    // Before the lock goes: a successor's bridges must not overlap ours (two
    // sink bridges on one container would split its notify/control streams).
    drop(host);
    // Likewise the forwards: their listeners close here, so a successor can
    // bind the configured ones' saved host ports again. Ad-hoc ones are
    // closed too; the next daemon restores them from `forwards.toml`.
    drop(registry);
    Ok(exit)
}

/// Take the start lock, or `None` when a live daemon (one answering on the
/// socket) holds it. A holder that doesn't answer is starting up or draining
/// after a handoff; wait for it, up to [`LOCK_WAIT`].
fn acquire_lock(dir: &Path) -> Result<Option<File>> {
    let path = endpoint::lock_path(dir);
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e).with_context(|| format!("cannot lock {}", path.display())),
        }
        if endpoint::connect(dir).is_ok() {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            bail!(
                "{} is locked but nothing answers on {}",
                path.display(),
                endpoint::socket_path(dir).display()
            );
        }
        std::thread::sleep(LOCK_POLL);
    }
}

struct Shared {
    version: Version,
    /// The Inbox store the API serves.
    inbox: PathBuf,
    /// The `state.toml` whose changes are `instances.changed`.
    state: PathBuf,
    /// Live connections. Its size is the `clients` holder count.
    conns: Mutex<HashMap<u64, ConnEntry>>,
    /// Inbox changes this daemon has seen: the `generation` of
    /// `inbox.changed`.
    inbox_generation: AtomicU64,
    /// The last moment a holder was seen.
    idle_since: Mutex<Instant>,
    /// A client asked the daemon to exit: [`Exit::Handoff`] or
    /// [`Exit::Shutdown`]. The first request wins.
    stop: Mutex<Option<Exit>>,
    /// The listener is gone; handoff and shutdown replies may go out.
    closed: (Mutex<bool>, Condvar),
    /// The host side's bridges, for `bridges.ensure`; unset without one.
    bridges: OnceLock<Waker>,
    /// The forward registry, for `forwards.*`; unset without a host side.
    forwards: OnceLock<forwards::Handle>,
    /// How verbs run; unset only in tests that build a bare `Shared`.
    runner: OnceLock<verbs::Runner>,
    /// The verbs not reaped yet, a predecessor's adopted ones included: the
    /// `verbs` holder count.
    verbs: Arc<verbs::InFlight>,
    /// Set when the drain starts: its read-side shutdown is not a client
    /// hanging up, so a verb's connection must not cancel on it.
    draining: AtomicBool,
}

impl Shared {
    fn new(version: Version, inbox: PathBuf, state: PathBuf) -> Self {
        Self {
            version,
            inbox,
            state,
            conns: Mutex::new(HashMap::new()),
            inbox_generation: AtomicU64::new(0),
            idle_since: Mutex::new(Instant::now()),
            stop: Mutex::new(None),
            closed: (Mutex::new(false), Condvar::new()),
            bridges: OnceLock::new(),
            forwards: OnceLock::new(),
            runner: OnceLock::new(),
            verbs: Arc::default(),
            draining: AtomicBool::new(false),
        }
    }

    fn holders(&self, host: Option<&Host>, registry: Option<&Registry>) -> Holders {
        let clients = self.conns.lock().unwrap().len();
        let forwards = registry.map_or(0, Registry::active);
        let verbs = self.verbs.len();
        let Some(host) = host else { return Holders { clients, forwards, verbs, ..Default::default() } };
        Holders {
            clients,
            forwards,
            verbs,
            live_instances: host.live_instances(),
            control: host.control(),
            autostart: host.autostarting(),
            ..Default::default()
        }
    }

    /// Register before the thread starts, so the next holder snapshot counts
    /// it even if the client is gone by then.
    fn spawn_conn(self: &Arc<Self>, id: u64, stream: Stream) {
        let (Ok(clone), Ok(writer), Ok(ctl)) = (stream.try_clone(), stream.try_clone(), stream.try_clone()) else {
            return;
        };
        // A socket option: covers every clone, responses and notifications.
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let out = Arc::new(Outbox::new(writer, ctl));
        self.conns.lock().unwrap().insert(id, ConnEntry { stream: clone, out: Arc::clone(&out) });
        let shared = Arc::clone(self);
        let spawned = std::thread::Builder::new().name(format!("serve-conn-{id}")).spawn(move || {
            serve_conn(id, stream, &shared, out);
            shared.conns.lock().unwrap().remove(&id);
            *shared.idle_since.lock().unwrap() = Instant::now();
        });
        if spawned.is_err() {
            self.conns.lock().unwrap().remove(&id);
        }
    }

    fn close(&self) {
        let (lock, cvar) = &self.closed;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    /// Ask the accept loop to exit with `exit` (a handoff or shutdown) and
    /// wait (bounded) until it has closed the listener; see the comment at
    /// the end of [`run`].
    fn begin_stop(&self, exit: Exit) {
        self.stop.lock().unwrap().get_or_insert(exit);
        let (lock, cvar) = &self.closed;
        let guard = lock.lock().unwrap();
        let _ = cvar.wait_timeout_while(guard, Duration::from_secs(2), |closed| !*closed);
    }

    /// Flag a notification on every connection subscribed to `topic`.
    fn publish(&self, topic: Topic, set: impl Fn(&mut Pending)) {
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                let subscribed = p.topics.contains(&topic);
                if subscribed {
                    set(p);
                }
                subscribed
            });
        }
    }

    /// `inbox.shown`: a container message's status line, to `inbox`
    /// subscribers. Latest wins: under backpressure older lines are dropped.
    fn publish_shown(&self, instance: &str, line: String) {
        self.publish(Topic::Inbox, |p| p.shown = Some((instance.to_string(), line.clone())));
    }

    /// The registry's events, to `forwards` subscribers: `forwards.changed`,
    /// and `forwards.status` lines, latest wins like `inbox.shown`.
    fn publish_forwards(&self, event: forwards::Event) {
        match event {
            forwards::Event::Changed => self.publish(Topic::Forwards, |p| p.forwards = true),
            forwards::Event::Status(line) => self.publish(Topic::Forwards, |p| p.forwards_status = Some(line.clone())),
        }
    }

    /// The reconnect hint: tell every subscriber the daemon is going away
    /// (`reason`: `handoff`, `shutdown` or `idle`), before the drain closes them.
    fn publish_closing(&self, reason: &'static str) {
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                let subscribed = !p.topics.is_empty();
                if subscribed {
                    p.closing = Some(reason);
                }
                subscribed
            });
        }
    }

    /// A verb's connection ends here too, leaving its verb running (the
    /// [`draining`](Self::draining) flag), so verbs never hold the drain.
    fn drain(&self, timeout: Duration) {
        self.draining.store(true, Ordering::SeqCst);
        for conn in self.conns.lock().unwrap().values() {
            let _ = conn.stream.shutdown(Shutdown::Read);
        }
        let deadline = Instant::now() + timeout;
        while !self.conns.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

struct ConnEntry {
    /// For the drain's shutdown.
    stream: Stream,
    out: Arc<Outbox>,
}

/// Everything written to one connection goes through here: the writer lock
/// serializes responses (from the connection thread) with notifications
/// (from its notifier thread), and `pending` holds the flagged
/// notifications the notifier hasn't written yet.
struct Outbox {
    writer: Mutex<Stream>,
    /// Unlocked clone, to cut the connection while a write holds `writer`.
    ctl: Stream,
    pending: Mutex<Pending>,
    wake: Condvar,
}

/// A connection's subscriptions and flagged notifications. Flags, not a
/// queue: notifications are coarse ("re-fetch"), so any number of changes
/// before the notifier runs is one line per topic.
#[derive(Default)]
struct Pending {
    topics: BTreeSet<Topic>,
    /// `inbox.changed` with this generation.
    inbox: Option<u64>,
    /// `inbox.shown` with `(instance, line)`, the latest only.
    shown: Option<(String, String)>,
    instances: bool,
    /// `forwards.changed`.
    forwards: bool,
    /// `forwards.status` with this line, the latest only.
    forwards_status: Option<String>,
    /// `closing` with this reason; written last.
    closing: Option<&'static str>,
    /// The connection ended: the notifier writes what's left and exits.
    closed: bool,
}

impl Pending {
    fn any(&self) -> bool {
        self.inbox.is_some()
            || self.shown.is_some()
            || self.instances
            || self.forwards
            || self.forwards_status.is_some()
            || self.closing.is_some()
    }

    fn take(&mut self) -> Vec<Notification> {
        let mut out = Vec::new();
        if let Some(generation) = self.inbox.take() {
            out.push(Notification { method: "inbox.changed".into(), params: json!({ "generation": generation }) });
        }
        if let Some((instance, line)) = self.shown.take() {
            out.push(Notification { method: "inbox.shown".into(), params: json!({ "instance": instance, "line": line }) });
        }
        if std::mem::take(&mut self.instances) {
            out.push(Notification { method: "instances.changed".into(), params: json!({}) });
        }
        if std::mem::take(&mut self.forwards) {
            out.push(Notification { method: "forwards.changed".into(), params: json!({}) });
        }
        if let Some(line) = self.forwards_status.take() {
            out.push(Notification { method: "forwards.status".into(), params: json!({ "line": line }) });
        }
        if let Some(reason) = self.closing.take() {
            out.push(Notification { method: "closing".into(), params: json!({ "reason": reason }) });
        }
        out
    }
}

impl Outbox {
    fn new(writer: Stream, ctl: Stream) -> Self {
        Self { writer: Mutex::new(writer), ctl, pending: Mutex::new(Pending::default()), wake: Condvar::new() }
    }

    /// One JSON line, whole, under the writer lock.
    fn send(&self, msg: &impl serde::Serialize) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(&line)?;
        writer.flush()
    }

    /// Change `pending` under its lock; `f` says whether to wake the
    /// notifier.
    fn post(&self, f: impl FnOnce(&mut Pending) -> bool) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if f(&mut pending) {
            self.wake.notify_all();
        }
    }
}

/// A connection's notifier thread: write what's flagged, until the
/// connection ends. A failed (or timed-out) write cuts the connection, which
/// ends its reader too.
fn notifier(out: &Outbox) {
    loop {
        let (notes, closed) = {
            let mut p = out.pending.lock().unwrap_or_else(|e| e.into_inner());
            while !p.any() && !p.closed {
                p = out.wake.wait(p).unwrap_or_else(|e| e.into_inner());
            }
            (p.take(), p.closed)
        };
        for note in &notes {
            if out.send(note).is_err() {
                let _ = out.ctl.shutdown(Shutdown::Both);
                return;
            }
        }
        if closed {
            return;
        }
    }
}

/// The daemon-wide change watcher (see the module doc). Dropping it stops
/// and joins the thread (within [`WATCH`]).
struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    fn spawn(shared: &Arc<Shared>, instances: Option<Arc<AtomicU64>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (shared, stop) = (Arc::clone(shared), Arc::clone(&stop));
            std::thread::Builder::new().name("serve-watch".into()).spawn(move || watch(&shared, instances, &stop))
        };
        let thread = thread.map_err(|e| log(&format!("cannot start the notification thread: {e}"))).ok();
        Self { stop, thread }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `instances.changed` covers both the host's container poll (`instances`)
/// and `state.toml` writes (phases, new/removed/renamed instances, done
/// flags), stamped like the store each tick, so a build's phase reaches a
/// subscriber within [`WATCH`].
fn watch(shared: &Shared, instances: Option<Arc<AtomicU64>>, stop: &AtomicBool) {
    let mut seen = ops::generation();
    let mut stamp = ops::stamp(&shared.inbox);
    let mut state_stamp = crate::inbox::store::stamp_at(&shared.state);
    let instances_now = || instances.as_ref().map_or(0, |c| c.load(Ordering::Acquire));
    let mut instances_seen = instances_now();
    while !stop.load(Ordering::SeqCst) {
        let generation = ops::wait_changed(seen, WATCH);
        let now = ops::stamp(&shared.inbox);
        if generation != seen || now != stamp {
            (seen, stamp) = (generation, now);
            let g = shared.inbox_generation.fetch_add(1, Ordering::SeqCst) + 1;
            shared.publish(Topic::Inbox, |p| p.inbox = Some(g));
        }
        let n = instances_now();
        let state_now = crate::inbox::store::stamp_at(&shared.state);
        if n != instances_seen || state_now != state_stamp {
            (instances_seen, state_stamp) = (n, state_now);
            shared.publish(Topic::Instances, |p| p.instances = true);
        }
    }
}

/// A connection's own state, on its thread.
struct ConnState {
    id: u64,
    /// The hello's `client` (`tui`, `cli`, `api:<name>`), for the log.
    client: String,
    /// What the store records as the client of this connection's user ops
    /// (`api::audit_client` of `client`).
    audit: String,
    out: Arc<Outbox>,
    notifier: Option<JoinHandle<()>>,
}

impl ConnState {
    fn label(&self) -> String {
        match self.client.as_str() {
            "" => format!("connection {}", self.id),
            client => format!("connection {} ({client})", self.id),
        }
    }
}

/// One connection: a request per line, a response per request, until EOF.
fn serve_conn(id: u64, stream: Stream, shared: &Shared, out: Arc<Outbox>) {
    let mut conn = ConnState { id, client: String::new(), audit: api::audit_client(""), out, notifier: None };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(proto::MAX_LINE).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let too_long = !line.ends_with(b"\n") && line.len() as u64 >= proto::MAX_LINE;
        let response = if too_long {
            Response::err(None, "invalid", "request line too long")
        } else if line.trim_ascii().is_empty() {
            continue;
        } else {
            match handle(&line, shared, &mut conn) {
                Some(response) => response,
                // A verb's client hung up (or the daemon drains): no answer.
                None => break,
            }
        };
        if conn.out.send(&response).is_err() || too_long {
            break;
        }
    }
    conn.out.post(|p| {
        p.closed = true;
        true
    });
    if let Some(t) = conn.notifier.take() {
        let _ = t.join();
    }
}

fn handle(line: &[u8], shared: &Shared, conn: &mut ConnState) -> Option<Response> {
    let req: Request = match serde_json::from_slice(line) {
        Ok(req) => req,
        Err(e) => return Some(Response::err(None, "invalid", format!("not a request: {e}"))),
    };
    let answer = match req.method.as_str() {
        "hello" => return Some(hello(req.id, req.params, shared, conn)),
        method if verbs::is_verb(method) => return verb(req.id, method, req.params, shared, conn),
        "subscribe" => subscribe(req.params, conn, true),
        "unsubscribe" => subscribe(req.params, conn, false),
        "shutdown" => {
            log(&format!("{}: shutdown requested", conn.label()));
            shared.begin_stop(Exit::Shutdown);
            Ok(json!({ "ok": true }))
        }
        method => {
            let bridges = shared.bridges.get().map(|w| w as &dyn api::Bridging);
            let forwards = shared.forwards.get().map(|h| h as &dyn api::Forwarding);
            api::call(method, req.params, &api::Ctx { inbox: &shared.inbox, client: &conn.audit, bridges, forwards })
        }
    };
    Some(match answer {
        Ok(result) => Response::ok(req.id, result),
        Err(e) => {
            if e.code == "internal" {
                log(&format!("{}: {}: {}", conn.label(), req.method, e.message));
            }
            Response::err(req.id, e.code, e.message)
        }
    })
}

/// A verb request (`serve::verbs`): held on this connection until the verb
/// ends (or a `detach` answer), streaming its output. `None`: the client
/// hung up, or the daemon drains; nothing is answered.
fn verb(id: Option<u64>, method: &str, params: Value, shared: &Shared, conn: &ConnState) -> Option<Response> {
    let Some(runner) = shared.runner.get() else {
        return Some(Response::err(id, "internal", "this daemon runs no verbs"));
    };
    let req = match verbs::parse(method, params) {
        Ok(req) => req,
        Err(e) => return Some(Response::err(id, e.code, e.message)),
    };
    let peer = ConnPeer { out: &conn.out, shared };
    match verbs::serve(&req, id, runner, &shared.verbs, &peer, &conn.label(), log) {
        verbs::Outcome::Answer(Ok(result)) => Some(Response::ok(id, result)),
        verbs::Outcome::Answer(Err(f)) => {
            if f.code == "internal" {
                log(&format!("{}: {method}: {}", conn.label(), f.message));
            }
            Some(Response::err_with(id, f.code, f.message, f.data))
        }
        verbs::Outcome::Gone => None,
    }
}

/// A verb's view of its connection.
struct ConnPeer<'a> {
    out: &'a Outbox,
    shared: &'a Shared,
}

impl verbs::Peer for ConnPeer<'_> {
    fn notify(&self, method: &str, params: Value) -> std::io::Result<()> {
        self.out.send(&Notification { method: method.into(), params })
    }

    fn hung_up(&self, tick: Duration) -> bool {
        hung_up(&self.out.ctl, tick)
    }

    fn draining(&self) -> bool {
        self.shared.draining.load(Ordering::SeqCst)
    }
}

/// Wait up to `tick` for `stream`'s peer to hang up, without reading:
/// readable and a peeked 0 bytes is EOF. Pending data (a request pipelined
/// behind the verb) is left for the reader, and waited out by sleeping
/// rather than polling again, which would return at once and spin.
fn hung_up(stream: &Stream, tick: Duration) -> bool {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let ms = libc::c_int::try_from(tick.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one valid pollfd, alive for the call.
    if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
        return false;
    }
    let mut byte = 0u8;
    // SAFETY: a one-byte buffer we own; MSG_PEEK leaves the data queued.
    let n = unsafe { libc::recv(fd, (&raw mut byte).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    match n {
        0 => true,
        1.. => {
            std::thread::sleep(tick);
            false
        }
        _ => {
            let e = std::io::Error::last_os_error();
            !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted)
        }
    }
}

/// `subscribe` (`on`) / `unsubscribe`. The first subscribe starts the
/// connection's notifier. Nothing is sent for the past: a client subscribes,
/// then fetches.
fn subscribe(params: Value, conn: &mut ConnState, on: bool) -> Result<Value, api::ApiError> {
    let topics = api::topics(params)?;
    if on && conn.notifier.is_none() {
        let out = Arc::clone(&conn.out);
        let spawned = std::thread::Builder::new().name(format!("serve-notify-{}", conn.id)).spawn(move || notifier(&out));
        conn.notifier = Some(spawned.map_err(|e| api::ApiError::internal(e.into()))?);
    }
    conn.out.post(|p| {
        for topic in topics {
            if on {
                p.topics.insert(topic);
            } else {
                p.topics.remove(&topic);
                match topic {
                    Topic::Inbox => {
                        p.inbox = None;
                        p.shown = None;
                    }
                    Topic::Instances => p.instances = false,
                    Topic::Forwards => {
                        p.forwards = false;
                        p.forwards_status = None;
                    }
                }
            }
        }
        false
    });
    Ok(json!({ "ok": true }))
}

fn hello(id: Option<u64>, params: Value, shared: &Shared, conn: &mut ConnState) -> Response {
    let params: HelloParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "invalid", format!("bad hello params: {e}")),
    };
    conn.client.clone_from(&params.client);
    conn.audit = api::audit_client(&params.client);
    let client = Version { semver: params.version, build: params.build };
    let handoff = client.is_newer_than(&shared.version);
    if handoff {
        log(&format!(
            "client `{}` has version {} build {}: handing off",
            params.client, client.semver, client.build
        ));
        shared.begin_stop(Exit::Handoff);
    }
    let result = HelloResult {
        version: shared.version.semver.clone(),
        build: shared.version.build,
        protocol: proto::PROTOCOL,
        handoff,
    };
    Response::ok(id, serde_json::to_value(result).unwrap_or(Value::Null))
}

/// One line to stderr, which the lazy start points at `serve.log`.
pub(super) fn log(msg: &str) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    eprintln!("[{secs}] devsandbox serve: {msg}");
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::serve::client::{self, Hello};

    pub(crate) fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    pub(crate) fn opts(build: u64, idle_ms: u64) -> Options {
        Options {
            version: Version { semver: "0.6.0".into(), build },
            keep_alive: false,
            idle_timeout: Duration::from_millis(idle_ms),
            host: false,
            inbox: Some(std::env::temp_dir().join(format!("dsv-inbox-{}", std::process::id())).join("inbox.json")),
            // Never this binary on the user's state: a verb sent to a test
            // daemon that doesn't set its own fails fast.
            verbs: Some(verbs::Runner {
                program: vec!["false".into()],
                spool: std::env::temp_dir().join(format!("dsv-verbs-{}", std::process::id())),
                state: std::env::temp_dir().join(format!("dsv-verbs-{}", std::process::id())).join("state.toml"),
            }),
            // Never the user's: the watcher only stamps it, but a test that
            // listens for `instances.changed` sets its own.
            state: Some(std::env::temp_dir().join(format!("dsv-state-{}", std::process::id())).join("state.toml")),
        }
    }

    pub(crate) fn spawn_daemon(dir: &Path, opts: Options) -> std::thread::JoinHandle<Result<Exit>> {
        let dir = dir.to_owned();
        std::thread::spawn(move || run(&dir, &opts))
    }

    fn v(build: u64) -> Version {
        Version { semver: "0.6.0".into(), build }
    }

    /// Connect (retrying while the daemon binds) and say hello.
    fn hello_at(dir: &Path, version: &Version) -> (Stream, Hello) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stream) = endpoint::connect(dir) {
                let mut stream = stream;
                let hello = client::hello(&mut stream, version, "test").unwrap();
                return (stream, hello);
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn concurrent_starts_have_one_winner_and_all_clients_reach_it() {
        let dir = scratch("race");
        let daemons: Vec<_> = (0..8).map(|_| spawn_daemon(&dir, opts(1, 300))).collect();
        let clients: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || hello_at(&dir, &v(1)).1)
            })
            .collect();
        for c in clients {
            match c.join().unwrap() {
                Hello::Ready(result) => assert_eq!(result.build, 1),
                other => panic!("{other:?}"),
            }
        }
        let mut exits: Vec<_> = daemons.into_iter().map(|d| d.join().unwrap().unwrap()).collect();
        exits.sort_by_key(|e| *e != Exit::Idle);
        assert_eq!(exits[0], Exit::Idle);
        assert!(exits[1..].iter().all(|e| *e == Exit::AlreadyRunning), "{exits:?}");
        assert!(!endpoint::socket_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_hello_hands_off_drains_and_exits() {
        let dir = scratch("handoff");
        let daemon = spawn_daemon(&dir, opts(1, 60_000));

        // Equal and older clients just proceed.
        let (mut idle_client, equal) = hello_at(&dir, &v(1));
        assert!(matches!(equal, Hello::Ready(_)), "{equal:?}");
        let (_older_conn, older) = hello_at(&dir, &Version { semver: "0.5.9".into(), build: 9 });
        assert!(matches!(older, Hello::Ready(_)), "{older:?}");

        let started = Instant::now();
        let (_newer_conn, newer) = hello_at(&dir, &v(2));
        assert!(matches!(newer, Hello::Handoff), "{newer:?}");
        // Nothing answers once the handoff reply is out.
        assert!(endpoint::connect(&dir).is_err());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        // Idle connections are closed, not waited out.
        assert!(started.elapsed() < DRAIN_TIMEOUT);
        let mut buf = [0u8; 1];
        assert_eq!(idle_client.read(&mut buf).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_methods_and_garbage_get_errors_with_ids() {
        let dir = scratch("unknown");
        let daemon = spawn_daemon(&dir, opts(1, 200));
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut ask = |line: &str| {
            writer.write_all(format!("{line}\n").as_bytes()).unwrap();
            let mut out = String::new();
            reader.read_line(&mut out).unwrap();
            serde_json::from_str::<Value>(&out).unwrap()
        };
        let r = ask(r#"{"id":7,"method":"inbox.poll.vote"}"#);
        assert_eq!(r["id"], 7);
        assert_eq!(r["error"]["code"], "unknown-method");
        let r = ask("not json");
        assert_eq!(r["error"]["code"], "invalid");
        let r = ask(r#"{"id":8,"method":"hello","params":{"nope":1}}"#);
        assert_eq!((r["id"].as_u64(), r["error"]["code"].as_str()), (Some(8), Some("invalid")));
        drop(ask);
        drop(writer);
        drop(reader);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn connect(dir: &Path) -> client::Conn {
        client::connect_with(dir, "test", &v(1), client::START_TIMEOUT, &|_, _| Ok(())).unwrap()
    }

    fn note(path: &Path, msg: &str) {
        use crate::devsbd::notify::{Level, Record};
        let record = Record { level: Level::Info, key: None, link: None, msg: msg.into(), at: 1 };
        crate::inbox::store::update_at(path, |i| i.push("w-id".into(), "w".into(), record, true)).unwrap();
    }

    /// Notifications from other tests' store writes (the in-process
    /// generation is process-wide) are harmless extras: drop what's queued.
    fn drain_notes(conn: &mut client::Conn, wait: Duration) {
        while conn.next_notification(wait).unwrap().is_some() {}
    }

    #[test]
    fn subscribers_hear_inbox_changes_between_responses() {
        let dir = scratch("notify");
        let store = scratch("notify-store").join("inbox.json");
        let daemon = spawn_daemon(&dir, Options { inbox: Some(store.clone()), ..opts(1, 200) });
        let mut conn = connect(&dir);
        assert_eq!(conn.daemon.protocol, proto::PROTOCOL);

        let r = conn.call("subscribe", json!({"topics": ["nope"]})).unwrap();
        assert_eq!(r.error.unwrap().code, "invalid");
        let r = conn.call("subscribe", json!({"topics": ["inbox"]})).unwrap();
        assert_eq!(r.result, Some(json!({"ok": true})));
        drain_notes(&mut conn, Duration::from_millis(200));

        note(&store, "one");
        let n = conn.next_notification(Duration::from_secs(3)).unwrap().expect("no inbox.changed");
        assert_eq!(n.method, "inbox.changed");
        assert!(n.params["generation"].as_u64().unwrap() >= 1, "{n:?}");

        // A notification sent before a response doesn't break the call: it's
        // set aside, and handed out without touching the socket.
        drain_notes(&mut conn, Duration::from_millis(100));
        note(&store, "two");
        std::thread::sleep(Duration::from_millis(300));
        let r = conn.call("inbox.threads.list", json!({"view": "all"})).unwrap();
        assert_eq!(r.result.unwrap().as_array().unwrap().len(), 2);
        let n = conn.next_notification(Duration::ZERO).unwrap().expect("not set aside by call");
        assert_eq!(n.method, "inbox.changed");

        // API errors carry the request's id.
        let r = conn.call("inbox.thread.get", json!({"thread": 999})).unwrap();
        assert_eq!(r.error.unwrap().code, "not-found");

        let r = conn.call("unsubscribe", json!({"topics": ["inbox"]})).unwrap();
        assert!(r.error.is_none());
        drain_notes(&mut conn, Duration::ZERO);
        note(&store, "three");
        assert!(conn.next_notification(Duration::from_millis(800)).unwrap().is_none());

        drop(conn);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    /// A `state.toml` write (a phase, a new instance, a rename) is an
    /// `instances.changed`, with no host side polling containers.
    #[test]
    fn subscribers_hear_state_changes() {
        let dir = scratch("state-watch");
        let state = scratch("state-watch-file").join("state.toml");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        let daemon = spawn_daemon(&dir, Options { state: Some(state.clone()), ..opts(1, 200) });
        let mut conn = connect(&dir);
        let r = conn.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        assert!(conn.next_notification(Duration::from_millis(700)).unwrap().is_none(), "nothing moved yet");

        std::fs::write(&state, "[instance.a]\n").unwrap();
        wait_for(&mut conn, "instances.changed");
        // A second write (longer: the size moves even within mtime's grain).
        std::fs::write(&state, "[instance.a]\n[instance.b]\n").unwrap();
        wait_for(&mut conn, "instances.changed");

        drop(conn);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(state.parent().unwrap());
    }

    /// The next notification named `method`, skipping others (stray
    /// `inbox.changed` from other tests' writes).
    fn wait_for(conn: &mut client::Conn, method: &str) -> Notification {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let n = conn.next_notification(left).unwrap().unwrap_or_else(|| panic!("no {method}"));
            if n.method == method {
                return n;
            }
        }
    }

    #[test]
    fn two_dashboards_hear_one_write() {
        let dir = scratch("two");
        let store = scratch("two-store").join("inbox.json");
        let daemon = spawn_daemon(&dir, Options { inbox: Some(store.clone()), ..opts(1, 200) });
        let (mut a, mut b) = (connect(&dir), connect(&dir));
        for conn in [&mut a, &mut b] {
            conn.call("subscribe", json!({"topics": ["inbox"]})).unwrap();
        }
        note(&store, "one");
        let id = ops::load(&store).unwrap().threads[0].id;
        drain_notes(&mut a, Duration::from_millis(700));
        drain_notes(&mut b, Duration::ZERO);
        // One write through the API, from `a`: both hear it.
        let r = a.call("inbox.thread.markRead", json!({ "thread": id })).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        wait_for(&mut a, "inbox.changed");
        wait_for(&mut b, "inbox.changed");
        drop((a, b));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    #[test]
    fn shown_lines_reach_inbox_subscribers_latest_first() {
        use std::os::unix::net::UnixStream;
        let shared = Arc::new(Shared::new(v(1), scratch("shown-store").join("inbox.json"), scratch("shown-store").join("state.toml")));
        let mut clients = Vec::new();
        for (id, topics) in [(1, json!(["inbox"])), (2, json!(["inbox"])), (3, json!(["instances"]))] {
            let (server, client) = UnixStream::pair().unwrap();
            shared.spawn_conn(id, server);
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let mut writer = client;
            writeln!(writer, "{}", json!({"id": 1, "method": "subscribe", "params": {"topics": topics}})).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["result"], json!({"ok": true}));
            reader.get_ref().set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            clients.push((reader, writer));
        }
        // Coalescing keeps the latest line when two land before a write;
        // either way the last one seen is the newest.
        shared.publish_shown("disp", "disp: first".into());
        shared.publish_shown("disp", "disp: second".into());
        for (reader, _) in &mut clients[..2] {
            let last = loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let n: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(n["method"], "inbox.shown", "{n}");
                assert_eq!(n["params"]["instance"], "disp");
                if n["params"]["line"] == "disp: second" {
                    break n;
                }
                assert_eq!(n["params"]["line"], "disp: first");
            };
            assert!(last.get("id").is_none());
        }
        // Not subscribed to `inbox`: nothing.
        let (reader, _) = &mut clients[2];
        reader.get_ref().set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut line = String::new();
        assert!(reader.read_line(&mut line).is_err() || line.is_empty(), "{line}");
    }

    /// A connection over a socket pair, subscribed to `topics`.
    fn subscribed(shared: &Arc<Shared>, id: u64, topics: Value) -> BufReader<std::os::unix::net::UnixStream> {
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        shared.spawn_conn(id, server);
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut writer = client;
        writeln!(writer, "{}", json!({"id": 1, "method": "subscribe", "params": {"topics": topics}})).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["result"], json!({"ok": true}));
        reader.get_ref().set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        // Dropping `writer` closes one fd; the reader's clone keeps the socket.
        reader
    }

    #[test]
    fn forwards_subscribers_hear_changes_and_status_lines() {
        let shared = Arc::new(Shared::new(v(1), scratch("fwd-store").join("inbox.json"), scratch("fwd-store").join("state.toml")));
        let mut fwd = subscribed(&shared, 1, json!(["forwards"]));
        let mut other = subscribed(&shared, 2, json!(["inbox", "instances"]));
        shared.publish_forwards(forwards::Event::Changed);
        let mut line = String::new();
        fwd.read_line(&mut line).unwrap();
        let n: Value = serde_json::from_str(&line).unwrap();
        assert_eq!((n["method"].as_str(), &n["params"]), (Some("forwards.changed"), &json!({})));
        shared.publish_forwards(forwards::Event::Status("3000: connection refused".into()));
        line.clear();
        fwd.read_line(&mut line).unwrap();
        let n: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(n["method"], "forwards.status");
        assert_eq!(n["params"]["line"], "3000: connection refused");
        // Not subscribed to `forwards`: nothing.
        other.get_ref().set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        line.clear();
        assert!(other.read_line(&mut line).is_err() || line.is_empty(), "{line}");
    }

    #[test]
    fn active_forwards_are_holders() {
        let shared = Shared::new(v(1), scratch("fwd-hold").join("inbox.json"), scratch("fwd-hold").join("state.toml"));
        let registry = Registry::spawn(|_| {}, scratch("fwd-hold-store").join("forwards.toml"), Box::new(|_| {}));
        assert_eq!(shared.holders(None, Some(&registry)), Holders::default());
        let dir = scratch("fwd-hold-root");
        std::fs::create_dir_all(&dir).unwrap();
        let req = forwards::AddRequest {
            dir: dir.clone(),
            instance: None,
            service: Some("db".into()),
            bind: std::net::Ipv4Addr::LOCALHOST.into(),
            host_port: crate::devsbd::forward::HostPort::Prefer(0),
            container_port: 5432,
        };
        let h = registry.handle();
        let added = api::Forwarding::add(&h, req).unwrap();
        // A request behind the post-command poll: the count is current.
        api::Forwarding::list(&h, None).unwrap();
        let holders = shared.holders(None, Some(&registry));
        assert_eq!(holders, Holders { forwards: 1, ..Default::default() });
        assert!(holders.any());
        api::Forwarding::rm(&h, added.id).unwrap();
        api::Forwarding::list(&h, None).unwrap();
        assert!(!shared.holders(None, Some(&registry)).any());
        drop(registry);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subscribers_get_a_closing_hint_on_handoff() {
        let dir = scratch("closing");
        let daemon = spawn_daemon(&dir, opts(1, 60_000));
        let mut conn = connect(&dir);
        conn.call("subscribe", json!({"topics": ["inbox", "instances"]})).unwrap();
        let (_newer, h) = hello_at(&dir, &v(2));
        assert!(matches!(h, Hello::Handoff));
        let closing = loop {
            let n = conn.next_notification(Duration::from_secs(3)).unwrap().expect("no closing hint");
            if n.method == "closing" {
                break n;
            }
        };
        assert_eq!(closing.params["reason"], "handoff");
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_answers_drains_and_exits() {
        let dir = scratch("shutdown");
        let daemon = spawn_daemon(&dir, Options { keep_alive: true, ..opts(1, 50) });
        let mut sub = connect(&dir);
        sub.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        let (mut idle_client, _) = hello_at(&dir, &v(1));

        let started = Instant::now();
        let mut conn = connect(&dir);
        let r = conn.call("shutdown", json!({})).unwrap();
        assert_eq!(r.result, Some(json!({"ok": true})), "{r:?}");
        // The answer goes out once nothing answers on the socket any more.
        assert!(endpoint::connect(&dir).is_err());
        let closing = wait_for(&mut sub, "closing");
        assert_eq!(closing.params, json!({"reason": "shutdown"}));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        // Idle connections are closed, not waited out.
        assert!(started.elapsed() < DRAIN_TIMEOUT);
        let mut buf = [0u8; 1];
        assert_eq!(idle_client.read(&mut buf).unwrap(), 0);
        assert!(!endpoint::socket_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every notification is an `ApiEvents` key in the npm typings, its
    /// params matching the interface it names (`serve::dts`).
    #[test]
    fn npm_typings_match_every_notification() {
        use crate::serve::dts;
        let mut p = Pending {
            inbox: Some(1),
            shown: Some(("disp".into(), "disp: hi".into())),
            instances: true,
            forwards: true,
            forwards_status: Some("forwarding".into()),
            closing: Some("idle"),
            ..Pending::default()
        };
        let notes = p.take();
        assert_eq!(notes.len(), 6, "a Pending flag isn't covered here");
        for n in notes {
            let ty = dts::prop_type("ApiEvents", &n.method)
                .unwrap_or_else(|| panic!("index.d.ts `ApiEvents` lacks `'{}'`", n.method));
            if ty == "{}" {
                assert_eq!(n.params, json!({}), "{}", n.method);
            } else {
                dts::assert_matches(&ty, &n.params);
            }
        }
    }

    /// A daemon whose verbs are one script: `$*` (the argv) picks the case.
    fn verb_daemon(tag: &str, idle_ms: u64, script: &str) -> (PathBuf, PathBuf, JoinHandle<Result<Exit>>) {
        let dir = scratch(tag);
        let (runner, root) = verbs::tests::script_runner(tag, script);
        let daemon = spawn_daemon(&dir, Options { verbs: Some(runner), ..opts(1, idle_ms) });
        (dir, root, daemon)
    }

    fn verb_line(id: u64, method: &str, root: &Path, extra: Value) -> String {
        let mut params = json!({"root": root, "env": {"PATH": std::env::var("PATH").unwrap_or_default()}});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        format!("{}\n", json!({"id": id, "method": method, "params": params}))
    }

    fn wait_file(path: &Path, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while !path.exists() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    const VERB_SCRIPT: &str = r#"case "$*" in
        *slow*) trap 'echo int > "$DIR/int"; exit 130' INT; echo ready; sleep 2; touch "$DIR/done" ;;
        *start*) sleep 1; touch "$DIR/started" ;;
        *) echo "hi $*"; echo 'Error: nope' >&2; exit 2 ;;
    esac"#;

    #[test]
    fn verbs_stream_over_the_socket_and_a_hang_up_cancels() {
        let (dir, root, daemon) = verb_daemon("verb-sock", 200, VERB_SCRIPT);
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        // A request pipelined behind the verb is neither a hang-up nor lost.
        let lines = verb_line(1, "stop", &root, json!({"instance": "fast"})) + r#"{"id":2,"method":"nope"}"# + "\n";
        writer.write_all(lines.as_bytes()).unwrap();
        let mut seen = Vec::new();
        let answer = loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id").is_some() {
                break msg;
            }
            seen.push(msg);
        };
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["error"], json!({"code": "failed", "message": "nope", "data": {"exit": 2, "instance": null}}));
        let out: String = seen
            .iter()
            .filter(|n| n["method"] == "output" && n["params"]["stream"] == "stdout")
            .map(|n| n["params"]["data"].as_str().unwrap())
            .collect();
        assert!(out.starts_with("hi --in-process -C "), "{out}");
        assert!(seen.iter().all(|n| n["params"]["request"] == 1), "{seen:?}");
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["error"]["code"], "unknown-method");

        // Hang up mid-verb: SIGINT to its group.
        writer.write_all(verb_line(3, "stop", &root, json!({"instance": "slow"})).as_bytes()).unwrap();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.contains("ready") {
                break;
            }
        }
        drop((reader, writer));
        assert!(wait_file(&root.join("int"), Duration::from_secs(3)), "no SIGINT");
        assert!(!root.join("done").exists());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_detached_verb_holds_the_daemon_until_reaped() {
        let (dir, root, daemon) = verb_daemon("verb-hold", 150, VERB_SCRIPT);
        let mut conn = connect(&dir);
        let env = json!({"PATH": std::env::var("PATH").unwrap_or_default()});
        let r = conn.call("start", json!({"root": root, "env": env, "all": true, "detach": true})).unwrap();
        assert_eq!(r.result, Some(json!({"instance": null, "detached": true})), "{r:?}");
        drop(conn);
        std::thread::sleep(Duration::from_millis(600));
        assert!(!daemon.is_finished(), "idled out with a verb in flight");
        assert!(!root.join("started").exists());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        assert!(root.join("started").exists(), "exited before the verb did");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_drain_leaves_a_verb_running() {
        let (dir, root, daemon) = verb_daemon("verb-drain", 60_000, VERB_SCRIPT);
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        writer.write_all(verb_line(1, "stop", &root, json!({"instance": "slow"})).as_bytes()).unwrap();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.contains("ready") {
                break;
            }
        }
        let started = Instant::now();
        let r = connect(&dir).call("shutdown", json!({})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        // The drain didn't wait the verb out, and didn't cancel it.
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
        let mut rest = String::new();
        let _ = reader.read_to_string(&mut rest);
        assert!(!rest.contains("\"id\":1"), "answered: {rest}");
        assert!(wait_file(&root.join("done"), Duration::from_secs(4)), "the verb didn't finish");
        assert!(!root.join("int").exists(), "the drain cancelled it");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_predecessors_live_verb_is_adopted_and_holds_until_it_exits() {
        let tag = "verb-adopt";
        let dir = scratch(tag);
        let (runner, root) = verbs::tests::script_runner(tag, "exit 0");
        let spool = runner.spool.clone();
        std::fs::create_dir_all(&spool).unwrap();
        let mut live = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let mut dead = std::process::Command::new("true").spawn().unwrap();
        dead.wait().unwrap();
        let files = |pid: u32| [spool.join(format!("{pid}.out")), spool.join(format!("{pid}.err"))];
        for path in files(live.id()).iter().chain(&files(dead.id())) {
            std::fs::write(path, "x").unwrap();
        }
        let daemon = spawn_daemon(&dir, Options { verbs: Some(runner), ..opts(1, 150) });
        // Seen through the socket: the sweep ran before the listener answers.
        let _ = hello_at(&dir, &v(1));
        assert!(files(dead.id()).iter().all(|p| !p.exists()), "a dead verb's files were kept");
        std::thread::sleep(Duration::from_millis(500));
        assert!(!daemon.is_finished(), "idled out with an adopted verb running");
        assert!(files(live.id()).iter().all(|p| p.exists()));
        // Reaped here (a zombie is still alive to `kill(0)`).
        live.kill().unwrap();
        live.wait().unwrap();
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        assert!(files(live.id()).iter().all(|p| !p.exists()), "an adopted verb's files were kept");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_connected_client_holds_and_keep_alive_never_idles() {
        let dir = scratch("hold");
        let daemon = spawn_daemon(&dir, opts(1, 150));
        let (conn, _) = hello_at(&dir, &v(1));
        std::thread::sleep(Duration::from_millis(500));
        assert!(!daemon.is_finished(), "exited while a client was connected");
        drop(conn);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);

        let keep = Options { keep_alive: true, ..opts(1, 50) };
        let daemon = spawn_daemon(&dir, keep);
        let _ = hello_at(&dir, &v(1));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!daemon.is_finished(), "keep-alive daemon idled out");
        // Stop it with a handoff (`shutdown` is the other way, tested above).
        let (_c, h) = hello_at(&dir, &v(2));
        assert!(matches!(h, Hello::Handoff));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
