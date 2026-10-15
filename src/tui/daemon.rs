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
//! Ports tab: the daemon owns every forward (`serve::forwards`), so they
//! outlive the dashboard. The worker subscribes to `forwards` too and, on
//! `forwards.changed` and after every (re)connect, fetches `forwards.list`
//! for the dashboard's config root ([`DaemonUpdate::Ports`]); `forwards.status`
//! lines are status lines. Adds and stops go out as `forwards.add` /
//! `forwards.rm` ([`Cmd::ForwardAdd`], [`Cmd::ForwardRm`]). Disconnected, the
//! tab has no rows and an add says the daemon is needed.
//!
//! Builds: `d` on an in-flight row is `instances.cancel` ([`Cmd::Cancel`]),
//! whoever started the build; the answer is a status line. The verbs
//! themselves don't go through here: each holds its own connection until
//! it's answered (`tui::spawn_verb`).
//!
//! One-shot reads and writes: the logs modal (`containers.logs`), the config
//! view (`sandboxes.get`/`services.get`, `containers.inspect`), the build
//! log (`instances.buildLog`), process signals (`instances.signal`),
//! `:rename` (`instances.rename`) and done/undone (`instances.done`) are
//! quick, so they share this connection ([`Cmd::Fetch`], [`Cmd::Signal`],
//! [`Cmd::Rename`], [`Cmd::Done`]). Each is answered by one
//! [`DaemonUpdate`] naming its target; disconnected, that answer says the
//! dashboard needs devsandbox serve ([`needs_serve`]), as `cancel`'s does.
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

use super::app::{ConfigKind, Fetch, PendingDone, PendingSignal, PortRequest, PortRow};
use crate::build_log::Tail;
use crate::config::SandboxText;
use crate::inbox::{self, Op};
use crate::serve::client::{self, Conn};
use crate::serve::forwards::{Added, ForwardRow};
use crate::serve::endpoint;
use crate::serve::proto::{Response, Version};

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
    /// [`Fetch::Logs`]'s answer: the modal body.
    Logs { container: String, body: String },
    /// [`Fetch::Config`]'s answer (`Err`: the message both bodies show).
    Config { kind: ConfigKind, name: String, text: Result<SandboxText, String> },
    /// [`Fetch::Inspect`]'s answer: the inspect pane's body.
    Inspect { container: String, body: String },
    /// [`Cmd::BuildLog`]'s answer, for `App::set_build_log`.
    BuildLog { instance: String, read: Result<Option<Tail>, String> },
    /// A process signal's status line; the process rows are stale.
    Signalled(String),
    /// A rename's status line; the snapshot is stale.
    Renamed(String),
    /// A done-flag write's status line, `failed` or not, and the request's
    /// `report` (`PendingDone`).
    Done { status: String, failed: bool, report: bool },
}

/// From the event loop to the worker.
#[derive(Debug, PartialEq)]
enum Cmd {
    /// Inbox writes, in order (see the module doc).
    Inbox(Vec<Op>),
    /// `forwards.add` in the dashboard's config root.
    ForwardAdd(PortRequest),
    /// `forwards.rm`.
    ForwardRm(u64),
    /// `instances.cancel` for `instance` (a state key).
    Cancel(String),
    /// A modal's read.
    Fetch(Fetch),
    /// `instances.buildLog` for an instance id.
    BuildLog(String),
    /// `instances.signal`.
    Signal(PendingSignal),
    /// `instances.rename`.
    Rename { instance: String, new_name: String },
    /// `instances.done`.
    Done(PendingDone),
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

    /// Ask the daemon to start a forward. Never blocks; the outcome is a
    /// status line.
    pub fn forward_add(&self, req: PortRequest) {
        self.send(Cmd::ForwardAdd(req));
    }

    /// Ask the daemon to stop forward `id`. Never blocks.
    pub fn forward_rm(&self, id: u64) {
        self.send(Cmd::ForwardRm(id));
    }

    /// Ask the daemon to cancel `instance`'s build. Never blocks; the
    /// outcome is a status line.
    pub fn cancel(&self, instance: String) {
        self.send(Cmd::Cancel(instance));
    }

    /// Ask for a modal's read. Never blocks; answered by its update.
    pub fn fetch(&self, fetch: Fetch) {
        self.send(Cmd::Fetch(fetch));
    }

