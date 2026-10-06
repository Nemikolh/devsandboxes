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
//! Modelled on `forwards.rs`'s `ForwardWorker`; nothing is ever written to
//! stderr, failures are [`DaemonUpdate::Status`] lines.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use crate::inbox::{self, Op};
use crate::serve::client::{self, Conn};
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
    /// A one-line status: the daemon is unavailable, a write failed.
    Status(String),
}

/// Handle to the daemon worker. Never blocks the UI thread; dropping it
/// closes the channel and joins briefly (see [`JOIN_TIMEOUT`]).
pub struct DaemonWorker {
    tx: Option<Sender<Vec<Op>>>,
    updates: Receiver<DaemonUpdate>,
    connected: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DaemonWorker {
    pub fn spawn() -> DaemonWorker {
        let (tx, rx) = mpsc::channel();
        let (utx, updates) = mpsc::channel();
        let connected = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&connected);
        let handle = std::thread::Builder::new()
            .name("tui-daemon".into())
            .spawn(move || run(rx, utx, &flag))
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
        if let Some(tx) = &self.tx {
            let _ = tx.send(ops);
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
        .then(|| client::connect_with(&dir, "tui", &Version::current(), client::START_TIMEOUT, &|_| Ok(())).ok())
        .flatten();
    let mut conn = match waited {
        Some(conn) => conn,
        None => client::connect(&dir, "tui")?,
    };
    let r = conn.call("subscribe", json!({ "topics": ["inbox", "instances"] }))?;
    if let Some(e) = r.error {
        anyhow::bail!("subscribe: {} ({})", e.message, e.code);
    }
    Ok(conn)
}

struct Worker<'a> {
    utx: Sender<DaemonUpdate>,
    connected: &'a AtomicBool,
    conn: Option<Conn>,
    backoff: Backoff,
    next_attempt: Instant,
    after_handoff: bool,
    /// Last connect error shown, so a daemon that stays down is one line.
    last_error: Option<String>,
}

fn run(rx: Receiver<Vec<Op>>, utx: Sender<DaemonUpdate>, connected: &AtomicBool) {
    let mut w = Worker {
        utx,
        connected,
        conn: None,
        backoff: Backoff::default(),
        next_attempt: Instant::now(),
        after_handoff: false,
        last_error: None,
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
            Ok(ops) => w.apply(ops),
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
                // Changes while disconnected only reached us by the stamp.
                self.send(DaemonUpdate::InboxChanged);
            }
            Err(e) => {
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
            if let Err(e) = inbox::store::path().and_then(|p| inbox::ops::apply(&p, &local)) {
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
        use crate::inbox::{Action, Reply, State, ThreadPut, store};
        use crate::serve::api::{self, Ctx};

        let seed = |path: &std::path::Path| {
            let put = ThreadPut {
                key: "k".into(),
                title: "t".into(),
                state: State::NeedsYou,
                reply: Some(Reply::default()),
                actions: vec![Action { id: "go".into(), label: "Go".into(), ..Action::default() }],
                ..ThreadPut::default()
            };
            store::update_at(path, |i| i.put("d-id", "d", 1, put)).unwrap();
            let record = Record { level: Level::Info, key: None, link: None, msg: "n".into(), at: 1 };
            store::update_at(path, |i| i.push("w-id".into(), "w".into(), record, true)).unwrap();
        };
        let dir = std::env::temp_dir().join(format!("devsandbox-tui-daemon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (via_api, via_ops) = (dir.join("api.toml"), dir.join("ops.toml"));
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
            api::call(method, params, &Ctx { inbox: &via_api }).unwrap_or_else(|e| panic!("{op:?}: {e:?}"));
            inbox::ops::apply(&via_ops, std::slice::from_ref(op)).unwrap();
        }
        // Ids and times aside (minted at apply time): threads, then events.
        let strip = |path: &std::path::Path| -> String {
            let i = inbox::ops::load(path).unwrap();
            let events = inbox::ops::events(path, "d-id").unwrap();
            let t: Vec<_> = i.threads.iter().map(|t| (t.id, t.kind, t.state, t.unread, t.entries.len())).collect();
            let e: Vec<_> = events.into_iter().map(|(_, e)| (e.kind.as_str().to_string(), e.action, e.text)).collect();
            format!("{t:?} {e:?}")
        };
        assert_eq!(strip(&via_api), strip(&via_ops));
        let _ = std::fs::remove_dir_all(&dir);
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
