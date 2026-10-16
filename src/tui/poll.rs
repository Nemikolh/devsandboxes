//! The dashboard's per-request read from the host daemon
//! (docs/architecture/dashboard/overview.md, *Threads*): the expanded instances' processes
//! (`instances.procs`), per dashboard since each expands its own. The event
//! loop decides when (`PROC_TICK`, an expand, a snapshot after a change) and
//! keeps at most one request outstanding; this worker only asks and answers.
//! The snapshot isn't read here: the daemon pushes it to `tui::daemon`'s
//! watch (`instances.snapshot`).
//!
//! One thread with its own connection, so a procs fetch (a `docker exec` per
//! instance) never waits behind `tui::daemon`'s long-lived connection, nor
//! holds up its notifications. No subscribe.
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
//! A command channel plus a short-joining `Drop`; nothing is
//! ever written to stderr, failures are [`FetchError`]s.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::data::FetchError;
use super::procs::{self, ProcState};
use super::remote;
use crate::serve::api::ProcsAnswer;
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::proto::{Response, Version};

/// How long [`Drop`] waits for the threads before detaching them (one may
/// be mid-call): the terminal restore mustn't hang on the daemon.
const JOIN_TIMEOUT: Duration = Duration::from_millis(300);

/// From the worker to the event loop: one per request, in order.
#[derive(Debug)]
pub enum PollUpdate {
    /// `instances.procs`'s answer as the tree's states, by instance name.
    Procs(Result<BTreeMap<String, ProcState>, FetchError>),
}

/// How the thread gets its connection: [`connect`], or a test daemon's.
type Connector = Arc<dyn Fn() -> Result<Conn> + Send + Sync>;

/// Handle to the poll worker. Never blocks the UI thread.
pub struct PollWorker {
    procs: Option<Sender<Vec<String>>>,
    updates: Receiver<PollUpdate>,
    handles: Vec<JoinHandle<()>>,
}

impl PollWorker {
    /// Spawn the worker.
    pub fn spawn() -> PollWorker {
        Self::spawn_with(Arc::new(connect))
    }

    fn spawn_with(connector: Connector) -> PollWorker {
        let (utx, updates) = mpsc::channel();
        let (ptx, prx) = mpsc::channel::<Vec<String>>();
        let mut handles = Vec::new();
        let spawned = std::thread::Builder::new().name("tui-poll-procs".into()).spawn(move || {
            lane(&*connector, prx, |conn, names| {
                let answer = procs_by_machine(&names, |machine, names| {
                    let (method, params) = procs_call(names, machine);
                    call(conn, method, params).and_then(procs_answer)
                });
                (answer.is_ok_or_api(), PollUpdate::Procs(answer.into_result()))
            }, |e| PollUpdate::Procs(Err(e)), &utx)
        });
        handles.extend(spawned.ok());
        PollWorker { procs: Some(ptx), updates, handles }
    }

    /// Ask for `names`' processes (instance keys, `remote::qualify`). Never
    /// blocks; the answer is one [`PollUpdate::Procs`] for them all.
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
        // Closing the channel ends an idle thread at once; one mid-call is
        // left to die with the process, which closes its socket.
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

/// The thread's loop: per request, (re)connect if needed, then `ask`, which
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

/// The API call for `names`' processes on `machine` (`None`: here).
fn procs_call(names: &[&str], machine: Option<&str>) -> (&'static str, Value) {
    let mut params = json!({ "instances": names });
    if let Some(m) = machine {
        params["machine"] = m.into();
    }
    ("instances.procs", params)
}

/// `keys`' processes, one `ask` per machine in turn (docs/architecture/daemon/machines.md, *Forwarded requests*:
/// a machine's link takes one request per machine), keyed back by key.
/// Only this host's (or a single machine's) keys: that one answer as is.
/// Several: a machine whose call the daemon refused gets that reason as
/// each of its instances' row, and a lost connection is the whole answer.
fn procs_by_machine(
    keys: &[String],
    mut ask: impl FnMut(Option<&str>, &[&str]) -> Answer<BTreeMap<String, ProcState>>,
) -> Answer<BTreeMap<String, ProcState>> {
    let mut groups: BTreeMap<Option<&str>, Vec<&str>> = BTreeMap::new();
    for key in keys {
        let (machine, name) = remote::split(key);
        groups.entry(machine).or_default().push(name);
    }
    let single = groups.len() == 1;
    let mut out = BTreeMap::new();
    for (machine, names) in groups {
        match ask(machine, &names) {
            Answer::Ok(states) => {
                out.extend(states.into_iter().map(|(name, state)| (remote::qualify(machine, &name), state)));
            }
            Answer::Api(why) if !single => {
                let row = ProcState::Message(format!("(processes unavailable: {why})"));
                out.extend(names.iter().map(|name| (remote::qualify(machine, name), row.clone())));
            }
            other => return other,
        }
    }
    Answer::Ok(out)
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
        assert_eq!(procs_call(&["a", "b"], None), ("instances.procs", json!({"instances": ["a", "b"]})));
        assert_eq!(procs_call(&["a"], Some("box")).1, json!({"instances": ["a"], "machine": "box"}));
    }