    /// Ask for instance `id`'s build log. Never blocks; answered by
    /// [`DaemonUpdate::BuildLog`].
    pub fn build_log(&self, id: String) {
        self.send(Cmd::BuildLog(id));
    }

    /// Send a process signal. Never blocks; answered by
    /// [`DaemonUpdate::Signalled`].
    pub fn signal(&self, sig: PendingSignal) {
        self.send(Cmd::Signal(sig));
    }

    /// Rename an instance. Never blocks; answered by
    /// [`DaemonUpdate::Renamed`].
    pub fn rename(&self, instance: String, new_name: String) {
        self.send(Cmd::Rename { instance, new_name });
    }

    /// Set or clear a done flag. Never blocks; answered by
    /// [`DaemonUpdate::Done`].
    pub fn set_done(&self, req: PendingDone) {
        self.send(Cmd::Done(req));
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
        Op::SaveDraft { thread, message, answers } => (
            "inbox.form.saveDraft",
            json!({ "thread": thread, "message": message, "answers": inbox::form::partial_json(answers) }),
        ),
        Op::Submit { thread, message, answers } => (
            "inbox.form.submit",
            json!({ "thread": thread, "message": message, "answers": inbox::form::partial_json(answers) }),
        ),
    }
}

/// The config root as the daemon keys it: canonical (what `run` records),
/// else at least absolute (the API refuses relative dirs).
pub(super) fn root_dir(dir: PathBuf) -> PathBuf {
    dir.canonicalize().or_else(|_| std::path::absolute(&dir)).unwrap_or(dir)
}

/// The API call for [`Cmd::Cancel`].
fn cancel_call(instance: &str) -> (&'static str, Value) {
    ("instances.cancel", json!({ "instance": instance }))
}

/// The status line for an `instances.cancel` answer: the pid it SIGINTed,
/// or why not (not building any more, no such instance).
fn cancel_status(instance: &str, r: Response) -> String {
    match (r.result, r.error) {
        (_, Some(e)) => format!("cancel {instance}: {}", e.message),
        (result, None) => match result.as_ref().and_then(|v| v.get("pid")).and_then(Value::as_u64) {
            Some(pid) => format!("cancelling {instance} (SIGINT to pid {pid})"),
            None => format!("cancelling {instance}"),
        },
    }
}

/// What a one-shot call came back with.
#[derive(Debug, PartialEq)]
enum Answer {
    /// The result (`null` when the response had none).
    Ok(Value),
    /// The daemon's error message.
    Refused(String),
    /// The daemon couldn't be asked: the whole line ([`needs_serve`]).
    Unreachable(String),
}

impl Answer {
    fn from_response(r: Response) -> Answer {
        match (r.result, r.error) {
            (_, Some(e)) => Answer::Refused(e.message),
            (result, None) => Answer::Ok(result.unwrap_or(Value::Null)),
        }
    }

    /// The result as `T`, else the line to show (an answer that doesn't
    /// parse is a newer or broken daemon: say so under `what`).
    fn parse<T: serde::de::DeserializeOwned>(self, what: &str) -> Result<T, String> {
        match self {
            Answer::Ok(v) => serde_json::from_value(v).map_err(|e| format!("{what}: unexpected answer: {e}")),
            Answer::Refused(msg) | Answer::Unreachable(msg) => Err(msg),
        }
    }
}

/// The line for a request the daemon couldn't be asked (`why`: the last
/// connect error, when known), as `cancel`'s.
fn needs_serve(what: &str, why: Option<&str>) -> String {
    match why {
        Some(why) => format!("{what}: needs devsandbox serve ({why})"),
        None => format!("{what}: needs devsandbox serve"),
    }
}

/// The API call for a modal's read in config root `root`, and what its
/// failures are called.
fn fetch_call(root: &Path, fetch: &Fetch) -> (&'static str, (&'static str, Value)) {
    match fetch {
        Fetch::Logs { container } => {
            ("logs", ("containers.logs", json!({ "container": container, "lines": super::app::LOG_LINES })))
        }
        Fetch::Inspect { container } => ("inspect", ("containers.inspect", json!({ "container": container }))),
        Fetch::Config { kind, name } => {
            let method = match kind {
                ConfigKind::Sandbox => "sandboxes.get",
                ConfigKind::Service => "services.get",
            };
            ("config", (method, json!({ "root": root.to_string_lossy(), "name": name })))
        }
    }
}

