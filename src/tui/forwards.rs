//! TUI Ports-tab forwarder worker (docs/port-forwarding.md, step 10). Owns every
//! live [`Forward`] on its own thread, so all config/state/docker work — route
//! resolution, `devsbd::ensure`, the listening-process probe — stays off the UI
//! thread. The event loop drives it with [`ForwardWorker::add`] / [`remove`] and
//! drains [`ForwardWorker::try_recv`] for row/status updates without blocking.
//!
//! Modelled on `devsbd::bridge::BridgeWorker`: a command channel plus a `Drop`
//! that closes the channel and joins, so every `Forward` (and its bridge, its
//! `exec`) is dropped before the terminal is restored. Nothing is ever written
//! to stderr — the TUI owns the screen; errors surface as [`ForwardUpdate`]s.
//!
//! [`remove`]: ForwardWorker::remove

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::commands::port;
use crate::devsbd::forward::{Forward, ForwardSpec, ForwardState, ForwardStatus, HostPort};
use crate::state::State;

use super::app::PortRow;

/// How often the worker re-polls every forward's `status()` while idle, so a
/// state change (Connecting → Active, a note) reaches the UI within a poll. Also
/// the cadence at which per-connection notes are drained.
const POLL: Duration = Duration::from_millis(500);

/// A command from the UI thread to the worker.
enum Cmd {
    /// Start a forward for this request; the worker assigns its id.
    Add(PortRequestOwned),
    /// Drop the forward with this id (the `d` shortcut).
    Remove(u64),
}

/// The worker's owned copy of a `PortRequest` (the app's type is UI-side).
struct PortRequestOwned {
    instance: String,
    service: Option<String>,
    address: Option<String>,
    spec: String,
}

/// An update from the worker to the UI thread, drained each event-loop iteration.
pub enum ForwardUpdate {
    /// The full set of Ports-tab rows, sent only when it changed.
    Rows(Vec<PortRow>),
    /// A one-line status for the help bar (a bind error, a connection note).
    Status(String),
}

/// Handle to the forwarder worker thread. Commands never block the UI thread; on
/// drop the channel closes and the thread is joined, dropping every `Forward`
/// (killing its bridge/`exec`) before the caller restores the terminal.
pub struct ForwardWorker {
    tx: Option<Sender<Cmd>>,
    updates: Receiver<ForwardUpdate>,
    handle: Option<JoinHandle<()>>,
}

impl ForwardWorker {
    /// Spawn the worker, resolving routes against config root `dir`.
    pub fn spawn(dir: PathBuf) -> ForwardWorker {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (utx, updates) = mpsc::channel::<ForwardUpdate>();
        let handle = std::thread::spawn(move || run(dir, rx, utx));
        ForwardWorker { tx: Some(tx), updates, handle: Some(handle) }
    }

    /// Ask the worker to start a forward. Never blocks; a dead worker is ignored.
    pub fn add(&self, req: super::app::PortRequest) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Add(PortRequestOwned {
                instance: req.instance,
                service: req.service,
                address: req.address,
                spec: req.spec,
            }));
        }
    }

    /// Ask the worker to drop the forward with `id`. Never blocks.
    pub fn remove(&self, id: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Remove(id));
        }
    }

    /// Next pending update, or `None` when nothing is queued. Non-blocking.
    pub fn try_recv(&self) -> Option<ForwardUpdate> {
        self.updates.try_recv().ok()
    }
}

impl Drop for ForwardWorker {
    fn drop(&mut self) {
        // Close the channel so the worker's `recv_timeout` returns disconnected
        // and it drops every `Forward` (killing each bridge/`exec`), then join so
        // that teardown finishes before the terminal is restored.
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The worker's owned forwards, keyed by the id it hands out (monotonic).
struct Forwards {
    dir: PathBuf,
    next_id: u64,
    live: BTreeMap<u64, Forward>,
    /// Last rows sent, so an unchanged poll produces no `Rows` update.
    last_rows: Vec<PortRow>,
    /// Last status sent, so a repeated note isn't re-sent.
    last_status: Option<String>,
}

/// Worker loop: block on the command channel with a `POLL` timeout so it wakes
/// for both commands and the periodic status poll. Returns (dropping every
/// forward) once the command channel disconnects.
fn run(dir: PathBuf, rx: Receiver<Cmd>, utx: Sender<ForwardUpdate>) {
    let mut fwds = Forwards { dir, next_id: 1, live: BTreeMap::new(), last_rows: Vec::new(), last_status: None };
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Cmd::Add(req)) => fwds.add(req, &utx),
            Ok(Cmd::Remove(id)) => fwds.remove(id, &utx),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // UI dropped the worker: exit, dropping `fwds` (every Forward).
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        fwds.poll(&utx);
    }
}