    #[test]
    fn procs_go_one_request_per_machine_keyed_back() {
        let keys: Vec<String> = ["web", "box/web", "box/api", "down/x"].map(String::from).to_vec();
        let mut asked = Vec::new();
        let answer = procs_by_machine(&keys, |machine, names| {
            asked.push((machine.map(String::from), names.iter().map(|n| n.to_string()).collect::<Vec<_>>()));
            match machine {
                Some("down") => Answer::Api("down: timed out".into()),
                _ => Answer::Ok(names.iter().map(|n| (n.to_string(), ProcState::Message(format!("{machine:?}")))).collect()),
            }
        });
        assert_eq!(
            asked,
            [
                (None, vec!["web".to_string()]),
                (Some("box".into()), vec!["web".into(), "api".into()]),
                (Some("down".into()), vec!["x".into()]),
            ]
        );
        let Answer::Ok(states) = answer else { panic!("not ok") };
        assert_eq!(states["web"], ProcState::Message("None".into()));
        assert_eq!(states["box/web"], ProcState::Message("Some(\"box\")".into()));
        assert_eq!(states["down/x"], ProcState::Message("(processes unavailable: down: timed out)".into()));
        // Alone, a refusal is the answer, as before machines.
        let alone = procs_by_machine(&keys[..1], |_, _| Answer::Api("no".into()));
        assert!(matches!(alone, Answer::Api(why) if why == "no"));
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

    /// No daemon: a request answers why at once (no start, no hang).
    #[test]
    fn unreachable_daemon_answers_errors() {
        let worker = PollWorker::spawn_with(Arc::new(|| anyhow::bail!("nothing answers")));
        worker.request_procs(vec!["a".into()]);
        let PollUpdate::Procs(Err(e)) = wait_update(&worker) else { panic!("expected a procs error") };
        assert_eq!(e, FetchError { unreachable: true, reason: "nothing answers".into() });
    }

    /// Against a real (test) daemon: the call goes out, the daemon's error
    /// comes back as such, and the connection is kept for the next one.
    /// `instances.procs` can't be refused without reading state, so the
    /// lane runs a call that can: a missing dir, refused before any state
    /// or runtime is touched.
    #[test]
    fn a_daemon_error_is_an_api_error_and_keeps_the_connection() {
        use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
        let sock = scratch("tui-poll");
        let o = opts(1, 2000);
        let version = o.version.clone();
        let daemon = spawn_daemon(&sock, o);
        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&connects);
        let connector = move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            client::connect_with(&sock, "tui", &version, Duration::from_secs(5), &|_, _| Ok(()))
        };
        let absent = std::env::temp_dir().join(format!("dsv-tui-poll-absent-{}", std::process::id()));
        let (tx, rx) = mpsc::channel();
        let (utx, updates) = mpsc::channel();
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        drop(tx);
        lane(&connector, rx, |conn, ()| {
            let answer = call(conn, "instances.list", json!({ "dir": absent.to_string_lossy() }));
            (answer.is_ok_or_api(), PollUpdate::Procs(answer.and_then(|_| Answer::Ok(BTreeMap::new())).into_result()))
        }, |e| PollUpdate::Procs(Err(e)), &utx);
        for _ in 0..2 {
            let PollUpdate::Procs(Err(e)) = updates.try_recv().unwrap() else { panic!("expected an error") };
            assert!(!e.unreachable, "{e:?}");
            assert!(e.reason.starts_with("no directory"), "{e:?}");
        }
        assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = daemon.join();
    }
}