/// A modal read's answer as the update that fills it.
fn fetch_update(fetch: Fetch, answer: Answer) -> DaemonUpdate {
    let body = |answer: Answer, f: fn(Value) -> String| match answer {
        Answer::Ok(v) => f(v),
        Answer::Refused(msg) | Answer::Unreachable(msg) => msg,
    };
    match fetch {
        Fetch::Logs { container } => DaemonUpdate::Logs { container, body: body(answer, logs_body) },
        Fetch::Inspect { container } => DaemonUpdate::Inspect { container, body: body(answer, inspect_body) },
        Fetch::Config { kind: ConfigKind::Sandbox, name } => {
            DaemonUpdate::Config { kind: ConfigKind::Sandbox, name, text: answer.parse("config") }
        }
        Fetch::Config { kind: ConfigKind::Service, name } => {
            let text = answer.parse::<crate::serve::api::ServiceConfig>("config").map(|s| SandboxText {
                resolved: s.original.clone(),
                original: s.original,
                hash: String::new(),
            });
            DaemonUpdate::Config { kind: ConfigKind::Service, name, text }
        }
    }
}

/// `containers.logs`' `{text}` as the modal body.
fn logs_body(result: Value) -> String {
    let text = result.get("text").and_then(Value::as_str).unwrap_or_default();
    if text.trim().is_empty() { "(no log output)".to_string() } else { text.to_string() }
}

/// `containers.inspect`'s JSON, pretty-printed as `devsandbox inspect`
/// prints it (`pretty_inspect`, minus the parse the daemon did).
fn inspect_body(result: Value) -> String {
    serde_json::to_string_pretty(&result).unwrap_or_else(|e| format!("cannot format inspect JSON: {e}"))
}

/// The API call for [`Cmd::BuildLog`]: `ansi`, so the modal can colour it.
fn build_log_call(instance: &str) -> (&'static str, Value) {
    ("instances.buildLog", json!({ "instance": instance, "lines": super::app::BUILD_LOG_LINES, "ansi": true }))
}

/// The API call for [`Cmd::Signal`].
fn signal_call(sig: &PendingSignal) -> (&'static str, Value) {
    ("instances.signal", json!({ "instance": sig.instance, "pid": sig.pid, "signal": sig.signal.wire() }))
}

/// The status line for an `instances.signal` answer.
fn signal_status(sig: &PendingSignal, answer: Answer) -> String {
    match answer {
        Answer::Ok(_) => format!("sent {} to pid {}", sig.signal.name(), sig.pid),
        Answer::Refused(msg) => format!("{}: {msg}", sig.signal.name()),
        Answer::Unreachable(line) => line,
    }
}

/// The API call for [`Cmd::Rename`].
fn rename_call(instance: &str, new_name: &str) -> (&'static str, Value) {
    ("instances.rename", json!({ "instance": instance, "newName": new_name }))
}

/// The status line for an `instances.rename` answer: the name the daemon
/// settled on (trimmed), or why not.
fn rename_status(instance: &str, new_name: &str, answer: Answer) -> String {
    match answer {
        Answer::Ok(v) => {
            let renamed = v.get("instance").and_then(Value::as_str).unwrap_or(new_name);
            format!("renamed {instance} -> {renamed}")
        }
        Answer::Refused(msg) => format!("rename failed: {msg}"),
        Answer::Unreachable(line) => line,
    }
}

/// The API call for [`Cmd::Done`].
fn done_call(req: &PendingDone) -> (&'static str, Value) {
    ("instances.done", json!({ "instance": req.instance, "done": req.done }))
}

