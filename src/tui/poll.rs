//! The dashboard's reads from the host daemon (docs/dashboard-client.md, F2):
//! the snapshot (`instances.list`) and the expanded instances' processes
//! (`instances.procs`). The event loop decides when (its 2 s tick, the
//! `StatsClock`, `PROC_TICK`, follow-ups) and keeps at most one request of
//! each kind outstanding; this worker only asks and answers.
//!
//! Two threads, one connection each, so a 1.5 s `docker stats` never holds
//! up a procs fetch (a `docker exec` per instance) or the other way round,
//! and neither waits behind `tui::daemon`'s long-lived connection. No
//! subscribe: `tui::daemon` relays `instances.changed`.
//!
//! Connecting never starts a daemon: it waits up to `START_TIMEOUT` for one
//! to answer. Starting one is `tui::daemon`'s job (it lazy-starts at once
//! and after a crash, and after a handoff waits for the newer client's
//! successor). Without a `closing` hint (only subscribers get one) a lost
//! connection here can't tell a handoff from a crash, and a start of our own
//! after a handoff would race the successor with an older daemon. Nothing
//! answering is an error answer, so the event loop never blocks and the
//! next request tries again.
//!
//! A command channel per thread plus a short-joining `Drop`; nothing is
//! ever written to stderr, failures are [`FetchError`]s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::data::{Depth, FetchError};
use super::procs::{self, ProcState};
use crate::serve::api::{InstancesList, ProcsAnswer};
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::proto::{Response, Version};

/// How long [`Drop`] waits for the threads before detaching them (one may
/// be mid-call): the terminal restore mustn't hang on the daemon.
const JOIN_TIMEOUT: Duration = Duration::from_millis(300);

/// From the worker to the event loop: one per request, in order per kind.
#[derive(Debug)]
pub enum PollUpdate {
    /// `instances.list`'s answer, `stats` set from the depth asked for.
    Snapshot(Result<InstancesList, FetchError>),
    /// `instances.procs`'s answer as the tree's states, by instance name.
    Procs(Result<BTreeMap<String, ProcState>, FetchError>),
}

/// How a thread gets its connection: [`connect`], or a test daemon's.
type Connector = Arc<dyn Fn() -> Result<Conn> + Send + Sync>;

/// Handle to the poll worker. Never blocks the UI thread.
pub struct PollWorker {
    snapshots: Option<Sender<Depth>>,
    procs: Option<Sender<Vec<String>>>,
    updates: Receiver<PollUpdate>,
    handles: Vec<JoinHandle<()>>,
}

impl PollWorker {
    /// Spawn the worker for the dashboard of config root `dir`.
    pub fn spawn(dir: PathBuf) -> PollWorker {
        Self::spawn_with(super::daemon::root_dir(dir), Arc::new(connect))
    }

    fn spawn_with(dir: PathBuf, connector: Connector) -> PollWorker {
        let (utx, updates) = mpsc::channel();
        let (stx, srx) = mpsc::channel::<Depth>();
        let (ptx, prx) = mpsc::channel::<Vec<String>>();
        let mut handles = Vec::new();
        let (c, u) = (Arc::clone(&connector), utx.clone());
        let spawned = std::thread::Builder::new().name("tui-poll-list".into()).spawn(move || {
            lane(&*c, srx, |conn, depth| {
                let (method, params) = list_call(&dir, depth);
                let answer = call(conn, method, params).and_then(|r| list_answer(r, depth));
                (answer.is_ok_or_api(), PollUpdate::Snapshot(answer.into_result()))
            }, |e| PollUpdate::Snapshot(Err(e)), &u)
        });
        handles.extend(spawned.ok());
        let spawned = std::thread::Builder::new().name("tui-poll-procs".into()).spawn(move || {
            lane(&*connector, prx, |conn, names| {
                let (method, params) = procs_call(&names);
                let answer = call(conn, method, params).and_then(procs_answer);
                (answer.is_ok_or_api(), PollUpdate::Procs(answer.into_result()))
            }, |e| PollUpdate::Procs(Err(e)), &utx)
        });
        handles.extend(spawned.ok());
        PollWorker { snapshots: Some(stx), procs: Some(ptx), updates, handles }
    }

    /// Ask for a snapshot down to `depth`. Never blocks; the answer is a
    /// [`PollUpdate::Snapshot`].
    pub fn request_snapshot(&self, depth: Depth) {
        if let Some(tx) = &self.snapshots {
            let _ = tx.send(depth);
        }
    }

    /// Ask for `names`' processes. Never blocks; the answer is a
    /// [`PollUpdate::Procs`].
    pub fn request_procs(&self, names: Vec<String>) {
        if let Some(tx) = &self.procs {
            let _ = tx.send(names);
        }
    }

