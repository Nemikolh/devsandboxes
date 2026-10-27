//! Host → container requests (docs/architecture/helper/bridges.md, *Host →
//! container requests*): every call into a container's `devsbd daemon` goes
//! through a [`HelperClient`], one per serve daemon. It finds the container's
//! live bridge link on the [`BridgeBoard`], checks the helper serves `RPC`,
//! queues the call behind that container's [`MAX_CALLS`], opens one `RPC`
//! stream per call (`Mux::open_host`), stamps `meta.caller`, and logs one line
//! per call. [`Call`] reads the reply records; cancelling is closing the
//! stream. What a container's daemon offers (`rpc.methods`) is cached per
//! bridge attach ([`HelperClient::offers`]).
//!
//! Also the internal `helper.call` API method ([`parse`], [`serve`]): a held
//! request like a verb, the call's records relayed as `helper.record`
//! notifications, its last one the answer; a hang-up cancels the call.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::api::{self, ApiError};
use super::verbs::{Failure, Outcome, Peer};
use crate::devsbd::bridge::{BridgeBoard, Link};
use crate::devsbd::mux::{Conn, Mux};
use crate::devsbd::proto::caps;
use crate::devsbd::rpc::{self, Exit, Json, MAX_DEPTH, Obj, Record};

/// Calls in flight per container; more wait in its queue, in arrival order.
/// Each one is a host-opened stream, and those share the bridge's
/// `MAX_HOST_STREAMS` (256, `src/devsbd/mux.rs`) with the port forwards'
/// `Connect`s, of which one browser tab can open dozens: 16 leaves the
/// forwards well over 200. It also bounds the in-container daemon's threads
/// (one per stream). Held streams (a `run.wait`) count, so a caller that
/// holds many queues the rest.
pub const MAX_CALLS: usize = 16;

/// How often a queued caller re-checks whether it was abandoned, and the
/// `helper.call` relay its connection for a hang-up.
const TICK: Duration = Duration::from_millis(50);

/// How long a call that ended without a last record waits for the stream's
/// close reason (the mux reports it right after the local end reads EOF).
const REASON_WAIT: Duration = Duration::from_secs(1);

/// Where calls find a container's live bridge link: the daemon's
/// [`BridgeBoard`], or a fake in tests.
pub trait Links: Send + Sync {
    fn link(&self, container: &str) -> Option<Link>;
}

impl Links for BridgeBoard {
    fn link(&self, container: &str) -> Option<Link> {
        BridgeBoard::link(self, container)
    }
}

/// Why a call didn't start.
#[derive(Debug)]
pub enum CallError {
    /// No live bridge to the container: not running, no helper, or its
    /// handshake isn't done.
    NoBridge(String),
    /// The container's helper doesn't serve `RPC` (an older one): the caller
    /// falls back to `exec`.
    Unsupported(String),
    /// The caller gave up while queued.
    Abandoned,
    /// Opening the stream or sending the `call` failed.
    Io(io::Error),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CallError::NoBridge(c) => {
                write!(f, "no live bridge to {c} (not running, no helper, or its handshake isn't done)")
            }
            CallError::Unsupported(c) => write!(f, "the helper in {c} has no RPC: restart the instance"),
            CallError::Abandoned => write!(f, "abandoned in the queue"),
            CallError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl CallError {
    /// The API error `helper.call` answers.
    fn failure(&self) -> Failure {
        let code = match self {
            CallError::NoBridge(_) => "not-found",
            CallError::Unsupported(_) => "unsupported",
            CallError::Abandoned | CallError::Io(_) => "failed",
        };
        Failure { code, message: self.to_string(), data: None }
    }
}

/// One container's calls: how many run, and who waits (ids, in order).
#[derive(Default)]
struct Lane {
    active: usize,
    queue: VecDeque<u64>,
}

/// Pure queue rule: `me` starts once it's first in line and a slot is free.
fn may_start(lane: &Lane, me: u64, limit: usize) -> bool {
    lane.queue.front() == Some(&me) && lane.active < limit
}

struct Inner {
    links: Box<dyn Links>,
    limit: usize,
    lanes: Mutex<HashMap<String, Lane>>,
    cv: Condvar,
    next_waiter: AtomicU64,
    /// Container → (the bridge attach it was asked on, `rpc.methods`' list).
    #[allow(dead_code)] // `offers`' cache
    methods: Mutex<HashMap<String, (u64, Arc<Vec<String>>)>>,
    log: fn(&str),
}

/// The serve daemon's one way into containers' daemons. Cloneable.
#[derive(Clone)]
pub struct HelperClient {
    inner: Arc<Inner>,
}

impl HelperClient {
    /// Calls over `links`, at most [`MAX_CALLS`] per container; `log`
    /// writes one `serve.log` line.
    pub fn new(links: impl Links + 'static, log: fn(&str)) -> HelperClient {
        HelperClient::with_limit(links, MAX_CALLS, log)
    }

