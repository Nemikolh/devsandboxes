//! The dashboard's link to the host daemon (docs/serve.md, docs/api.md): one
//! worker thread that owns the [`Conn`], so the UI thread never waits on the
//! socket or on a lazy start (up to 5 s).
//!
//! It connects lazily, subscribes to `inbox` and `instances`, and forwards
//! what the daemon says as [`DaemonUpdate`]s the event loop drains. Holding
//! the connection makes the dashboard a holder, so the worker reconnects
//! after a `closing`, an EOF or an error, on the [`Backoff`] schedule; after
//! a version handoff it first waits for the successor without starting one
//! (see [`open`]).
//!
//! Inbox writes: the event loop hands the worker its `Op`s while
//! [`DaemonWorker::connected`]; each goes out as its API method
//! ([`op_call`]). Without a daemon the loop applies them itself through
//! `inbox::ops::apply`, and so does the worker for ops it gets after losing
//! the connection: the API runs the same ops code, so the owner's events are
//! the same either way. Reads stay local (the store file is the truth): an
//! `inbox.changed` only makes the loop reload at once.
//!
//! Terminals: when the dashboard opens one on a relay-mode instance, the
//! loop has the worker `bridges.ensure` it with the dashboard's own
//! `SSH_AUTH_SOCK` ([`Cmd::EnsureBridge`]), so the daemon's bridge relays
//! that agent at once. Disconnected, it's skipped: no local bridge exists to
//! fall back to, and the daemon bridges running instances on its own poll.
//!
//! Ports tab: the daemon owns every forward (`serve::forwards`), so they
//! outlive the dashboard. The worker subscribes to `forwards` too and, on
//! `forwards.changed` and after every (re)connect, fetches `forwards.list`
//! for the dashboard's config root ([`DaemonUpdate::Ports`]); `forwards.status`
//! lines are status lines. Adds and stops go out as `forwards.add` /
//! `forwards.rm` ([`Cmd::ForwardAdd`], [`Cmd::ForwardRm`]). Disconnected, the
//! tab has no rows and an add says the daemon is needed.
//!
//! A command channel plus a short-joining `Drop`; nothing is ever written to
//! stderr, failures are [`DaemonUpdate::Status`] lines.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::app::{PortRequest, PortRow};
use crate::inbox::{self, Op};
use crate::serve::client::{self, Conn};
use crate::serve::forwards::{Added, ForwardRow};
use crate::serve::endpoint;
use crate::serve::proto::Version;

/// How long one wait for a notification blocks while connected: the
/// latency of an inbox write queued meanwhile, and of the worker noticing
/// it was dropped.
const POLL: Duration = Duration::from_millis(100);
/// How long [`Drop`] waits for the worker before detaching it (it may be
/// mid-connect): the terminal restore mustn't hang on the daemon.
const JOIN_TIMEOUT: Duration = Duration::from_millis(300);

/// From the worker to the event loop.
#[derive(Debug, PartialEq)]
pub enum DaemonUpdate {
    /// The store changed (`inbox.changed`, or ops the worker applied): reload.
    InboxChanged,
    /// The running containers changed (`instances.changed`): snapshot early.
    InstancesChanged,
    /// A container message's status line (`inbox.shown`).
    Shown(String),
    /// The Ports tab's rows: this config root's forwards, or none while
    /// disconnected.
    Ports(Vec<PortRow>),
    /// A one-line status: the daemon is unavailable, a write failed.
    Status(String),
}

/// From the event loop to the worker.
#[derive(Debug, PartialEq)]
enum Cmd {
    /// Inbox writes, in order (see the module doc).
    Inbox(Vec<Op>),
    /// `bridges.ensure` for `instance` (a state key), reporting `agent`.
    EnsureBridge { instance: String, agent: Option<PathBuf> },
    /// `forwards.add` in the dashboard's config root.
    ForwardAdd(PortRequest),
    /// `forwards.rm`.
    ForwardRm(u64),
}