    /// Next pending update, or `None`. Non-blocking.
    pub fn try_recv(&self) -> Option<PollUpdate> {
        self.updates.try_recv().ok()
    }
}

impl Drop for PollWorker {
    fn drop(&mut self) {
        // Closing the channels ends idle threads at once; one mid-call is
        // left to die with the process, which closes its socket.
        self.snapshots.take();
        self.procs.take();
        let deadline = Instant::now() + JOIN_TIMEOUT;
        while !self.handles.iter().all(JoinHandle::is_finished) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        for handle in std::mem::take(&mut self.handles) {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

/// Connect without ever starting a daemon (see the module doc).
fn connect() -> Result<Conn> {
    let dir = endpoint::socket_dir()?;
    client::connect_with(&dir, "tui", &Version::current(), client::START_TIMEOUT, &|_, _| Ok(()))
}

/// How one call ended: an answer, the daemon's error, or no daemon.
enum Answer<T> {
    Ok(T),
    /// The daemon answered with an error (or one this build can't read).
    Api(String),
    /// No answer: the connection broke or never was.
    Lost(String),
}

impl<T> Answer<T> {
    fn and_then<U>(self, f: impl FnOnce(T) -> Answer<U>) -> Answer<U> {
        match self {
            Answer::Ok(v) => f(v),
            Answer::Api(e) => Answer::Api(e),
            Answer::Lost(e) => Answer::Lost(e),
        }
    }

    /// Whether the connection is still good.
    fn is_ok_or_api(&self) -> bool {
        !matches!(self, Answer::Lost(_))
    }

    fn into_result(self) -> Result<T, FetchError> {
        match self {
            Answer::Ok(v) => Ok(v),
            Answer::Api(reason) => Err(FetchError { unreachable: false, reason }),
            Answer::Lost(reason) => Err(FetchError { unreachable: true, reason }),
        }
    }
}

/// One thread's loop: per request, (re)connect if needed, then `ask`, which
/// says whether the connection survived. A failed connect answers
/// `failed(why)`; the next request tries again.
fn lane<R>(
    connector: &(dyn Fn() -> Result<Conn> + Send + Sync),
    rx: Receiver<R>,
    mut ask: impl FnMut(&mut Conn, R) -> (bool, PollUpdate),
    failed: impl Fn(FetchError) -> PollUpdate,
    utx: &Sender<PollUpdate>,
) {
    let mut conn: Option<Conn> = None;
    while let Ok(req) = rx.recv() {
        if conn.is_none() {
            match connector() {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    let _ = utx.send(failed(FetchError { unreachable: true, reason: format!("{e:#}") }));
                    continue;
                }
            }
        }
        let Some(c) = conn.as_mut() else { continue };
        let (alive, update) = ask(c, req);
        if !alive {
            conn = None;
        }
        if utx.send(update).is_err() {
            return;
        }
    }
}

/// One call; an I/O failure is [`Answer::Lost`], an error response
/// [`Answer::Api`] with its message.
fn call(conn: &mut Conn, method: &str, params: Value) -> Answer<Value> {
    match conn.call(method, params) {
        Ok(Response { error: Some(e), .. }) => Answer::Api(e.message),
        Ok(r) => Answer::Ok(r.result.unwrap_or(Value::Null)),
        Err(e) => Answer::Lost(format!("{e:#}")),
    }
}

/// The wire name of a [`Depth`] (`instances.list`'s `depth`, F1).
fn depth_name(depth: Depth) -> &'static str {
    match depth {
        Depth::Disk => "disk",
        Depth::Listing => "listing",
        Depth::Full => "full",
    }
}

/// The API call for a snapshot of config root `dir` down to `depth`.
fn list_call(dir: &Path, depth: Depth) -> (&'static str, Value) {
    ("instances.list", json!({ "dir": dir.to_string_lossy(), "depth": depth_name(depth) }))
}

/// `instances.list`'s result as the dashboard's snapshot. The wire doesn't
/// carry `stats` (it says whether cpu/mem were sampled, so `set_snapshot`
/// knows to carry the last ones over): it's the depth asked for.
fn list_answer(result: Value, depth: Depth) -> Answer<InstancesList> {
    match serde_json::from_value::<InstancesList>(result) {
        Ok(mut list) => {
            list.snapshot.stats = depth == Depth::Full;
            Answer::Ok(list)
        }
        Err(e) => Answer::Api(format!("bad instances.list answer: {e}")),
    }
}

/// The API call for `names`' processes.
fn procs_call(names: &[String]) -> (&'static str, Value) {
    ("instances.procs", json!({ "instances": names }))
}

/// `instances.procs`'s result as the tree's states.
fn procs_answer(result: Value) -> Answer<BTreeMap<String, ProcState>> {
    match serde_json::from_value::<ProcsAnswer>(result) {
        Ok(answer) => Answer::Ok(answer.into_iter().map(|(name, entry)| (name, procs::proc_state(entry))).collect()),
        Err(e) => Answer::Api(format!("bad instances.procs answer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_are_what_the_api_parses() {
        assert_eq!(
            list_call(Path::new("/cfg"), Depth::Disk),
            ("instances.list", json!({"dir": "/cfg", "depth": "disk"}))
        );
        assert_eq!(list_call(Path::new("/cfg"), Depth::Listing).1["depth"], "listing");
        assert_eq!(list_call(Path::new("/cfg"), Depth::Full).1["depth"], "full");
        assert_eq!(procs_call(&["a".into(), "b".into()]), ("instances.procs", json!({"instances": ["a", "b"]})));
    }

    /// The depth asked for decides `stats`, whatever the wire says; the
    /// rest is the snapshot as serialized, `thread_children` included.
    #[test]
    fn list_answers_take_stats_from_the_depth() {
        let snapshot = crate::snapshot::Snapshot {
            instances: Vec::new(),
            sandboxes: Vec::new(),
            services: Vec::new(),
            sandbox_count: 2,
            runtime_name: "docker".into(),
            runtime_version: Some("24".into()),
            collected_at: Instant::now(),
            stats: true,
            error: None,
        };
        let children = [("d-id".to_string(), [("k".to_string(), "web-k".to_string())].into())].into();
        let wire = serde_json::to_value(InstancesList { snapshot, thread_children: children }).unwrap();
        let Answer::Ok(listing) = list_answer(wire.clone(), Depth::Listing) else { panic!("not parsed") };
        assert!(!listing.snapshot.stats);
        assert_eq!(listing.snapshot.sandbox_count, 2);
        assert_eq!(listing.thread_children["d-id"]["k"], "web-k");
        let Answer::Ok(full) = list_answer(wire, Depth::Full) else { panic!("not parsed") };
        assert!(full.snapshot.stats);
        assert!(matches!(list_answer(json!([1]), Depth::Full), Answer::Api(_)));
    }

    #[test]
    fn procs_answers_become_proc_states() {
        let wire = json!({
            "up": {"processes": [{"pid": "1", "ppid": "0", "args": "init"}], "signalable": true, "runs": []},
            "gone": {"error": "no instance `gone`"},
        });
        let Answer::Ok(states) = procs_answer(wire) else { panic!("not parsed") };
        assert!(matches!(&states["up"], ProcState::Rows { rows, signalable: true } if rows.len() == 1));
        assert_eq!(states["gone"], ProcState::Message("(processes unavailable: no instance `gone`)".into()));
    }

    fn wait_update(worker: &PollWorker) -> PollUpdate {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(u) = worker.try_recv() {
                return u;
            }
            assert!(Instant::now() < deadline, "no update");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// No daemon: each request answers why at once (no start, no hang), on
    /// both lanes.
    #[test]
    fn unreachable_daemon_answers_errors() {
        let worker = PollWorker::spawn_with(PathBuf::from("/cfg"), Arc::new(|| anyhow::bail!("nothing answers")));
        worker.request_snapshot(Depth::Disk);
        let PollUpdate::Snapshot(Err(e)) = wait_update(&worker) else { panic!("expected a snapshot error") };
        assert_eq!(e, FetchError { unreachable: true, reason: "nothing answers".into() });
        worker.request_procs(vec!["a".into()]);
        let PollUpdate::Procs(Err(e)) = wait_update(&worker) else { panic!("expected a procs error") };
        assert!(e.unreachable);
    }

    /// Against a real (test) daemon: the call goes out, the daemon's error
    /// comes back as such, and the connection is kept for the next one. A
    /// missing dir is refused before any state or runtime is touched.
    #[test]
    fn a_daemon_error_is_an_api_error_and_keeps_the_connection() {
        use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
        let sock = scratch("tui-poll");
        let o = opts(1, 2000);
        let version = o.version.clone();
        let daemon = spawn_daemon(&sock, o);
        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&connects);
        let worker = PollWorker::spawn_with(
            std::env::temp_dir().join(format!("dsv-tui-poll-absent-{}", std::process::id())),
            Arc::new(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                client::connect_with(&sock, "tui", &version, Duration::from_secs(5), &|_, _| Ok(()))
            }),
        );
        for _ in 0..2 {
            worker.request_snapshot(Depth::Disk);
            let PollUpdate::Snapshot(Err(e)) = wait_update(&worker) else { panic!("expected an error") };
            assert!(!e.unreachable, "{e:?}");
            assert!(e.reason.starts_with("no directory"), "{e:?}");
        }
        assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(worker);
        let _ = daemon.join();
    }
}