/// The status line for an `instances.done` answer, worded as `devsandbox
/// done|undone` words a change, and whether it failed. The answer doesn't
/// say whether the flag was already so (the CLI's "is already done"): the
/// dashboard only asks for a change.
fn done_status(req: &PendingDone, answer: Answer) -> (String, bool) {
    let verb = if req.done { "done" } else { "undone" };
    match answer {
        Answer::Ok(_) if req.done => (format!("marked {} done", req.instance), false),
        Answer::Ok(_) => (format!("marked {} not done", req.instance), false),
        Answer::Refused(msg) => (format!("{verb}: {msg}"), true),
        Answer::Unreachable(line) => (line, true),
    }
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
            Ok(Cmd::ForwardAdd(req)) => w.forward_add(&req),
            Ok(Cmd::ForwardRm(id)) => w.forward_rm(id),
            Ok(Cmd::Cancel(instance)) => w.cancel(&instance),
            Ok(Cmd::Fetch(fetch)) => w.fetch(fetch),
            Ok(Cmd::BuildLog(id)) => w.build_log(id),
            Ok(Cmd::Signal(sig)) => w.signal(sig),
            Ok(Cmd::Rename { instance, new_name }) => w.rename(&instance, &new_name),
            Ok(Cmd::Done(req)) => w.set_done(req),
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

    /// `instances.cancel`; the answer (or why there's none) is a status
    /// line. No local fallback (F8): the daemon runs the dashboard's builds.
    fn cancel(&mut self, instance: &str) {
        let Some(conn) = self.conn.as_mut() else {
            let line = match &self.down {
                Some(why) => format!("cancel {instance}: needs devsandbox serve ({why})"),
                None => format!("cancel {instance}: needs devsandbox serve"),
            };
            return self.send(DaemonUpdate::Status(line));
        };
        let (method, params) = cancel_call(instance);
        match conn.call(method, params) {
            Ok(r) => self.send(DaemonUpdate::Status(cancel_status(instance, r))),
            Err(e) => {
                self.lost(false);
                self.send(DaemonUpdate::Status(format!("cancel {instance}: needs devsandbox serve ({e:#})")));
            }
        }
    }

    /// One call while connected; disconnected (or the call breaking the
    /// connection) it's [`Answer::Unreachable`] with `what`'s line.
    fn one_shot(&mut self, what: &str, (method, params): (&str, Value)) -> Answer {
        let Some(conn) = self.conn.as_mut() else {
            return Answer::Unreachable(needs_serve(what, self.down.as_deref()));
        };
        match conn.call(method, params) {
            Ok(r) => Answer::from_response(r),
            Err(e) => {
                self.lost(false);
                Answer::Unreachable(needs_serve(what, Some(&format!("{e:#}"))))
            }
        }
    }

    fn fetch(&mut self, fetch: Fetch) {
        let (what, call) = fetch_call(self.dir, &fetch);
        let answer = self.one_shot(what, call);
        self.send(fetch_update(fetch, answer));
    }

    fn build_log(&mut self, instance: String) {
        let read = self.one_shot("build log", build_log_call(&instance)).parse("build log");
        self.send(DaemonUpdate::BuildLog { instance, read });
    }

    fn signal(&mut self, sig: PendingSignal) {
        let answer = self.one_shot(sig.signal.name(), signal_call(&sig));
        self.send(DaemonUpdate::Signalled(signal_status(&sig, answer)));
    }

    fn rename(&mut self, instance: &str, new_name: &str) {
        let answer = self.one_shot("rename", rename_call(instance, new_name));
        self.send(DaemonUpdate::Renamed(rename_status(instance, new_name, answer)));
    }

    fn set_done(&mut self, req: PendingDone) {
        let what = if req.done { "done" } else { "undone" };
        let answer = self.one_shot(what, done_call(&req));
        let (status, failed) = done_status(&req, answer);
        self.send(DaemonUpdate::Done { status, failed, report: req.report });
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
            (
                Op::SaveDraft { thread: 9, message: "m".into(), answers: [("q".to_string(), inbox::Answer::Choices(vec!["a".into()]))].into() },
                "inbox.form.saveDraft",
                json!({"thread": 9, "message": "m", "answers": {"q": ["a"]}}),
            ),
            (
                Op::Submit { thread: 9, message: "m".into(), answers: [("q".to_string(), inbox::Answer::Confirm(true))].into() },
                "inbox.form.submit",
                json!({"thread": 9, "message": "m", "answers": {"q": true}}),
            ),
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
            let blocks = vec![inbox::Block::Form(inbox::form::tests::mixed())];
            let send = inbox::MessageSend { thread: "k".into(), id: "m".into(), blocks };
            store::update_at(path, |i| i.send("d-id", 1, send)).unwrap();
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
            Op::SaveDraft { thread, message: "m".into(), answers: [("pick".to_string(), inbox::Answer::Choice("a".into()))].into() },
            Op::Submit { thread, message: "m".into(), answers: [("note".to_string(), inbox::Answer::Text("hi".into()))].into() },
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
            let e: Vec<_> = events.into_iter().map(|(_, e)| (e.kind.as_str().to_string(), e.action, e.text, e.message, e.form, e.answers)).collect();
            format!("{t:?} {e:?}")
        };
        assert_eq!(strip(&via_api), strip(&via_ops));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands_map_to_their_api_methods() {
        // The handle queues each request as its command, in order.
        let (tx, rx) = mpsc::channel();
        let (_utx, updates) = mpsc::channel();
        let worker = DaemonWorker { tx: Some(tx), updates, connected: Arc::default(), handle: None };
        worker.apply(vec![Op::MarkRead(1)]);
        worker.cancel("web".into());
        assert_eq!(rx.try_recv().unwrap(), Cmd::Inbox(vec![Op::MarkRead(1)]));
        assert_eq!(rx.try_recv().unwrap(), Cmd::Cancel("web".into()));
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

    /// `d`'s cancel queues as its command, its call is what the API's
    /// `instances.cancel` parses (not run: that would read the user's
    /// state), and each answer kind reads as a status line.
    #[test]
    fn cancel_queues_maps_to_the_api_and_answers_render() {
        use crate::serve::api::{self, CancelParams};
        use crate::serve::proto::ErrorBody;
        let (tx, rx) = mpsc::channel();
        let (_utx, updates) = mpsc::channel();
        let worker = DaemonWorker { tx: Some(tx), updates, connected: Arc::default(), handle: None };
        worker.cancel("web-2".into());
        assert_eq!(rx.try_recv().unwrap(), Cmd::Cancel("web-2".into()));

        let (method, params) = cancel_call("web-2");
        assert_eq!((method, &params), ("instances.cancel", &json!({"instance": "web-2"})));
        let parsed: CancelParams = api::parse(params).unwrap();
        assert_eq!(parsed.instance, "web-2");

        let ok = |v: Value| Response { id: Some(1), result: Some(v), error: None };
        let err = |code: &str, message: &str| Response {
            id: Some(1),
            result: None,
            error: Some(ErrorBody { code: code.into(), message: message.into(), data: None }),
        };
        assert_eq!(cancel_status("web-2", ok(json!({"ok": true, "pid": 42}))), "cancelling web-2 (SIGINT to pid 42)");
        assert_eq!(
            cancel_status("web-2", err("invalid", "`web-2` is not building any more")),
            "cancel web-2: `web-2` is not building any more"
        );
        assert_eq!(cancel_status("x", err("not-found", "no instance `x`")), "cancel x: no instance `x`");
    }

    fn sig(pid: &str) -> PendingSignal {
        PendingSignal { instance: "web-2".into(), pid: pid.into(), signal: super::super::app::Signal::Term }
    }

    fn response(result: Option<Value>, error: Option<(&str, &str)>) -> Response {
        use crate::serve::proto::ErrorBody;
        let error = error.map(|(code, message)| ErrorBody { code: code.into(), message: message.into(), data: None });
        Response { id: Some(1), result, error }
    }

    /// The one-shot requests queue as their commands, in order.
    #[test]
    fn one_shot_requests_queue_as_their_commands() {
        let (tx, rx) = mpsc::channel();
        let (_utx, updates) = mpsc::channel();
        let worker = DaemonWorker { tx: Some(tx), updates, connected: Arc::default(), handle: None };
        let logs = Fetch::Logs { container: "devsandbox-web".into() };
        let done = PendingDone { instance: "web".into(), done: true, report: false };
        worker.fetch(logs.clone());
        worker.build_log("id0".into());
        worker.signal(sig("42"));
        worker.rename("a".into(), "b".into());
        worker.set_done(done.clone());
        assert_eq!(rx.try_recv().unwrap(), Cmd::Fetch(logs));
        assert_eq!(rx.try_recv().unwrap(), Cmd::BuildLog("id0".into()));
        assert_eq!(rx.try_recv().unwrap(), Cmd::Signal(sig("42")));
        assert_eq!(rx.try_recv().unwrap(), Cmd::Rename { instance: "a".into(), new_name: "b".into() });
        assert_eq!(rx.try_recv().unwrap(), Cmd::Done(done));
    }

    #[test]
    fn one_shot_calls_name_their_methods_and_params() {
        let root = Path::new("/cfg");
        let fetch = |f: Fetch| fetch_call(root, &f);
        assert_eq!(
            fetch(Fetch::Logs { container: "devsandbox-web".into() }),
            ("logs", ("containers.logs", json!({"container": "devsandbox-web", "lines": 50})))
        );
        assert_eq!(
            fetch(Fetch::Inspect { container: "devsandbox-web".into() }),
            ("inspect", ("containers.inspect", json!({"container": "devsandbox-web"})))
        );
        assert_eq!(
            fetch(Fetch::Config { kind: ConfigKind::Sandbox, name: "web".into() }),
            ("config", ("sandboxes.get", json!({"root": "/cfg", "name": "web"})))
        );
        assert_eq!(
            fetch(Fetch::Config { kind: ConfigKind::Service, name: "db".into() }),
            ("config", ("services.get", json!({"root": "/cfg", "name": "db"})))
        );
        assert_eq!(build_log_call("id0"), ("instances.buildLog", json!({"instance": "id0", "lines": 500, "ansi": true})));
        assert_eq!(
            signal_call(&PendingSignal { signal: super::super::app::Signal::Kill, ..sig("42") }),
            ("instances.signal", json!({"instance": "web-2", "pid": "42", "signal": "KILL"}))
        );
        assert_eq!(rename_call("a", "b"), ("instances.rename", json!({"instance": "a", "newName": "b"})));
        assert_eq!(
            done_call(&PendingDone { instance: "a".into(), done: false, report: true }),
            ("instances.done", json!({"instance": "a", "done": false}))
        );
    }

    /// The calls the API can run here without docker or the user's state:
    /// the config reads end to end, the rest as far as their param checks.
    #[test]
    fn one_shot_calls_are_accepted_by_the_api() {
        use crate::serve::api::{self, Ctx};
        let dir = std::env::temp_dir().join(format!("devsandbox-tui-oneshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "[sandbox.web]\nimage = \"alpine\"\n[services.db]\nimage = \"postgres\"\n",
        )
        .unwrap();
        let inbox = dir.join("inbox.json");
        let ctx = Ctx { inbox: &inbox, client: "tui", bridges: None, forwards: None };
        let ask = |(method, params): (&str, Value)| match api::call(method, params, &ctx) {
            Ok(v) => Answer::Ok(v),
            Err(e) => Answer::Refused(e.message),
        };

        let (_, call) = fetch_call(&dir, &Fetch::Config { kind: ConfigKind::Sandbox, name: "web".into() });
        let update = fetch_update(Fetch::Config { kind: ConfigKind::Sandbox, name: "web".into() }, ask(call));
        let DaemonUpdate::Config { kind: ConfigKind::Sandbox, name, text: Ok(text) } = update else { panic!("{update:?}") };
        assert_eq!((name.as_str(), text.original.as_str()), ("web", "image = \"alpine\"\n"));
        assert!(!text.hash.is_empty());
        let service = Fetch::Config { kind: ConfigKind::Service, name: "db".into() };
        let (_, call) = fetch_call(&dir, &service);
        assert_eq!(
            fetch_update(service, ask(call)),
            DaemonUpdate::Config {
                kind: ConfigKind::Service,
                name: "db".into(),
                text: Ok(SandboxText {
                    original: "image = \"postgres\"\n".into(),
                    resolved: "image = \"postgres\"\n".into(),
                    hash: String::new()
                }),
            }
        );
        let unknown = Fetch::Config { kind: ConfigKind::Sandbox, name: "nope".into() };
        let (_, call) = fetch_call(&dir, &unknown);
        let DaemonUpdate::Config { text: Err(msg), .. } = fetch_update(unknown, ask(call)) else { panic!() };
        assert!(msg.contains("no sandbox `nope`"), "{msg}");
        // Parsed and checked before any state or runtime is touched.
        let not_ours = Fetch::Logs { container: "web".into() };
        let (_, call) = fetch_call(&dir, &not_ours);
        let DaemonUpdate::Logs { body, .. } = fetch_update(not_ours, ask(call)) else { panic!() };
        assert!(body.contains("not a devsandbox container"), "{body}");
        let (_, call) = fetch_call(&dir, &Fetch::Inspect { container: "web".into() });
        assert!(matches!(ask(call), Answer::Refused(m) if m.contains("not a devsandbox container")));
        assert!(matches!(ask(signal_call(&sig("0"))), Answer::Refused(m) if m.contains("`pid` must be a process id")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_shot_answers_render() {
        // Responses: a result (none is `null`), or the daemon's message.
        assert_eq!(Answer::from_response(response(Some(json!({"ok": true})), None)), Answer::Ok(json!({"ok": true})));
        assert_eq!(Answer::from_response(response(None, None)), Answer::Ok(Value::Null));
        assert_eq!(Answer::from_response(response(None, Some(("not-found", "no instance `x`")))), Answer::Refused("no instance `x`".into()));
        assert_eq!(needs_serve("logs", Some("refused")), "logs: needs devsandbox serve (refused)");
        assert_eq!(needs_serve("logs", None), "logs: needs devsandbox serve");
        let down = || Answer::Unreachable(needs_serve("x", Some("down")));

        // Logs and inspect: the body, empty output said, errors as is.
        let logs = |a| match fetch_update(Fetch::Logs { container: "devsandbox-a".into() }, a) {
            DaemonUpdate::Logs { container, body } if container == "devsandbox-a" => body,
            other => panic!("{other:?}"),
        };
        assert_eq!(logs(Answer::Ok(json!({"text": "l1\nl2\n"}))), "l1\nl2\n");
        assert_eq!(logs(Answer::Ok(json!({"text": " \n"}))), "(no log output)");
        assert_eq!(logs(Answer::Refused("No such container".into())), "No such container");
        assert_eq!(logs(down()), "x: needs devsandbox serve (down)");
        let inspect = |a| match fetch_update(Fetch::Inspect { container: "devsandbox-a".into() }, a) {
            DaemonUpdate::Inspect { body, .. } => body,
            other => panic!("{other:?}"),
        };
        // As `devsandbox inspect` prints it.
        let raw = r#"[{"Id":"abc","State":{"Running":true}}]"#;
        assert_eq!(inspect(Answer::Ok(serde_json::from_str(raw).unwrap())), crate::commands::inspect::pretty_inspect(raw));
        assert_eq!(inspect(down()), "x: needs devsandbox serve (down)");
        let config = |a| match fetch_update(Fetch::Config { kind: ConfigKind::Sandbox, name: "web".into() }, a) {
            DaemonUpdate::Config { text, .. } => text,
            other => panic!("{other:?}"),
        };
        assert_eq!(config(down()), Err("x: needs devsandbox serve (down)".into()));
        assert!(config(Answer::Ok(json!({"surprise": 1}))).unwrap_err().starts_with("config: unexpected answer"));

        // The build log: `null` is none yet.
        let read = |a: Answer| a.parse::<Option<Tail>>("build log");
        assert_eq!(read(Answer::Ok(Value::Null)), Ok(None));
        assert_eq!(read(Answer::Ok(json!({"text": "x", "cut": true}))), Ok(Some(Tail { text: "x".into(), cut: true })));
        assert_eq!(read(Answer::Refused("no instance `id0`".into())), Err("no instance `id0`".into()));

        // Status lines, as the in-process paths worded them.
        assert_eq!(signal_status(&sig("42"), Answer::Ok(json!({"ok": true}))), "sent SIGTERM to pid 42");
        assert_eq!(signal_status(&sig("42"), Answer::Refused("no instance `web-2`".into())), "SIGTERM: no instance `web-2`");
        assert_eq!(signal_status(&sig("42"), down()), "x: needs devsandbox serve (down)");
        assert_eq!(rename_status("a", " b ", Answer::Ok(json!({"instance": "b"}))), "renamed a -> b");
        assert_eq!(rename_status("a", "b", Answer::Refused("`b` is taken".into())), "rename failed: `b` is taken");
        assert_eq!(rename_status("a", "b", down()), "x: needs devsandbox serve (down)");
        let done = |done| PendingDone { instance: "a".into(), done, report: true };
        let ok = || Answer::Ok(json!({"instance": "a", "done": true}));
        assert_eq!(done_status(&done(true), ok()), ("marked a done".into(), false));
        assert_eq!(done_status(&done(false), ok()), ("marked a not done".into(), false));
        assert_eq!(done_status(&done(false), Answer::Refused("no instance `a`".into())), ("undone: no instance `a`".into(), true));
        assert_eq!(done_status(&done(true), down()), ("x: needs devsandbox serve (down)".into(), true));
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