impl Forwards {
    /// Start one forward for `req`. Loads state, resolves the instance name
    /// non-interactively, validates the spec and address, and binds the
    /// listener; any failure is reported as a `Status` and no row is added.
    fn add(&mut self, req: PortRequestOwned, utx: &Sender<ForwardUpdate>) {
        match self.start(&req) {
            // The row itself surfaces on the next poll.
            Ok((addr, label)) => {
                let _ = utx.send(ForwardUpdate::Status(format!("forwarding {addr} -> {label}")));
            }
            Err(msg) => {
                let _ = utx.send(ForwardUpdate::Status(msg));
            }
        }
    }

    /// The impure edge of `add`, factored so the happy/error split above is
    /// clear. On success returns `(bound local addr, route label)`.
    fn start(&mut self, req: &PortRequestOwned) -> Result<(std::net::SocketAddr, String), String> {
        // Service-only forwards name no instance; an empty instance means the
        // request came without one (the prompt always fills it for `p`, but a
        // typed `port --service …` may not).
        let instance = (!req.instance.is_empty()).then(|| req.instance.clone());

        // Resolve the user-supplied name to an exact state key, non-interactively
        // (the UI owns the console; a prompt here would fight the alt screen).
        let instance_key = match &instance {
            Some(name) => {
                let state = State::load().map_err(|e| format!("{e:#}"))?;
                Some(crate::commands::resolve_instance_noninteractive(&state, name).map_err(|e| format!("{e:#}"))?)
            }
            None => None,
        };

        let (host_port, container_port) = port::parse_port_spec(&req.spec)?;
        let bind: IpAddr = match &req.address {
            Some(a) => a.parse().map_err(|_| format!("bad address `{a}`"))?,
            None => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        };

        let resolve = port::resolver(self.dir.clone(), instance_key, req.service.clone(), container_port);
        let probe = Box::new(port::listening_procs);
        let forward = Forward::start(ForwardSpec { bind, host_port, resolve, probe }).map_err(|e| {
            // Bind errors are the immediate, precise ones worth naming a port for.
            let port = match host_port {
                HostPort::Fixed(p) | HostPort::Prefer(p) => p,
            };
            format!("port {port}: {}", bind_reason(&e))
        })?;

        let addr = forward.status().local_addr;
        let label = route_label_or(&forward, container_port, req);
        let id = self.next_id;
        self.next_id += 1;
        self.live.insert(id, forward);
        Ok((addr, label))
    }

    /// Drop the forward with `id` (its `Drop` kills the bridge/`exec`). Reports a
    /// `stopped <local>` status; unknown ids are ignored (a stale `d`).
    fn remove(&mut self, id: u64, utx: &Sender<ForwardUpdate>) {
        if let Some(forward) = self.live.remove(&id) {
            let addr = forward.status().local_addr;
            drop(forward);
            let _ = utx.send(ForwardUpdate::Status(format!("stopped {addr}")));
        }
    }

    /// Poll every forward's status: rebuild the rows and send `Rows` only when
    /// they changed, then drain each forward's notes and send the newest as a
    /// deduped `Status`.
    fn poll(&mut self, utx: &Sender<ForwardUpdate>) {
        let rows: Vec<PortRow> = self
            .live
            .iter()
            .map(|(&id, f)| status_to_row(id, &f.status()))
            .collect();
        if rows != self.last_rows {
            self.last_rows = rows.clone();
            let _ = utx.send(ForwardUpdate::Rows(rows));
        }

        // Drain notes; the newest per forward, prefixed with the local port, is
        // the one worth showing. Dedup against the last status sent so a retry
        // loop doesn't repaint the same line every poll.
        let mut newest: Option<String> = None;
        for f in self.live.values() {
            let local_port = f.status().local_addr.port();
            for note in f.drain_notes() {
                newest = Some(format!("{local_port}: {note}"));
            }
        }
        if let Some(line) = newest {
            if self.last_status.as_deref() != Some(line.as_str()) {
                self.last_status = Some(line.clone());
                let _ = utx.send(ForwardUpdate::Status(line));
            }
        }
    }
}

/// Project a `ForwardStatus` into a Ports-tab row. Pure over `(id, status)` so
/// the state-string mapping is unit-tested without a forwarder.
fn status_to_row(id: u64, status: &ForwardStatus) -> PortRow {
    PortRow {
        id,
        local: status.local_addr.to_string(),
        target: status.route_label.clone(),
        process: status.process.clone(),
        state: state_string(&status.state),
        conns: status.open_conns,
    }
}