    fn with_limit(links: impl Links + 'static, limit: usize, log: fn(&str)) -> HelperClient {
        HelperClient {
            inner: Arc::new(Inner {
                links: Box::new(links),
                limit,
                lanes: Mutex::default(),
                cv: Condvar::new(),
                next_waiter: AtomicU64::new(1),
                methods: Mutex::default(),
                log,
            }),
        }
    }

    /// Call `method` in `container`'s daemon with `params`; `meta` gets
    /// `caller` (who asked: a dispatcher instance, an API client) stamped
    /// over whatever it had. Waits its turn behind the container's
    /// [`MAX_CALLS`], checking `abandoned` every [`TICK`] meanwhile. The call
    /// sends no input: its stream is half-closed after the `call` record.
    /// Read the reply with [`Call::next`]; dropping the [`Call`] before its
    /// last record cancels it.
    pub fn call(
        &self,
        container: &str,
        method: &str,
        params: Obj,
        meta: Obj,
        caller: &str,
        abandoned: &dyn Fn() -> bool,
    ) -> Result<Call, CallError> {
        let started = Instant::now();
        let logged = |e: CallError| {
            log_call(self.inner.log, container, method, caller, &e.to_string(), started);
            e
        };
        self.usable(container).map_err(logged)?;
        let slot = self.acquire(container, abandoned).map_err(logged)?;
        // Again: the bridge may have gone, or been replaced, while we queued.
        let link = self.usable(container).map_err(logged)?;
        let (tx, closed) = mpsc::channel();
        let (stream, mut conn) = link
            .open_rpc(move |reason| {
                let _ = tx.send(reason);
            })
            .map_err(|e| logged(CallError::Io(e)))?;
        let cancel = Cancel { mux: Arc::clone(&link.mux), stream, cancelled: Arc::default() };
        let record = Record::Call(rpc::Call { method: method.to_string(), params, meta: meta.with("caller", caller) });
        if let Err(e) = rpc::write_record(&mut conn, &record).and_then(|()| conn.shutdown(Shutdown::Write)) {
            link.mux.close_stream(stream, "call not sent");
            return Err(logged(CallError::Io(e)));
        }
        Ok(Call {
            conn,
            cancel,
            closed,
            attach: link.attach,
            outcome: None,
            done: false,
            started,
            container: container.to_string(),
            method: method.to_string(),
            caller: caller.to_string(),
            log: self.inner.log,
            _slot: slot,
        })
    }

