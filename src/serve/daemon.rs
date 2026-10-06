//! The daemon side of `devsandbox serve`: start lock, accept loop, version
//! handoff and idle exit (docs/serve.md).
//!
//! Threading: the calling thread runs the accept loop, waking every [`TICK`]
//! to re-check the idle countdown and the handoff flag; each connection gets
//! its own thread reading JSON lines and answering them (the API methods are
//! in `api.rs`). Exiting (idle or handoff) closes the listener and removes
//! the socket first, sends subscribers a `closing` notification, then shuts
//! down the read side of every connection, so idle ones end at once while a
//! request in flight still writes its response, and waits up to
//! [`DRAIN_TIMEOUT`] for them. Then the live host side ([`Host`]: bridges,
//! autostart) stops, killing every bridge. The start lock is released last,
//! so a successor never binds (or bridges) while this one drains.
//!
//! Notifications (docs/api.md): one daemon-wide `serve-watch` thread notices
//! changes (store writes by this process at once, by others within [`WATCH`]
//! via the file stamp; the host's container poll) and only *flags* them on
//! each subscribed connection's [`Outbox`]; it never does I/O, so a slow
//! client can't hold up the others. A connection's first `subscribe` starts
//! its `serve-notify-<n>` thread, which writes the flagged notifications.
//! Flags coalesce (one pending `inbox.changed` however many writes), so the
//! queue is bounded by construction; every write to a connection, response
//! or notification, goes through its one writer lock, and a client that
//! doesn't drain its socket for [`WRITE_TIMEOUT`] is disconnected.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::api::{self, Topic};
use super::endpoint::{self, Listener, Stream};
use super::host::Host;
use super::idle::{self, Decision, Holders};
use super::proto::{self, HelloParams, HelloResult, Notification, Request, Response, Version};
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
    /// No idle exit. `serve.keep-alive = true` in a global config will set
    /// this too once one exists (there is none yet: config is per `-C` root,
    /// the daemon per user); `serve install` (step 10) implies it.
    pub keep_alive: bool,
    /// [`idle::IDLE_TIMEOUT`] outside tests.
    pub idle_timeout: Duration,
    /// Run the live host side ([`Host`]: bridges, popups, autostart). Off in
    /// tests, which must not touch the user's state or containers.
    pub host: bool,
    /// The Inbox store the API serves; `None` is the default
    /// (`inbox::store::path`). Tests point it at their own file.
    pub inbox: Option<PathBuf>,
}