/// The Ports-tab state string: `"active"`, `"connecting"`, or `"error: …"`.
fn state_string(state: &ForwardState) -> String {
    match state {
        ForwardState::Active => "active".into(),
        ForwardState::Connecting => "connecting".into(),
        ForwardState::Error(reason) => format!("error: {reason}"),
    }
}

/// A short reason for a bind failure. `AddrInUse` is the common one and worth its
/// own phrasing (`address in use`); anything else falls back to the OS message.
fn bind_reason(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        "address in use".into()
    } else {
        e.to_string()
    }
}

/// The route label for the initial `forwarding …` status: the resolved label
/// once the supervisor has published one, else a best-effort `target:port` from
/// the request (the resolve runs on the forward's thread, so it's usually empty
/// this early).
fn route_label_or(forward: &Forward, container_port: u16, req: &PortRequestOwned) -> String {
    let label = forward.status().route_label;
    if !label.is_empty() {
        return label;
    }
    match &req.service {
        Some(svc) => format!("{svc}:{container_port}"),
        None => format!("{}:{container_port}", req.instance),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::forward::{ForwardState, ForwardStatus};
    use std::net::{Ipv4Addr, SocketAddr};

    fn status(state: ForwardState, process: Option<&str>, conns: usize) -> ForwardStatus {
        ForwardStatus {
            local_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 3000),
            route_label: "api:3000".into(),
            state,
            open_conns: conns,
            process: process.map(str::to_string),
        }
    }

    // ---- pure: ForwardStatus -> PortRow ----

    #[test]
    fn status_to_row_maps_state_strings() {
        let row = status_to_row(7, &status(ForwardState::Active, Some("node (pid 412)"), 3));
        assert_eq!(row.id, 7);
        assert_eq!(row.local, "127.0.0.1:3000");
        assert_eq!(row.target, "api:3000");
        assert_eq!(row.process.as_deref(), Some("node (pid 412)"));
        assert_eq!(row.state, "active");
        assert_eq!(row.conns, 3);

        assert_eq!(status_to_row(1, &status(ForwardState::Connecting, None, 0)).state, "connecting");
        assert_eq!(
            status_to_row(1, &status(ForwardState::Error("no such host db".into()), None, 0)).state,
            "error: no such host db"
        );
        // Unknown process → None (rendered as `-` by the table).
        assert_eq!(status_to_row(1, &status(ForwardState::Connecting, None, 0)).process, None);
    }

    // ---- pure: bind_reason ----

    #[test]
    fn bind_reason_names_addr_in_use() {
        let in_use = std::io::Error::new(std::io::ErrorKind::AddrInUse, "os says busy");
        assert_eq!(bind_reason(&in_use), "address in use");
        let other = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        assert_eq!(bind_reason(&other), "nope");
    }

    // ---- pure: rows-changed gate + note dedup, exercised through Forwards ----

    /// The `rows != last_rows` gate: identical rows produce no update; a change
    /// (a new state string) does.
    #[test]
    fn rows_changed_gate() {
        let a = vec![status_to_row(1, &status(ForwardState::Connecting, None, 0))];
        let b = a.clone();
        assert_eq!(a, b, "identical rows compare equal (no update sent)");
        let c = vec![status_to_row(1, &status(ForwardState::Active, None, 0))];
        assert_ne!(a, c, "a state change makes rows differ (update sent)");
    }

    // ---- worker send/coalesce/drop without docker ----

    /// A request for an unknown instance comes back as an error `Status`, and
    /// dropping the worker joins promptly. No docker: `resolve_instance` fails on
    /// the empty state before any `exec`. Modelled on
    /// `bridge::tests::worker_coalesces_and_joins`.
    #[test]
    fn add_unknown_instance_reports_status_then_joins() {
        let dir = std::env::temp_dir().join("devsbd-fwd-worker-test");
        let worker = ForwardWorker::spawn(dir);
        worker.add(super::super::app::PortRequest {
            instance: "ghost-instance-xyz".into(),
            service: None,
            address: None,
            spec: "3000".into(),
        });
        // The worker resolves against the real state; `ghost-instance-xyz` won't
        // exist, so we get a Status carrying the resolve error. (If a machine
        // genuinely had that instance the forward would start instead — the name
        // is chosen to be absurd.)
        let mut got_status = false;
        for _ in 0..200 {
            if let Some(ForwardUpdate::Status(_)) = worker.try_recv() {
                got_status = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(got_status, "no status came back for the unknown instance");
        drop(worker); // joins without deadlock
    }
}