    /// Whether `container`'s daemon offers `method`: a bare name matches any
    /// version, `name@version` only that one (`rpc.methods`' entries). Asked
    /// once per bridge attach (a call like any other, logged, by `caller`),
    /// then cached. `Ok(false)` from a helper without `RPC`: the caller
    /// falls back to `exec` as for a method it doesn't offer.
    #[allow(dead_code)] // the dispatcher's run ops next (docs/plans/active/helper-rpc.md)
    pub fn offers(&self, container: &str, method: &str, caller: &str) -> Result<bool, CallError> {
        let link = match self.usable(container) {
            Ok(link) => link,
            Err(CallError::Unsupported(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        let cached = self.inner.methods.lock().unwrap().get(container).filter(|(at, _)| *at == link.attach).map(|(_, m)| Arc::clone(m));
        let list = match cached {
            Some(list) => list,
            None => {
                let (attach, list) = self.methods_now(container, caller)?;
                let list = Arc::new(list);
                self.inner.methods.lock().unwrap().insert(container.to_string(), (attach, Arc::clone(&list)));
                list
            }
        };
        Ok(offered(&list, method))
    }

    /// `rpc.methods`, asked now: the attach it went over and the list.
    #[allow(dead_code)] // `offers`
    fn methods_now(&self, container: &str, caller: &str) -> Result<(u64, Vec<String>), CallError> {
        let mut call = self.call(container, "rpc.methods", Obj::new(), Obj::new(), caller, &|| false)?;
        let attach = call.attach;
        let bad = |what: String| CallError::Io(io::Error::new(io::ErrorKind::InvalidData, format!("rpc.methods: {what}")));
        match call.next() {
            Ok(Some(Record::Result(o))) => Ok((attach, o.strs("methods").unwrap_or_default().to_vec())),
            Ok(Some(Record::Error { code, message })) => Err(bad(format!("{code}: {message}"))),
            Ok(Some(other)) => Err(bad(format!("unexpected {other:?}"))),
            Ok(None) => Err(bad("no reply".into())),
            Err(e) => Err(CallError::Io(e)),
        }
    }

    /// `container`'s link, if it has one that serves `RPC`.
    fn usable(&self, container: &str) -> Result<Link, CallError> {
        let link = self.inner.links.link(container).ok_or_else(|| CallError::NoBridge(container.to_string()))?;
        if link.caps & caps::RPC == 0 {
            return Err(CallError::Unsupported(container.to_string()));
        }
        Ok(link)
    }

    /// Queue on `container`'s lane until [`may_start`]; `Abandoned` (and out
    /// of line) once `abandoned` says so.
    fn acquire(&self, container: &str, abandoned: &dyn Fn() -> bool) -> Result<Slot, CallError> {
        let me = self.inner.next_waiter.fetch_add(1, Ordering::Relaxed);
        let mut lanes = self.inner.lanes.lock().unwrap();
        lanes.entry(container.to_string()).or_default().queue.push_back(me);
        loop {
            let lane = lanes.get_mut(container).expect("our lane");
            if may_start(lane, me, self.inner.limit) {
                lane.queue.pop_front();
                lane.active += 1;
                drop(lanes);
                // The next in line may fit too.
                self.inner.cv.notify_all();
                return Ok(Slot { inner: Arc::clone(&self.inner), container: container.to_string() });
            }
            lanes = self.inner.cv.wait_timeout(lanes, TICK).unwrap().0;
            drop(lanes);
            let gone = abandoned();
            lanes = self.inner.lanes.lock().unwrap();
            if gone {
                let lane = lanes.get_mut(container).expect("our lane");
                lane.queue.retain(|&id| id != me);
                if lane.active == 0 && lane.queue.is_empty() {
                    lanes.remove(container);
                }
                drop(lanes);
                self.inner.cv.notify_all();
                return Err(CallError::Abandoned);
            }
        }
    }
}

/// `list` (`name@version` entries) offers `method` (a bare name, or one
/// with its version).
#[allow(dead_code)] // `offers`
fn offered(list: &[String], method: &str) -> bool {
    if method.contains('@') {
        list.iter().any(|m| m == method)
    } else {
        list.iter().any(|m| m.split_once('@').map_or(m.as_str(), |(name, _)| name) == method)
    }
}

/// A running call's place in its container's lane; freed on drop.
struct Slot {
    inner: Arc<Inner>,
    container: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut lanes = self.inner.lanes.lock().unwrap();
        if let Some(lane) = lanes.get_mut(&self.container) {
            lane.active -= 1;
            if lane.active == 0 && lane.queue.is_empty() {
                lanes.remove(&self.container);
            }
        }
        drop(lanes);
        self.inner.cv.notify_all();
    }
}

/// Cancels a [`Call`] from another thread: closes its stream (`Close
/// "cancelled"`), so the daemon drops it and the reader sees the end.
#[derive(Clone)]
pub struct Cancel {
    mux: Arc<Mux>,
    stream: u32,
    cancelled: Arc<AtomicBool>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.mux.close_stream(self.stream, "cancelled");
    }
}

/// One call in flight: its reply records, in order, the last one a
/// `result`, `error` or `exit`. Logged once, when dropped.
pub struct Call {
    conn: Conn,
    cancel: Cancel,
    closed: mpsc::Receiver<String>,
    /// The bridge attach it went over (`offers`' cache key).
    #[allow(dead_code)]
    attach: u64,
    /// The log line's outcome, once known.
    outcome: Option<String>,
    /// The last record was read (or the call failed): `next` reads no more.
    done: bool,
    started: Instant,
    container: String,
    method: String,
    caller: String,
    log: fn(&str),
    _slot: Slot,
}

impl Call {
    /// The next reply record; `Ok(None)` after the last one. A stream that
    /// ends without one (cancelled, the bridge or daemon gone) is an error
    /// saying why.
    pub fn next(&mut self) -> io::Result<Option<Record>> {
        if self.done {
            return Ok(None);
        }
        let ended = match rpc::read_record(&mut self.conn) {
            Ok(Some(rec)) => {
                if let Some(outcome) = last_outcome(&rec) {
                    self.done = true;
                    self.outcome = Some(outcome);
                }
                return Ok(Some(rec));
            }
            Ok(None) => None,
            Err(e) => Some(e),
        };
        self.done = true;
        let why = if self.cancel.cancelled.load(Ordering::SeqCst) {
            "cancelled".to_string()
        } else {
            match (ended, self.closed.recv_timeout(REASON_WAIT)) {
                (Some(e), _) => {
                    self.cancel.mux.close_stream(self.cancel.stream, "bad reply");
                    format!("bad reply: {e}")
                }
                (None, Ok(reason)) if !reason.is_empty() => format!("ended: {reason}"),
                (None, _) => "ended without a reply".to_string(),
            }
        };
        self.outcome = Some(why.clone());
        Err(io::Error::new(io::ErrorKind::UnexpectedEof, why))
    }