impl Options {
    pub fn new(keep_alive: bool) -> Self {
        Self { version: Version::current(), keep_alive, idle_timeout: idle::IDLE_TIMEOUT, host: true, inbox: None }
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
}

/// `devsandbox serve`: run the daemon in the foreground until it idles out or
/// hands off. `socket_dir` overrides [`endpoint::socket_dir`] (the lazy start
/// passes the dir its client resolved, so both agree).
pub fn serve(socket_dir: Option<PathBuf>, keep_alive: bool) -> Result<()> {
    let dir = match socket_dir {
        Some(dir) => dir,
        None => endpoint::socket_dir()?,
    };
    let opts = Options::new(keep_alive);
    match run(&dir, &opts)? {
        Exit::AlreadyRunning => {}
        Exit::Idle => log("idle, exiting"),
        Exit::Handoff => log("handed off to a newer version, exiting"),
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
    let listener = Listener::bind(dir)?;
    log(&format!(
        "listening on {} (pid {}, version {} build {}{})",
        endpoint::socket_path(dir).display(),
        std::process::id(),
        opts.version.semver,
        opts.version.build,
        if opts.keep_alive { ", keep-alive" } else { "" },
    ));

    let host = opts.host.then(|| Host::start(log));
    let shared = Arc::new(Shared::new(opts.version.clone(), inbox));
    let watcher = Watcher::spawn(&shared, host.as_ref().map(Host::changes));
    let mut next_conn = 0u64;
    let exit = loop {
        if shared.handoff.load(Ordering::SeqCst) {
            break Exit::Handoff;
        }
        let now = Instant::now();
        let holders = shared.holders(host.as_ref());
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

    // Stop accepting before anything else, then release the handoff replies:
    // once a newer client reads `handoff`, nothing answers on the socket, so
    // the successor it spawns waits for this lock instead of seeing a live
    // daemon and giving up.
    listener.remove();
    shared.close();
    shared.publish_closing(match exit {
        Exit::Handoff => "handoff",
        _ => "idle",
    });
    shared.drain(DRAIN_TIMEOUT);
    drop(watcher);
    // Before the lock goes: a successor's bridges must not overlap ours (two
    // sink bridges on one container would split its notify/control streams).
    drop(host);
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
    /// Live connections. Its size is the `clients` holder count.
    conns: Mutex<HashMap<u64, ConnEntry>>,
    /// Inbox changes this daemon has seen: the `generation` of
    /// `inbox.changed`.
    inbox_generation: AtomicU64,
    /// The last moment a holder was seen.
    idle_since: Mutex<Instant>,
    /// A newer client asked for a handoff.
    handoff: AtomicBool,
    /// The listener is gone; handoff replies may go out.
    closed: (Mutex<bool>, Condvar),
}

impl Shared {
    fn new(version: Version, inbox: PathBuf) -> Self {
        Self {
            version,
            inbox,
            conns: Mutex::new(HashMap::new()),
            inbox_generation: AtomicU64::new(0),
            idle_since: Mutex::new(Instant::now()),
            handoff: AtomicBool::new(false),
            closed: (Mutex::new(false), Condvar::new()),
        }
    }

    fn holders(&self, host: Option<&Host>) -> Holders {
        let clients = self.conns.lock().unwrap().len();
        let Some(host) = host else { return Holders { clients, ..Default::default() } };
        Holders {
            clients,
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

    /// Flag the handoff and wait (bounded) until the accept loop has closed
    /// the listener; see the comment at the end of [`run`].
    fn begin_handoff(&self) {
        self.handoff.store(true, Ordering::SeqCst);
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

    /// The reconnect hint: tell every subscriber the daemon is going away
    /// (`reason`: `handoff` or `idle`), before the drain closes them.
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

    fn drain(&self, timeout: Duration) {
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
    instances: bool,
    /// `closing` with this reason; written last.
    closing: Option<&'static str>,
    /// The connection ended: the notifier writes what's left and exits.
    closed: bool,
}

impl Pending {
    fn any(&self) -> bool {
        self.inbox.is_some() || self.instances || self.closing.is_some()
    }

    fn take(&mut self) -> Vec<Notification> {
        let mut out = Vec::new();
        if let Some(generation) = self.inbox.take() {
            out.push(Notification { method: "inbox.changed".into(), params: json!({ "generation": generation }) });
        }
        if std::mem::take(&mut self.instances) {
            out.push(Notification { method: "instances.changed".into(), params: json!({}) });
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

fn watch(shared: &Shared, instances: Option<Arc<AtomicU64>>, stop: &AtomicBool) {
    let mut seen = ops::generation();
    let mut stamp = ops::stamp(&shared.inbox);
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
        if n != instances_seen {
            instances_seen = n;
            shared.publish(Topic::Instances, |p| p.instances = true);
        }
    }
}

/// A connection's own state, on its thread.
struct ConnState {
    id: u64,
    /// The hello's `client` (`tui`, `cli`, `api:<name>`), for the log.
    client: String,
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
    let mut conn = ConnState { id, client: String::new(), out, notifier: None };
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
            handle(&line, shared, &mut conn)
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

fn handle(line: &[u8], shared: &Shared, conn: &mut ConnState) -> Response {
    let req: Request = match serde_json::from_slice(line) {
        Ok(req) => req,
        Err(e) => return Response::err(None, "invalid", format!("not a request: {e}")),
    };
    let answer = match req.method.as_str() {
        "hello" => return hello(req.id, req.params, shared, conn),
        "subscribe" => subscribe(req.params, conn, true),
        "unsubscribe" => subscribe(req.params, conn, false),
        method => api::call(method, req.params, &api::Ctx { inbox: &shared.inbox }),
    };
    match answer {
        Ok(result) => Response::ok(req.id, result),
        Err(e) => {
            if e.code == "internal" {
                log(&format!("{}: {}: {}", conn.label(), req.method, e.message));
            }
            Response::err(req.id, e.code, e.message)
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
                    Topic::Inbox => p.inbox = None,
                    Topic::Instances => p.instances = false,
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
    let client = Version { semver: params.version, build: params.build };
    let handoff = client.is_newer_than(&shared.version);
    if handoff {
        log(&format!(
            "client `{}` has version {} build {}: handing off",
            params.client, client.semver, client.build
        ));
        shared.begin_handoff();
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
            inbox: Some(std::env::temp_dir().join(format!("dsv-inbox-{}", std::process::id())).join("inbox.toml")),
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
        let r = ask(r#"{"id":7,"method":"inbox.form.submit"}"#);
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
        client::connect_with(dir, "test", &v(1), client::START_TIMEOUT, &|_| Ok(())).unwrap()
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
        let store = scratch("notify-store").join("inbox.toml");
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
        // Stop it the only way there is: a handoff.
        let (_c, h) = hello_at(&dir, &v(2));
        assert!(matches!(h, Hello::Handoff));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