/// Handle to the daemon worker. Never blocks the UI thread; dropping it
/// closes the channel and joins briefly (see [`JOIN_TIMEOUT`]).
pub struct DaemonWorker {
    tx: Option<Sender<Cmd>>,
    updates: Receiver<DaemonUpdate>,
    connected: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DaemonWorker {
    /// Spawn the worker for the dashboard of config root `dir` (whose
    /// forwards the Ports tab lists).
    pub fn spawn(dir: PathBuf) -> DaemonWorker {
        let (tx, rx) = mpsc::channel();
        let (utx, updates) = mpsc::channel();
        let connected = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&connected);
        let handle = std::thread::Builder::new()
            .name("tui-daemon".into())
            .spawn(move || run(&root_dir(dir), rx, utx, &flag))
            .ok();
        DaemonWorker { tx: Some(tx), updates, connected, handle }
    }

    /// Whether inbox writes should go through [`apply`](Self::apply). A
    /// connection lost after this said yes is the worker's to handle.
    pub fn connected(&self) -> bool {
        self.handle.is_some() && self.connected.load(Ordering::Acquire)
    }

    /// Send inbox ops to the daemon, in order. Never blocks.
    pub fn apply(&self, ops: Vec<Op>) {
        self.send(Cmd::Inbox(ops));
    }

    /// Ask the daemon for `instance`'s bridge, reporting `agent` (the
    /// dashboard's `SSH_AUTH_SOCK`). Never blocks; dropped while disconnected.
    pub fn ensure_bridge(&self, instance: String, agent: Option<PathBuf>) {
        self.send(Cmd::EnsureBridge { instance, agent });
    }

    /// Ask the daemon to start a forward. Never blocks; the outcome is a
    /// status line.
    pub fn forward_add(&self, req: PortRequest) {
        self.send(Cmd::ForwardAdd(req));
    }

    /// Ask the daemon to stop forward `id`. Never blocks.
    pub fn forward_rm(&self, id: u64) {
        self.send(Cmd::ForwardRm(id));
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// Next pending update, or `None`. Non-blocking.
    pub fn try_recv(&self) -> Option<DaemonUpdate> {
        self.updates.try_recv().ok()
    }
}

impl Drop for DaemonWorker {
    fn drop(&mut self) {
        // Closing the channel ends the worker within `POLL` unless it's
        // mid-connect; then it's left to die with the process, which closes
        // the socket.
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let deadline = Instant::now() + JOIN_TIMEOUT;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

/// Reconnect delays: right away after a connection is lost (the first
/// retry), then 1 s, 2 s, 4 s … capped at [`Backoff::CAP`] while connects
/// keep failing. A success resets it.
#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
}

impl Backoff {
    const CAP: Duration = Duration::from_secs(30);