    /// A handle that cancels this call from another thread.
    pub fn canceller(&self) -> Cancel {
        self.cancel.clone()
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if !self.done {
            self.cancel.cancel();
        }
        let outcome = self.outcome.clone().unwrap_or_else(|| "cancelled".into());
        log_call(self.log, &self.container, &self.method, &self.caller, &outcome, self.started);
    }
}

/// The log line's outcome for a last record; `None` for any other.
fn last_outcome(rec: &Record) -> Option<String> {
    match rec {
        Record::Result(_) => Some("ok".into()),
        Record::Error { code, message } => Some(format!("error {code}: {message}")),
        Record::Exit(Exit::Code(n)) => Some(format!("exit {n}")),
        Record::Exit(Exit::Signal(n)) => Some(format!("signal {n}")),
        _ => None,
    }
}

fn log_call(log: fn(&str), container: &str, method: &str, caller: &str, outcome: &str, started: Instant) {
    log(&format!("helper {container}: {method} from {caller}: {outcome} ({} ms)", started.elapsed().as_millis()));
}

// ---- `helper.call` ----

#[derive(Deserialize)]
struct RawParams {
    instance: String,
    method: String,
    #[serde(default)]
    params: Value,
}

/// `helper.call`'s params, checked.
#[derive(Debug)]
pub struct CallParams {
    /// The instance name (a `state.toml` key).
    pub instance: String,
    pub method: String,
    pub params: Obj,
}

/// `{"instance", "method", "params"?}`: `params` an object in the RPC JSON
/// subset (`rpc.rs`), absent or `null` = `{}`.
pub fn parse(params: Value) -> Result<CallParams, ApiError> {
    let p: RawParams = api::parse(params)?;
    if p.instance.is_empty() || p.method.is_empty() {
        return Err(ApiError::invalid("`instance` and `method` must be non-empty"));
    }
    let params = to_obj(&p.params).map_err(|e| ApiError::invalid(format!("`params`: {e}")))?;
    Ok(CallParams { instance: p.instance, method: p.method, params })
}

/// A JSON value as a call's `params`: an object (or `null`, empty) the RPC
/// JSON subset can carry, nested under the call's own object within
/// `MAX_DEPTH`.
pub fn to_obj(v: &Value) -> Result<Obj, String> {
    match v {
        Value::Null => Ok(Obj::new()),
        // The call record is depth 1, its `params` 2.
        Value::Object(_) => match to_json(v, 2)? {
            Json::Obj(o) => Ok(o),
            _ => unreachable!("an object"),
        },
        _ => Err("not an object".into()),
    }
}

fn to_json(v: &Value, depth: usize) -> Result<Json, String> {
    Ok(match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Number(n) => Json::Int(n.as_i64().ok_or_else(|| format!("{n}: only integers in i64 range"))?),
        Value::String(s) => Json::Str(s.clone()),
        Value::Array(items) => Json::Strs(
            items
                .iter()
                .map(|i| i.as_str().map(str::to_string).ok_or_else(|| "arrays hold strings only".to_string()))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(m) => {
            if depth > MAX_DEPTH {
                return Err(format!("objects nest at most {} deep", MAX_DEPTH - 1));
            }
            let mut o = Obj::new();
            for (k, v) in m {
                o = o.with(k, to_json(v, depth + 1)?);
            }
            Json::Obj(o)
        }
    })
}

/// An RPC JSON value as API JSON.
pub fn from_json(v: &Json) -> Value {
    match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(n) => json!(n),
        Json::Str(s) => Value::String(s.clone()),
        Json::Strs(items) => json!(items),
        Json::Obj(o) => Value::Object(o.0.iter().map(|(k, v)| (k.clone(), from_json(v))).collect::<Map<_, _>>()),
    }
}

/// What `helper.call` does with one reply record: a `helper.record`
/// notification's params, or the answer (the last record).
#[derive(Debug, PartialEq)]
enum Relay {
    Note(Value),
    Answer(Value),
}

/// `stdout`/`stderr` → `{"request", "record", "data": <UTF-8, lossy>}`,
/// `event` → `data` the event's object; `result` → `{"result": {…}}`,
/// `error` → `{"error": {"code", "message"}}`, `exit` → `{"exit": n}` or
/// `{"signal": n}`. Caller-side records coming back are a broken helper.
fn relay(request: Option<u64>, rec: &Record) -> Result<Relay, String> {
    let note = |record: &str, data: Value| Relay::Note(json!({"request": request, "record": record, "data": data}));
    Ok(match rec {
        Record::Stdout(b) => note("stdout", Value::String(String::from_utf8_lossy(b).into_owned())),
        Record::Stderr(b) => note("stderr", Value::String(String::from_utf8_lossy(b).into_owned())),
        Record::Event(o) => note("event", from_json(&Json::Obj(o.clone()))),
        Record::Result(o) => Relay::Answer(json!({"result": from_json(&Json::Obj(o.clone()))})),
        Record::Error { code, message } => Relay::Answer(json!({"error": {"code": code, "message": message}})),
        Record::Exit(Exit::Code(n)) => Relay::Answer(json!({"exit": n})),
        Record::Exit(Exit::Signal(n)) => Relay::Answer(json!({"signal": n})),
        other => return Err(format!("the helper sent a caller's record: {other:?}")),
    })
}

/// Run a `helper.call` to its answer: `p` in `container`, by `caller`,
/// relaying records to `peer` (`request` is the id the notifications
/// carry). A hang-up, queued or mid-call, cancels it: `Gone`.
pub fn serve(client: &HelperClient, container: &str, p: CallParams, request: Option<u64>, caller: &str, peer: &dyn Peer) -> Outcome {
    let abandoned = || peer.hung_up(Duration::ZERO);
    let mut call = match client.call(container, &p.method, p.params, Obj::new(), caller, &abandoned) {
        Ok(call) => call,
        Err(CallError::Abandoned) => return Outcome::Gone,
        Err(e) => return Outcome::Answer(Err(e.failure())),
    };
    let cancel = call.canceller();
    let failed = |message: String| Outcome::Answer(Err(Failure { code: "failed", message, data: None }));
    let (tx, rx) = mpsc::channel();
    // The reader blocks on the stream; this thread watches the connection.
    // Scoped: the call (and its log line) is done before the answer goes.
    std::thread::scope(|s| {
        s.spawn(move || {
            loop {
                let next = call.next();
                let more = matches!(next, Ok(Some(_)));
                if tx.send(next).is_err() || !more {
                    return;
                }
            }
        });
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Ok(Some(rec))) => match relay(request, &rec) {
                    Ok(Relay::Answer(answer)) => return Outcome::Answer(Ok(answer)),
                    Ok(Relay::Note(params)) => {
                        if peer.notify("helper.record", params).is_err() {
                            cancel.cancel();
                            return Outcome::Gone;
                        }
                    }
                    Err(e) => {
                        cancel.cancel();
                        return failed(e);
                    }
                },
                Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => return failed("the call ended without a reply".into()),
                Ok(Err(e)) => return failed(e.to_string()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if peer.hung_up(Duration::ZERO) {
                        cancel.cancel();
                        return Outcome::Gone;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::devsbd::proto::channel;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicUsize;

    /// Links to fake containers, each a mux pair whose daemon side answers
    /// `RPC` streams with `respond`.
    #[derive(Default, Clone)]
    pub(crate) struct Fake(Arc<Mutex<HashMap<String, Link>>>);

    impl Links for Fake {
        fn link(&self, container: &str) -> Option<Link> {
            self.0.lock().unwrap().get(container).cloned()
        }
    }

    impl Fake {
        pub(crate) fn add(&self, container: &str, caps: u32, attach: u64, respond: fn(UnixStream)) {
            let (d_r, h_w) = std::io::pipe().unwrap();
            let (h_r, d_w) = std::io::pipe().unwrap();
            let daemon = Mux::new(d_w);
            let host = Mux::new(h_w);
            daemon.on_host_open(Arc::new(move |_, ch| {
                (ch == channel::RPC).then(|| {
                    let (ours, theirs) = UnixStream::pair().unwrap();
                    std::thread::spawn(move || respond(theirs));
                    ours.into()
                })
            }));
            std::thread::spawn(move || daemon.serve(d_r, |_, _| None));
            let h = Arc::clone(&host);
            std::thread::spawn(move || h.serve(h_r, |_, _| None));
            self.0.lock().unwrap().insert(container.to_string(), Link { mux: host, caps, attach });
        }
    }

    /// Calls the test responders answered, per method name.
    static ASKED: AtomicUsize = AtomicUsize::new(0);

    /// Like `rpc_server`'s `serve`: `rpc.methods`, `echo` (params and meta
    /// back), `chatty` (stdout, an event, exit 3), `hang` (a stdout, then
    /// nothing until the stream goes), anything else `unknown-method`.
    pub(crate) fn respond(s: UnixStream) {
        let mut r = &s;
        let Ok(Some(Record::Call(call))) = rpc::read_record(&mut r) else { return };
        let mut w = &s;
        let mut send = |rec: Record| rpc::write_record(&mut w, &rec);
        match call.method.as_str() {
            "rpc.methods" => {
                ASKED.fetch_add(1, Ordering::SeqCst);
                let _ = send(Record::Result(Obj::new().with("methods", vec!["rpc.methods@1".to_string(), "run.start@2".to_string()])));
            }
            "echo" => {
                let _ = send(Record::Result(Obj::new().with("params", call.params).with("meta", call.meta)));
            }
            "chatty" => {
                let _ = send(Record::Stdout(b"hi\n".to_vec()));
                let _ = send(Record::Event(Obj::new().with("state", "running")));
                let _ = send(Record::Exit(Exit::Code(3)));
            }
            "hang" => {
                // A long-running method: the caller's `Eof` doesn't end it,
                // only the stream going (its socket shut by a `Close`).
                let _ = send(Record::Stdout(b"working".to_vec()));
                let _ = std::io::copy(&mut r, &mut std::io::sink());
                std::thread::sleep(Duration::from_secs(3));
                return;
            }
            m => {
                let _ = send(Record::error(rpc::code::UNKNOWN_METHOD, format!("no method `{m}`")));
            }
        }
        let _ = s.shutdown(Shutdown::Write);
        let _ = std::io::copy(&mut r, &mut std::io::sink());
    }

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub(crate) fn log(line: &str) {
        LINES.lock().unwrap().push(line.to_string());
    }

    pub(crate) fn logged(needle: &str) -> bool {
        LINES.lock().unwrap().iter().any(|l| l.contains(needle))
    }

    fn client(limit: usize) -> (HelperClient, Fake) {
        let fake = Fake::default();
        (HelperClient::with_limit(fake.clone(), limit, log), fake)
    }

    fn no() -> Obj {
        Obj::new()
    }

    #[test]
    fn a_call_stamps_the_caller_and_reads_to_its_last_record() {
        let (c, fake) = client(4);
        fake.add("c1", caps::RPC, 1, respond);
        let params = Obj::new().with("argv", vec!["ls".to_string()]);
        let meta = Obj::new().with("caller", "spoofed").with("deadline", 5i64);
        let mut call = c.call("c1", "echo", params.clone(), meta, "disp-1", &|| false).unwrap();
        let Some(Record::Result(o)) = call.next().unwrap() else { panic!() };
        assert_eq!(o.obj("params"), Some(&params));
        assert_eq!(o.obj("meta").unwrap().str("caller"), Some("disp-1"), "stamped over");
        assert_eq!(o.obj("meta").unwrap().int("deadline"), Some(5));
        assert_eq!(call.next().unwrap(), None, "done after the last record");
        drop(call);
        assert!(logged("helper c1: echo from disp-1: ok ("), "{:?}", LINES.lock().unwrap());

        let mut call = c.call("c1", "chatty", no(), no(), "cli", &|| false).unwrap();
        let mut recs = Vec::new();
        while let Some(rec) = call.next().unwrap() {
            recs.push(rec);
        }
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[2], Record::Exit(Exit::Code(3)));
        drop(call);
        assert!(logged("helper c1: chatty from cli: exit 3 ("));
    }

    #[test]
    fn no_bridge_and_no_rpc_cap_fail_before_any_stream() {
        let (c, fake) = client(4);
        fake.add("old", caps::TCP_FORWARD, 1, respond);
        assert!(matches!(c.call("none", "echo", no(), no(), "cli", &|| false), Err(CallError::NoBridge(_))));
        let Err(e) = c.call("old", "echo", no(), no(), "cli", &|| false) else { panic!() };
        assert!(matches!(e, CallError::Unsupported(_)));
        assert_eq!(e.failure().code, "unsupported");
        assert_eq!(c.offers("old", "run.start", "cli").unwrap(), false, "falls back like an unoffered method");
        assert!(logged("helper old: echo from cli: the helper in old has no RPC"));
    }

    #[test]
    fn dropping_or_cancelling_a_call_closes_its_stream() {
        let (c, fake) = client(4);
        fake.add("c2", caps::RPC, 1, respond);
        let mut call = c.call("c2", "hang", no(), no(), "cli", &|| false).unwrap();
        assert_eq!(call.next().unwrap(), Some(Record::Stdout(b"working".to_vec())));
        let cancel = call.canceller();
        let reader = std::thread::spawn(move || {
            let e = call.next().unwrap_err();
            (e.to_string(), call)
        });
        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
        let (why, call) = reader.join().unwrap();
        assert_eq!(why, "cancelled");
        drop(call);
        assert!(logged("helper c2: hang from cli: cancelled ("));
        // A dropped call cancels too, and its slot frees.
        let call = c.call("c2", "hang", no(), no(), "drop", &|| false).unwrap();
        drop(call);
        assert!(logged("helper c2: hang from drop: cancelled ("));
        assert!(c.inner.lanes.lock().unwrap().is_empty(), "slots freed");
    }

    #[test]
    fn calls_past_the_limit_queue_in_order_and_an_abandoned_one_leaves() {
        let (c, fake) = client(2);
        fake.add("c3", caps::RPC, 1, respond);
        let a = c.call("c3", "hang", no(), no(), "a", &|| false).unwrap();
        let b = c.call("c3", "hang", no(), no(), "b", &|| false).unwrap();
        // Third: queued until one ends.
        let started = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (c, started) = (c.clone(), Arc::clone(&started));
            std::thread::spawn(move || {
                let call = c.call("c3", "echo", no(), no(), "third", &|| false);
                started.store(true, Ordering::SeqCst);
                call.map(|mut call| call.next().unwrap())
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        assert!(!started.load(Ordering::SeqCst), "queued behind the limit");
        // A fourth gives up while queued.
        let t = Instant::now();
        let gone = c.call("c3", "echo", no(), no(), "fourth", &|| t.elapsed() > Duration::from_millis(100));
        assert!(matches!(gone, Err(CallError::Abandoned)));
        assert!(logged("helper c3: echo from fourth: abandoned in the queue"));
        assert!(!started.load(Ordering::SeqCst));
        drop(a);
        let answered = waiter.join().unwrap().unwrap();
        assert!(matches!(answered, Some(Record::Result(_))));
        assert_eq!(c.inner.lanes.lock().unwrap()["c3"].active, 1, "b still runs");
        drop(b);
        assert!(c.inner.lanes.lock().unwrap().is_empty());
    }

    #[test]
    fn queue_rule() {
        let lane = Lane { active: 1, queue: VecDeque::from([7, 8]) };
        assert!(may_start(&lane, 7, 2));
        assert!(!may_start(&lane, 8, 2), "not first in line");
        assert!(!may_start(&lane, 7, 1), "no slot");
    }

    #[test]
    fn offers_caches_rpc_methods_per_attach() {
        let (c, fake) = client(4);
        fake.add("c4", caps::RPC, 1, respond);
        let before = ASKED.load(Ordering::SeqCst);
        assert!(c.offers("c4", "run.start", "cli").unwrap());
        assert!(c.offers("c4", "run.start@2", "cli").unwrap());
        assert!(!c.offers("c4", "run.start@1", "cli").unwrap());
        assert!(!c.offers("c4", "run.wait", "cli").unwrap());
        assert_eq!(ASKED.load(Ordering::SeqCst) - before, 1, "asked once");
        // A new bridge attach asks again.
        fake.add("c4", caps::RPC, 2, respond);
        assert!(c.offers("c4", "rpc.methods", "cli").unwrap());
        assert_eq!(ASKED.load(Ordering::SeqCst) - before, 2);
        assert!(matches!(c.offers("gone", "x", "cli"), Err(CallError::NoBridge(_))));
    }

    #[test]
    fn offered_matches_names_and_versions() {
        let list = ["rpc.methods@1".to_string(), "run.start@2".to_string()];
        assert!(offered(&list, "run.start"));
        assert!(offered(&list, "run.start@2"));
        assert!(!offered(&list, "run.start@1"));
        assert!(!offered(&list, "run"));
    }

    #[test]
    fn params_convert_within_the_rpc_subset() {
        let v = json!({"argv": ["a", "b"], "cwd": "/w", "n": -3, "tty": true, "x": null, "env": {"A": "1"}});
        let o = to_obj(&v).unwrap();
        assert_eq!(from_json(&Json::Obj(o)), v);
        assert_eq!(to_obj(&Value::Null).unwrap(), Obj::new());
        assert!(to_obj(&json!([])).is_err());
        assert!(to_obj(&json!({"f": 1.5})).unwrap_err().contains("integers"));
        assert!(to_obj(&json!({"a": [1]})).unwrap_err().contains("strings only"));
        assert!(to_obj(&json!({"a": {"b": {}}})).is_ok(), "params at 2, then 3 and 4");
        assert!(to_obj(&json!({"a": {"b": {"c": {}}}})).unwrap_err().contains("nest"));
        // What the wire round-trips: encodable and decodable by the helper's codec.
        let call = Record::Call(rpc::Call { method: "m".into(), params: to_obj(&json!({"a": {"b": {}}})).unwrap(), meta: Obj::new() });
        let bytes = call.encode();
        assert_eq!(rpc::read_record(&mut &bytes[..]).unwrap(), Some(call));
    }

    #[test]
    fn helper_call_params() {
        let p = parse(json!({"instance": "web", "method": "rpc.methods"})).unwrap();
        assert_eq!((p.instance.as_str(), p.method.as_str(), p.params), ("web", "rpc.methods", Obj::new()));
        assert_eq!(parse(json!({"instance": "web"})).unwrap_err().code, "invalid");
        assert_eq!(parse(json!({"instance": "", "method": "m"})).unwrap_err().code, "invalid");
        assert_eq!(parse(json!({"instance": "w", "method": "m", "params": [1]})).unwrap_err().code, "invalid");
    }

    /// Internal, like `bridges.ensure`: not in the npm typings.
    #[test]
    fn helper_call_is_not_in_the_npm_typings() {
        use crate::serve::dts::prop_type;
        assert!(prop_type("ApiMethods", "helper.call").is_none());
        assert!(prop_type("ApiEvents", "helper.record").is_none());
    }

    #[test]
    fn records_relay_as_notes_then_one_answer() {
        let r = |rec: Record| relay(Some(4), &rec).unwrap();
        assert_eq!(r(Record::Stdout(b"hi".to_vec())), Relay::Note(json!({"request": 4, "record": "stdout", "data": "hi"})));
        assert_eq!(
            r(Record::Event(Obj::new().with("state", "done"))),
            Relay::Note(json!({"request": 4, "record": "event", "data": {"state": "done"}}))
        );
        assert_eq!(r(Record::Result(Obj::new().with("id", "r1"))), Relay::Answer(json!({"result": {"id": "r1"}})));
        assert_eq!(r(Record::error("bad-request", "x")), Relay::Answer(json!({"error": {"code": "bad-request", "message": "x"}})));
        assert_eq!(r(Record::Exit(Exit::Signal(9))), Relay::Answer(json!({"signal": 9})));
        assert!(relay(None, &Record::StdinEof).is_err());
    }

    /// A connection: records notifications, hangs up on cue.
    #[derive(Default)]
    struct Conn1 {
        notes: Mutex<Vec<(String, Value)>>,
        hang_up: AtomicBool,
    }

    impl Peer for Conn1 {
        fn notify(&self, method: &str, params: Value) -> io::Result<()> {
            self.notes.lock().unwrap().push((method.to_string(), params));
            Ok(())
        }
        fn hung_up(&self, tick: Duration) -> bool {
            std::thread::sleep(tick);
            self.hang_up.load(Ordering::SeqCst)
        }
        fn draining(&self) -> bool {
            false
        }
    }

    fn call_params(method: &str) -> CallParams {
        CallParams { instance: "web".into(), method: method.into(), params: Obj::new() }
    }

    #[test]
    fn serve_relays_records_and_answers_the_last() {
        let (c, fake) = client(4);
        fake.add("c5", caps::RPC, 1, respond);
        let peer = Conn1::default();
        let Outcome::Answer(Ok(answer)) = serve(&c, "c5", call_params("chatty"), Some(9), "cli", &peer) else { panic!() };
        assert_eq!(answer, json!({"exit": 3}));
        let notes = peer.notes.lock().unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes.iter().all(|(m, p)| m == "helper.record" && p["request"] == 9));
        assert_eq!(notes[1].1["data"], json!({"state": "running"}));
        drop(notes);

        let Outcome::Answer(Ok(answer)) = serve(&c, "c5", call_params("nope"), Some(1), "cli", &peer) else { panic!() };
        assert_eq!(answer["error"]["code"], "unknown-method");
        let Outcome::Answer(Err(f)) = serve(&c, "none", call_params("echo"), Some(1), "cli", &peer) else { panic!() };
        assert_eq!(f.code, "not-found");
    }

    #[test]
    fn serve_cancels_on_a_hang_up() {
        let (c, fake) = client(4);
        fake.add("c6", caps::RPC, 1, respond);
        let peer = Arc::new(Conn1::default());
        let hanger = Arc::clone(&peer);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            hanger.hang_up.store(true, Ordering::SeqCst);
        });
        assert!(matches!(serve(&c, "c6", call_params("hang"), Some(1), "cli", &*peer), Outcome::Gone));
        assert!(logged("helper c6: hang from cli: cancelled ("));
        assert!(c.inner.lanes.lock().unwrap().is_empty());
    }
}