    /// The delay before the next attempt, counting one more failure.
    fn next(&mut self) -> Duration {
        let delay = match self.failures {
            0 => Duration::ZERO,
            n => Duration::from_secs(1u64 << (n - 1).min(5)).min(Self::CAP),
        };
        self.failures = self.failures.saturating_add(1);
        delay
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
}

/// The API call for a dashboard [`Op`], by store id. `RemoveThread` is
/// only ever a notification (the dashboard dismisses nothing else).
fn op_call(op: &Op) -> (&'static str, Value) {
    match op {
        Op::MarkRead(id) => ("inbox.thread.markRead", json!({ "thread": id })),
        Op::MarkNotifyRead => ("inbox.notify.markRead", json!({ "all": true })),
        Op::RemoveThread(id) => ("inbox.notify.dismiss", json!({ "thread": id })),
        Op::ClearNotify => ("inbox.notify.dismiss", json!({ "all": true })),
        Op::Act { thread, action } => ("inbox.thread.act", json!({ "thread": thread, "action": action })),
        Op::Reply { thread, text } => ("inbox.thread.reply", json!({ "thread": thread, "text": text })),
        Op::MarkDone(id) => ("inbox.thread.done", json!({ "thread": id })),
        Op::Reopen(id) => ("inbox.thread.reopen", json!({ "thread": id })),
    }
}

/// The API call for [`Cmd::EnsureBridge`]. A non-UTF-8 agent path can't go
/// on the wire; the daemon then uses the agents it knows.
fn ensure_call(instance: &str, agent: Option<&std::path::Path>) -> (&'static str, Value) {
    ("bridges.ensure", json!({ "instance": instance, "agent": agent.and_then(|p| p.to_str()) }))
}

/// The config root as the daemon keys it: canonical (what `run` records),
/// else at least absolute (the API refuses relative dirs).
fn root_dir(dir: PathBuf) -> PathBuf {
    dir.canonicalize().or_else(|_| std::path::absolute(&dir)).unwrap_or(dir)
}

/// The API call for `forwards.list` of config root `dir`.
fn list_call(dir: &Path) -> (&'static str, Value) {
    ("forwards.list", json!({ "dir": dir.to_string_lossy() }))
}

/// The API call for [`Cmd::ForwardAdd`]. An empty instance (a typed
/// `port --service …` without one) is a service-only forward.
fn add_call(dir: &Path, req: &PortRequest) -> (&'static str, Value) {
    let params = json!({
        "dir": dir.to_string_lossy(),
        "instance": (!req.instance.is_empty()).then_some(&req.instance),
        "service": req.service,
        "address": req.address,
        "spec": req.spec,
    });
    ("forwards.add", params)
}

/// The API call for [`Cmd::ForwardRm`].
fn rm_call(id: u64) -> (&'static str, Value) {
    ("forwards.rm", json!({ "id": id }))
}

/// `forwards.list`'s rows as the Ports tab's.
fn port_rows(result: Value) -> Result<Vec<PortRow>> {
    let rows: Vec<ForwardRow> = serde_json::from_value(result)?;
    Ok(rows
        .into_iter()
        .map(|r| PortRow {
            id: r.id,
            local: r.local,
            target: r.target,
            process: r.process,
            state: r.state,
            conns: r.conns,
            configured: r.configured,
        })
        .collect())
}

/// The status line for a `forwards.add` answer.
fn added_status(result: Value) -> String {
    match serde_json::from_value::<Added>(result) {
        Ok(a) => format!("forwarding {} -> {}", a.local, a.target),
        Err(_) => "forwarding".into(),
    }
}

/// The status line for a `forwards.rm` answer.
fn removed_status(result: &Value) -> String {
    let local = result.get("local").and_then(Value::as_str).unwrap_or("forward");
    match result.get("configured").and_then(Value::as_bool) {
        Some(true) => format!("stopped {local} (forwardPorts: back when its instance restarts)"),
        _ => format!("stopped {local}"),
    }
}

/// Whether an API error on `op` is worth a status line. A `not-found` on a
/// mark-read or dismiss is a thread another dashboard (or `thread rm`)
/// removed first: the local path ignores it too.
fn reports(op: &Op, code: &str) -> bool {
    !(code == "not-found" && !op.enqueues_event())
}

/// Connect (lazy start) and subscribe. After a handoff, wait for the
/// successor the newer client is starting rather than race it with a daemon
/// of our own (which, older, would win the lock and stall that client);
/// start one only if none shows up.
fn open(after_handoff: bool) -> Result<Conn> {
    let dir = endpoint::socket_dir()?;
    let waited = after_handoff
        .then(|| client::connect_with(&dir, "tui", &Version::current(), client::START_TIMEOUT, &|_, _| Ok(())).ok())
        .flatten();
    let mut conn = match waited {
        Some(conn) => conn,
        None => client::connect(&dir, "tui")?,
    };
    let r = conn.call("subscribe", json!({ "topics": ["inbox", "instances", "forwards"] }))?;
    if let Some(e) = r.error {
        anyhow::bail!("subscribe: {} ({})", e.message, e.code);
    }
    Ok(conn)
}

struct Worker<'a> {
    /// The dashboard's config root, as [`root_dir`] made it.
    dir: &'a Path,
    utx: Sender<DaemonUpdate>,
    connected: &'a AtomicBool,
    conn: Option<Conn>,
    backoff: Backoff,
    next_attempt: Instant,
    after_handoff: bool,
    /// Last connect error shown, so a daemon that stays down is one line.
    last_error: Option<String>,
    /// Why the last connect failed, for a forward asked for meanwhile.
    down: Option<String>,
}

fn run(dir: &Path, rx: Receiver<Cmd>, utx: Sender<DaemonUpdate>, connected: &AtomicBool) {
    let mut w = Worker {
        dir,
        utx,
        connected,
        conn: None,
        backoff: Backoff::default(),
        next_attempt: Instant::now(),
        after_handoff: false,
        last_error: None,
        down: None,
    };
    loop {
        if w.conn.is_none() && Instant::now() >= w.next_attempt {
            w.connect();
        }
        let wait = match w.conn {
            Some(_) => Duration::ZERO,
            None => w.next_attempt.saturating_duration_since(Instant::now()).min(POLL),
        };
        match rx.recv_timeout(wait) {
            Ok(Cmd::Inbox(ops)) => w.apply(ops),
            Ok(Cmd::EnsureBridge { instance, agent }) => w.ensure_bridge(&instance, agent.as_deref()),
            Ok(Cmd::ForwardAdd(req)) => w.forward_add(&req),
            Ok(Cmd::ForwardRm(id)) => w.forward_rm(id),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        w.listen();
    }
}

impl Worker<'_> {
    fn send(&self, update: DaemonUpdate) {
        let _ = self.utx.send(update);
    }

    fn connect(&mut self) {
        match open(std::mem::take(&mut self.after_handoff)) {
            Ok(conn) => {
                self.conn = Some(conn);
                self.connected.store(true, Ordering::Release);
                self.backoff.reset();
                self.last_error = None;
                self.down = None;
                // Changes while disconnected only reached us by the stamp.
                self.send(DaemonUpdate::InboxChanged);
                // A new daemon may hold other forwards than the last one.
                self.fetch_ports();
            }
            Err(e) => {
                self.down = Some(format!("{e:#}"));
                let msg = format!("host daemon unavailable: {e:#}");
                if self.last_error.as_ref() != Some(&msg) {
                    self.send(DaemonUpdate::Status(msg.clone()));
                    self.last_error = Some(msg);
                }
                // Failure 0's zero delay is for a lost connection; a failed
                // connect waits at least the next step.
                if self.backoff.failures == 0 {
                    self.backoff.next();
                }
                self.next_attempt = Instant::now() + self.backoff.next();
            }
        }
    }

    /// The connection is gone: schedule the reconnect.
    fn lost(&mut self, after_handoff: bool) {
        self.conn = None;
        self.connected.store(false, Ordering::Release);
        // No live rows without the daemon that holds them.
        self.send(DaemonUpdate::Ports(Vec::new()));
        self.after_handoff = after_handoff;
        self.next_attempt = Instant::now() + self.backoff.next();
    }

    /// Wait up to [`POLL`] for one notification and pass it on.
    fn listen(&mut self) {
        let Some(conn) = self.conn.as_mut() else { return };
        let note = match conn.next_notification(POLL) {
            Ok(Some(note)) => note,
            Ok(None) => return,
            Err(_) => return self.lost(false),
        };
        match note.method.as_str() {
            "inbox.changed" => self.send(DaemonUpdate::InboxChanged),
            "instances.changed" => self.send(DaemonUpdate::InstancesChanged),
            "forwards.changed" => self.fetch_ports(),
            "forwards.status" => {
                if let Some(line) = note.params.get("line").and_then(Value::as_str) {
                    self.send(DaemonUpdate::Status(line.to_string()));
                }
            }
            "inbox.shown" => {
                if let Some(line) = note.params.get("line").and_then(Value::as_str) {
                    self.send(DaemonUpdate::Shown(line.to_string()));
                }
            }
            "closing" => {
                let handoff = note.params.get("reason").and_then(Value::as_str) == Some("handoff");
                self.lost(handoff);
            }
            // Newer notifications this build doesn't know.
            _ => {}
        }
    }

    /// `bridges.ensure`, while connected. Quiet: an error answer (an instance
    /// removed meanwhile, an older daemon without the method) only means the
    /// daemon's own poll bridges it.
    fn ensure_bridge(&mut self, instance: &str, agent: Option<&std::path::Path>) {
        let Some(conn) = self.conn.as_mut() else { return };
        let (method, params) = ensure_call(instance, agent);
        if conn.call(method, params).is_err() {
            self.lost(false);
        }
    }

    /// `forwards.list` for this root, into the Ports tab.
    fn fetch_ports(&mut self) {
        let Some(conn) = self.conn.as_mut() else { return };
        let (method, params) = list_call(self.dir);
        match conn.call(method, params) {
            Ok(r) => match (r.result, r.error) {
                (_, Some(e)) => self.send(DaemonUpdate::Status(format!("ports: {}", e.message))),
                (Some(result), None) => match port_rows(result) {
                    Ok(rows) => self.send(DaemonUpdate::Ports(rows)),
                    Err(e) => self.send(DaemonUpdate::Status(format!("ports: {e:#}"))),
                },
                (None, None) => {}
            },
            Err(_) => self.lost(false),
        }
    }

    /// `forwards.add`; the answer (or why there's none) is a status line. The
    /// row arrives with the `forwards.changed` that follows.
    fn forward_add(&mut self, req: &PortRequest) {
        let Some(conn) = self.conn.as_mut() else {
            let why = self.down.as_deref().unwrap_or("not connected");
            return self.send(DaemonUpdate::Status(format!("port forwarding needs the host daemon: {why}")));
        };
        let (method, params) = add_call(self.dir, req);
        match conn.call(method, params) {
            Ok(r) => self.send(DaemonUpdate::Status(match (r.result, r.error) {
                (_, Some(e)) => e.message,
                (result, None) => added_status(result.unwrap_or_default()),
            })),
            Err(e) => {
                self.lost(false);
                self.send(DaemonUpdate::Status(format!("port forwarding needs the host daemon: {e:#}")));
            }
        }
    }

    /// `forwards.rm`. Disconnected there are no rows to stop. A `not-found`
    /// is a forward another client stopped first: quiet.
    fn forward_rm(&mut self, id: u64) {
        let Some(conn) = self.conn.as_mut() else { return };
        let (method, params) = rm_call(id);
        match conn.call(method, params) {
            Ok(r) => match (r.result, r.error) {
                (_, Some(e)) if e.code == "not-found" => {}
                (_, Some(e)) => self.send(DaemonUpdate::Status(e.message)),
                (result, None) => self.send(DaemonUpdate::Status(removed_status(&result.unwrap_or_default()))),
            },
            Err(_) => self.lost(false),
        }
    }

    /// One batch of ops, in order: through the daemon while connected, the
    /// rest straight to the store. An op whose call broke the connection may
    /// or may not have landed: an idempotent one is applied locally, an
    /// event-enqueuing one is reported, never sent twice.
    fn apply(&mut self, ops: Vec<Op>) {
        let mut local = Vec::new();
        for op in ops {
            let Some(conn) = self.conn.as_mut() else {
                local.push(op);
                continue;
            };
            let (method, params) = op_call(&op);
            match conn.call(method, params) {
                Ok(r) => {
                    if let Some(e) = r.error.filter(|e| reports(&op, &e.code)) {
                        self.send(DaemonUpdate::Status(format!("inbox not saved: {}", e.message)));
                    }
                }
                Err(e) => {
                    self.lost(false);
                    if op.enqueues_event() {
                        self.send(DaemonUpdate::Status(format!("inbox: may not be saved, check the thread: {e:#}")));
                    } else {
                        local.push(op);
                    }
                }
            }
        }
        if !local.is_empty() {
            if let Err(e) = inbox::store::path().and_then(|p| inbox::ops::apply(&p, &local, "tui")) {
                self.send(DaemonUpdate::Status(format!("inbox not saved: {e:#}")));
            }
        }
        self.send(DaemonUpdate::InboxChanged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_map_to_their_api_methods() {
        let cases = [
            (Op::MarkRead(3), "inbox.thread.markRead", json!({"thread": 3})),
            (Op::MarkNotifyRead, "inbox.notify.markRead", json!({"all": true})),
            (Op::RemoveThread(4), "inbox.notify.dismiss", json!({"thread": 4})),
            (Op::ClearNotify, "inbox.notify.dismiss", json!({"all": true})),
            (Op::Act { thread: 5, action: "go".into() }, "inbox.thread.act", json!({"thread": 5, "action": "go"})),
            (Op::Reply { thread: 6, text: "hi".into() }, "inbox.thread.reply", json!({"thread": 6, "text": "hi"})),
            (Op::MarkDone(7), "inbox.thread.done", json!({"thread": 7})),
            (Op::Reopen(8), "inbox.thread.reopen", json!({"thread": 8})),
        ];
        for (op, method, params) in cases {
            assert_eq!(op_call(&op), (method, params), "{op:?}");
        }
    }

    /// Every method `op_call` names exists, and each one, run against a
    /// store, does what `ops::apply` of the op does.
    #[test]
    fn mapped_calls_match_the_local_path() {
        use crate::devsbd::notify::{Level, Record};
        use crate::inbox::{Action, Compose, State, ThreadPut, store};
        use crate::serve::api::{self, Ctx};

        let seed = |path: &std::path::Path| {
            let put = ThreadPut {
                key: "k".into(),
                title: "t".into(),
                state: State::NeedsYou,
                compose: Some(Compose::default()),
                actions: vec![Action { id: "go".into(), label: "Go".into(), ..Action::default() }],
                ..ThreadPut::default()
            };
            store::update_at(path, |i| i.put("d-id", "d", 1, put)).unwrap();
            let record = Record { level: Level::Info, key: None, link: None, msg: "n".into(), at: 1 };
            store::update_at(path, |i| i.push("w-id".into(), "w".into(), record, true)).unwrap();
        };
        let dir = std::env::temp_dir().join(format!("devsandbox-tui-daemon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (via_api, via_ops) = (dir.join("api.json"), dir.join("ops.json"));
        seed(&via_api);
        seed(&via_ops);
        let inbox = inbox::ops::load(&via_api).unwrap();
        let thread = inbox.threads.iter().find(|t| t.kind == inbox::Kind::Thread).unwrap().id;
        let note = inbox.threads.iter().find(|t| t.kind == inbox::Kind::Notify).unwrap().id;
        let ops = [
            Op::MarkRead(thread),
            Op::Act { thread, action: "go".into() },
            Op::Reply { thread, text: "hi".into() },
            Op::MarkDone(thread),
            Op::Reopen(thread),
            Op::MarkNotifyRead,
            Op::RemoveThread(note),
            Op::ClearNotify,
        ];
        for op in &ops {
            let (method, params) = op_call(op);
            api::call(method, params, &Ctx { inbox: &via_api, client: "tui", bridges: None, forwards: None }).unwrap_or_else(|e| panic!("{op:?}: {e:?}"));
            inbox::ops::apply(&via_ops, std::slice::from_ref(op), "tui").unwrap();
        }
        // Ids and times aside (minted at apply time): threads, then events.
        let strip = |path: &std::path::Path| -> String {
            let i = inbox::ops::load(path).unwrap();
            let events = inbox::ops::events(path, "d-id").unwrap();
            let t: Vec<_> = i.threads.iter().map(|t| (t.id, t.kind, t.state, t.unread, t.feed.iter().map(|f| f.kind.clone()).collect::<Vec<_>>())).collect();
            let e: Vec<_> = events.into_iter().map(|(_, e)| (e.kind.as_str().to_string(), e.action, e.text)).collect();
            format!("{t:?} {e:?}")
        };
        assert_eq!(strip(&via_api), strip(&via_ops));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands_map_to_their_api_methods() {
        assert_eq!(
            ensure_call("web", Some(std::path::Path::new("/tmp/ssh-x/agent.1"))),
            ("bridges.ensure", json!({"instance": "web", "agent": "/tmp/ssh-x/agent.1"}))
        );
        assert_eq!(ensure_call("web", None), ("bridges.ensure", json!({"instance": "web", "agent": null})));
        // The handle queues each request as its command, in order.
        let (tx, rx) = mpsc::channel();
        let (_utx, updates) = mpsc::channel();
        let worker = DaemonWorker { tx: Some(tx), updates, connected: Arc::default(), handle: None };
        worker.apply(vec![Op::MarkRead(1)]);
        worker.ensure_bridge("web".into(), Some("/a".into()));
        assert_eq!(rx.try_recv().unwrap(), Cmd::Inbox(vec![Op::MarkRead(1)]));
        assert_eq!(rx.try_recv().unwrap(), Cmd::EnsureBridge { instance: "web".into(), agent: Some("/a".into()) });
    }

    /// `ensure_call`'s method and params are what the API handler takes.
    #[test]
    fn ensure_call_is_accepted_by_the_api() {
        use crate::serve::api::{self, Bridging, Ctx};
        struct Knows;
        impl Bridging for Knows {
            fn container(&self, _: &str) -> anyhow::Result<Option<String>> {
                Ok(Some("devsandbox-web".into()))
            }
            fn report_agent(&self, _: PathBuf) {}
            fn reconcile_now(&self, _: String) -> u64 {
                1
            }
            fn wait_ready(&self, _: &str, _: u64, _: std::time::Duration) -> crate::devsbd::bridge::Readiness {
                unreachable!("the dashboard never waits")
            }
        }
        let path = std::env::temp_dir().join(format!("devsandbox-tui-ensure-{}", std::process::id())).join("inbox.json");
        let (method, params) = ensure_call("web", Some(std::path::Path::new("/a")));
        assert_eq!(api::call(method, params, &Ctx { inbox: &path, client: "tui", bridges: Some(&Knows), forwards: None }), Ok(json!({"ok": true})));
    }

    fn port_req(instance: &str, service: Option<&str>) -> PortRequest {
        PortRequest { instance: instance.into(), service: service.map(String::from), address: None, spec: "8080:3000".into() }
    }

    #[test]
    fn forward_commands_queue_and_map_to_their_api_methods() {
        let (tx, rx) = mpsc::channel();
        let (_utx, updates) = mpsc::channel();
        let worker = DaemonWorker { tx: Some(tx), updates, connected: Arc::default(), handle: None };
        worker.forward_add(port_req("web", None));
        worker.forward_rm(4);
        assert_eq!(rx.try_recv().unwrap(), Cmd::ForwardAdd(port_req("web", None)));
        assert_eq!(rx.try_recv().unwrap(), Cmd::ForwardRm(4));

        let dir = Path::new("/cfg");
        assert_eq!(list_call(dir), ("forwards.list", json!({"dir": "/cfg"})));
        assert_eq!(rm_call(4), ("forwards.rm", json!({"id": 4})));
        assert_eq!(
            add_call(dir, &port_req("web", None)),
            (
                "forwards.add",
                json!({"dir": "/cfg", "instance": "web", "service": null, "address": null, "spec": "8080:3000"})
            )
        );
        // No instance typed: a service-only forward.
        assert_eq!(add_call(dir, &port_req("", Some("db"))).1["instance"], Value::Null);
        assert!(root_dir(PathBuf::from("rel/dir")).is_absolute());
    }

    /// The calls are what the API takes, and its answers become rows and
    /// status lines.
    #[test]
    fn forward_calls_are_accepted_by_the_api_and_answers_render() {
        use crate::serve::api::{self, ApiError, Ctx, Forwarding};
        use crate::serve::forwards::{AddRequest, Removed};
        struct Fake;
        impl Forwarding for Fake {
            fn list(&self, dir: Option<PathBuf>) -> Result<Vec<ForwardRow>, ApiError> {
                assert_eq!(dir.as_deref(), Some(Path::new("/cfg")));
                Ok(vec![ForwardRow {
                    id: 3,
                    dir: "/cfg".into(),
                    local: "127.0.0.1:8080".into(),
                    target: "web:3000".into(),
                    process: Some("node (pid 1)".into()),
                    state: "active".into(),
                    conns: 2,
                    configured: true,
                }])
            }
            fn add(&self, req: AddRequest) -> Result<Added, ApiError> {
                assert_eq!(req.instance.as_deref(), Some("web"));
                Ok(Added { id: 3, local: "127.0.0.1:8080".into(), target: "web:3000".into() })
            }
            fn rm(&self, _: u64) -> Result<Removed, ApiError> {
                Ok(Removed { local: "127.0.0.1:8080".into(), configured: true })
            }
        }
        let path = std::env::temp_dir().join(format!("devsandbox-tui-fwd-{}", std::process::id())).join("inbox.json");
        let ctx = Ctx { inbox: &path, client: "tui", bridges: None, forwards: Some(&Fake) };
        let dir = Path::new("/cfg");

        let (method, params) = list_call(dir);
        let rows = port_rows(api::call(method, params, &ctx).unwrap()).unwrap();
        assert_eq!(
            rows,
            [PortRow {
                id: 3,
                local: "127.0.0.1:8080".into(),
                target: "web:3000".into(),
                process: Some("node (pid 1)".into()),
                state: "active".into(),
                conns: 2,
                configured: true,
            }]
        );
        let (method, params) = add_call(dir, &port_req("web", None));
        assert_eq!(added_status(api::call(method, params, &ctx).unwrap()), "forwarding 127.0.0.1:8080 -> web:3000");
        let (method, params) = rm_call(3);
        assert_eq!(
            removed_status(&api::call(method, params, &ctx).unwrap()),
            "stopped 127.0.0.1:8080 (forwardPorts: back when its instance restarts)"
        );
        assert_eq!(removed_status(&json!({"ok": true, "local": "127.0.0.1:1", "configured": false})), "stopped 127.0.0.1:1");
    }

    #[test]
    fn stale_targets_are_quiet_but_lost_events_are_not() {
        assert!(!reports(&Op::MarkRead(1), "not-found"));
        assert!(!reports(&Op::RemoveThread(1), "not-found"));
        assert!(!reports(&Op::MarkNotifyRead, "not-found"));
        assert!(reports(&Op::MarkDone(1), "not-found"));
        assert!(reports(&Op::Act { thread: 1, action: "a".into() }, "not-found"));
        assert!(reports(&Op::MarkRead(1), "internal"));
        assert!(reports(&Op::Reply { thread: 1, text: "x".into() }, "denied"));
    }

    #[test]
    fn backoff_retries_at_once_then_doubles_to_the_cap() {
        let mut b = Backoff::default();
        let secs: Vec<u64> = (0..9).map(|_| b.next().as_secs()).collect();
        assert_eq!(secs, [0, 1, 2, 4, 8, 16, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next(), Duration::ZERO);
        assert_eq!(b.next(), Duration::from_secs(1));
        // Never overflows, however long the daemon stays down.
        let mut b = Backoff { failures: u32::MAX - 1 };
        assert_eq!((b.next(), b.next()), (Backoff::CAP, Backoff::CAP));
    }
}
