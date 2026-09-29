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
//! It also runs the sandboxes' `forwardPorts` ([`ForwardWorker::sync`]): every
//! snapshot hands it the running instances, and it starts and stops their
//! configured forwards to match, reusing the host port saved in state for each
//! (docs/port-forwarding.md, _Configured forwards_).
//!
//! [`remove`]: ForwardWorker::remove

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::commands::{port, services};
use crate::config::{Config, ForwardPort, ServiceScope};
use crate::devsbd::forward::{Forward, ForwardSpec, ForwardState, ForwardStatus, HostPort, PREFER_SPAN};
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
    /// The running instances' state keys: reconcile configured forwards.
    Sync(BTreeSet<String>),
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

    /// Hand the worker the running instances (state keys) so it starts their
    /// `forwardPorts` and stops those of instances that went away. Never blocks.
    pub fn sync(&self, running: BTreeSet<String>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Sync(running));
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

/// Who a configured forward belongs to, which is also where its host port is
/// saved in state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    /// Instance state key: its own ports and its isolated services'.
    Instance(String),
    /// A `global` service's port: one forward per config root, however many
    /// running instances declare it.
    Global,
}

/// Identity of a configured forward: its owner plus the entry's
/// [`ForwardPort::key`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Configured {
    owner: Owner,
    entry: String,
}

/// The worker's owned forwards, keyed by the id it hands out (monotonic).
struct Forwards {
    dir: PathBuf,
    /// Config-root id (`services::project_id`), the key for global forwards'
    /// saved ports. `None` when `dir` doesn't resolve: no configured forwards.
    project: Option<String>,
    next_id: u64,
    live: BTreeMap<u64, Forward>,
    /// Live configured forwards -> their id in `live`.
    configured: BTreeMap<Configured, u64>,
    /// Configured forwards stopped with `d`: not restarted this session.
    suppressed: BTreeSet<Configured>,
    /// Configured forwards that failed to start (reported once); retried when
    /// their owner stops and comes back.
    failed: BTreeSet<Configured>,
    /// Last rows sent, so an unchanged poll produces no `Rows` update.
    last_rows: Vec<PortRow>,
    /// Last status sent, so a repeated note isn't re-sent.
    last_status: Option<String>,
}

/// Worker loop: block on the command channel with a `POLL` timeout so it wakes
/// for both commands and the periodic status poll. Returns (dropping every
/// forward) once the command channel disconnects.
fn run(dir: PathBuf, rx: Receiver<Cmd>, utx: Sender<ForwardUpdate>) {
    let project = services::project_id(&dir).ok();
    let mut fwds = Forwards {
        dir,
        project,
        next_id: 1,
        live: BTreeMap::new(),
        configured: BTreeMap::new(),
        suppressed: BTreeSet::new(),
        failed: BTreeSet::new(),
        last_rows: Vec::new(),
        last_status: None,
    };
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Cmd::Add(req)) => fwds.add(req, &utx),
            Ok(Cmd::Remove(id)) => fwds.remove(id, &utx),
            Ok(Cmd::Sync(running)) => fwds.sync(&running, &utx),
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
        self.insert(forward);
        Ok((addr, label))
    }

    fn insert(&mut self, forward: Forward) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.live.insert(id, forward);
        id
    }

    /// Drop the forward with `id` (its `Drop` kills the bridge/`exec`). Reports a
    /// `stopped <local>` status; unknown ids are ignored (a stale `d`). A
    /// configured forward stays stopped for the rest of the session instead of
    /// coming straight back on the next sync.
    fn remove(&mut self, id: u64, utx: &Sender<ForwardUpdate>) {
        if let Some(forward) = self.live.remove(&id) {
            let addr = forward.status().local_addr;
            drop(forward);
            let configured = self.configured.iter().find(|(_, v)| **v == id).map(|(c, _)| c.clone());
            let msg = match configured {
                Some(c) => {
                    self.configured.remove(&c);
                    self.suppressed.insert(c);
                    format!("stopped {addr} (forwardPorts: back when the dashboard reopens)")
                }
                None => format!("stopped {addr}"),
            };
            let _ = utx.send(ForwardUpdate::Status(msg));
        }
    }

    /// Reconcile configured forwards with the running instances: stop those
    /// no longer wanted, start the missing ones on their saved (or a newly
    /// allocated) host port, and save newly chosen ports. Config or state that
    /// doesn't load leaves everything as it is; the dashboard shows that error.
    fn sync(&mut self, running: &BTreeSet<String>, utx: &Sender<ForwardUpdate>) {
        let Some(project) = self.project.clone() else { return };
        let (Ok(config), Ok(mut state)) = (Config::load(&self.dir), State::load()) else { return };
        let wanted = wanted(&config, &state, running, &project);

        let gone: Vec<Configured> = self.configured.keys().filter(|c| !wanted.contains_key(c)).cloned().collect();
        for c in gone {
            if let Some(id) = self.configured.remove(&c) {
                self.live.remove(&id);
            }
        }
        self.failed.retain(|c| wanted.contains_key(c));

        // Stale saved ports would block allocation forever; prune before
        // computing reservations.
        let mut dirty = prune_saved(&mut state, &config, &project);
        let mut chosen = Vec::new();
        for (c, fp) in &wanted {
            if self.configured.contains_key(c) || self.suppressed.contains(c) || self.failed.contains(c) {
                continue;
            }
            match self.start_configured(&state, &project, c, fp) {
                Ok((id, port, label)) => {
                    self.configured.insert(c.clone(), id);
                    let _ = utx.send(ForwardUpdate::Status(format!("forwarding 127.0.0.1:{port} -> {label}")));
                    // Later allocations in this pass must see it as taken.
                    dirty |= save_port(&mut state, &project, c, port);
                    chosen.push((c.clone(), port));
                }
                Err(msg) => {
                    self.failed.insert(c.clone());
                    let _ = utx.send(ForwardUpdate::Status(msg));
                }
            }
        }
        if !dirty {
            return;
        }
        // Re-read before writing so a `run`/`rm` since the load isn't undone.
        let Ok(mut fresh) = State::load() else { return };
        let mut changed = prune_saved(&mut fresh, &config, &project);
        for (c, port) in &chosen {
            changed |= save_port(&mut fresh, &project, c, *port);
        }
        if changed && let Err(e) = fresh.save() {
            let _ = utx.send(ForwardUpdate::Status(format!("forwardPorts: {e:#}")));
        }
    }

    /// Bind the first free candidate host port for `c` and start its forward.
    /// Returns `(id, host port, route label)`.
    fn start_configured(
        &mut self,
        state: &State,
        project: &str,
        c: &Configured,
        fp: &ForwardPort,
    ) -> Result<(u64, u16, String), String> {
        let instance = match &c.owner {
            Owner::Instance(key) => Some(key.clone()),
            Owner::Global => None,
        };
        let saved = saved_port(state, project, c);
        let reserved = reserved_ports(state, project, c);
        for port in candidates(saved, fp.base(), &reserved) {
            let spec = ForwardSpec {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                host_port: HostPort::Fixed(port),
                resolve: port::resolver(self.dir.clone(), instance.clone(), fp.service.clone(), fp.port),
                probe: Box::new(port::listening_procs),
            };
            match Forward::start(spec) {
                Ok(forward) => {
                    let label = match (&fp.service, &instance) {
                        (Some(svc), _) => format!("{svc}:{}", fp.port),
                        (None, Some(key)) => format!("{key}:{}", fp.port),
                        (None, None) => fp.port.to_string(),
                    };
                    return Ok((self.insert(forward), port, label));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => return Err(format!("forwardPorts `{}`: port {port}: {}", c.entry, bind_reason(&e))),
            }
        }
        Err(format!("forwardPorts `{}`: no free host port from {} up", c.entry, fp.base()))
    }

    /// Poll every forward's status: rebuild the rows and send `Rows` only when
    /// they changed, then drain each forward's notes and send the newest as a
    /// deduped `Status`.
    fn poll(&mut self, utx: &Sender<ForwardUpdate>) {
        let configured: BTreeSet<u64> = self.configured.values().copied().collect();
        let rows: Vec<PortRow> = self
            .live
            .iter()
            .map(|(&id, f)| status_to_row(id, &f.status(), configured.contains(&id)))
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
fn status_to_row(id: u64, status: &ForwardStatus, configured: bool) -> PortRow {
    PortRow {
        id,
        local: status.local_addr.to_string(),
        target: status.route_label.clone(),
        process: status.process.clone(),
        state: state_string(&status.state),
        conns: status.open_conns,
        configured,
    }
}

/// Whether `service` is a `global` one (its forward is shared by the config
/// root). An unknown service counts as isolated; its route reports the error.
fn is_global(config: &Config, service: &str) -> bool {
    config.resolve_service(service).is_ok_and(|s| s.spec.scope == ServiceScope::Global)
}

/// Whether `inst` belongs to this config root. Instances from before `project`
/// was recorded match on the sandbox name alone.
fn in_root(config: &Config, inst: &crate::state::Instance, project: &str) -> bool {
    config.sandboxes.contains_key(&inst.sandbox) && (inst.project.is_empty() || inst.project == project)
}

/// The configured forwards the running instances declare. Pure over
/// `(config, state, running)`. A global service's entry is keyed once for the
/// whole root; sandboxes that don't resolve are skipped (the dashboard shows
/// config errors elsewhere).
fn wanted(
    config: &Config,
    state: &State,
    running: &BTreeSet<String>,
    project: &str,
) -> BTreeMap<Configured, ForwardPort> {
    let mut out = BTreeMap::new();
    for key in running {
        let Some(inst) = state.instances.get(key).filter(|i| in_root(config, i, project)) else { continue };
        let Ok(sandbox) = config.resolve_sandbox(&inst.sandbox) else { continue };
        for fp in sandbox.properties.forward_ports.unwrap_or_default() {
            let owner = match &fp.service {
                Some(svc) if is_global(config, svc) => Owner::Global,
                _ => Owner::Instance(key.clone()),
            };
            out.entry(Configured { owner, entry: fp.key() }).or_insert(fp);
        }
    }
    out
}

/// The host port saved for `c`, if any.
fn saved_port(state: &State, project: &str, c: &Configured) -> Option<u16> {
    match &c.owner {
        Owner::Instance(key) => state.instances.get(key)?.forwarded_ports.get(&c.entry).copied(),
        Owner::Global => state.global_forwarded_ports.get(project)?.get(&c.entry).copied(),
    }
}

/// Every saved host port except `c`'s own: what other forwards (running or
/// not, any config root) hold, so a stopped instance keeps its ports.
fn reserved_ports(state: &State, project: &str, c: &Configured) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    for (key, inst) in &state.instances {
        for (entry, port) in &inst.forwarded_ports {
            if !(c.owner == Owner::Instance(key.clone()) && *entry == c.entry) {
                out.insert(*port);
            }
        }
    }
    for (root, entries) in &state.global_forwarded_ports {
        for (entry, port) in entries {
            if !(c.owner == Owner::Global && root == project && *entry == c.entry) {
                out.insert(*port);
            }
        }
    }
    out
}

/// Host ports to try, in order: the saved one, then `base` upward over
/// [`PREFER_SPAN`] ports, skipping ones other forwards hold.
fn candidates(saved: Option<u16>, base: u16, reserved: &BTreeSet<u16>) -> Vec<u16> {
    let scan = (base..=base.saturating_add(PREFER_SPAN - 1)).filter(|p| Some(*p) != saved && !reserved.contains(p));
    saved.into_iter().chain(scan).collect()
}

/// Record `port` as `c`'s host port. Returns whether state changed.
fn save_port(state: &mut State, project: &str, c: &Configured, port: u16) -> bool {
    let map = match &c.owner {
        Owner::Instance(key) => match state.instances.get_mut(key) {
            Some(inst) => &mut inst.forwarded_ports,
            None => return false,
        },
        Owner::Global => state.global_forwarded_ports.entry(project.to_string()).or_default(),
    };
    map.insert(c.entry.clone(), port) != Some(port)
}

/// Drop saved ports of entries this config root no longer declares, so they
/// stop reserving host ports. Only touches instances of this root whose
/// sandbox resolves; global entries only when every sandbox resolves (else an
/// entry might just be unreadable right now). Returns whether state changed.
fn prune_saved(state: &mut State, config: &Config, project: &str) -> bool {
    let mut changed = false;
    let mut global_keys = BTreeSet::new();
    let mut all_resolved = true;
    let mut per_sandbox: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for name in config.sandboxes.keys() {
        let Ok(sandbox) = config.resolve_sandbox(name) else {
            all_resolved = false;
            continue;
        };
        let keys = per_sandbox.entry(name).or_default();
        for fp in sandbox.properties.forward_ports.unwrap_or_default() {
            match &fp.service {
                Some(svc) if is_global(config, svc) => global_keys.insert(fp.key()),
                _ => keys.insert(fp.key()),
            };
        }
    }
    for inst in state.instances.values_mut() {
        if inst.project != project {
            continue;
        }
        let Some(keys) = per_sandbox.get(inst.sandbox.as_str()) else { continue };
        let before = inst.forwarded_ports.len();
        inst.forwarded_ports.retain(|entry, _| keys.contains(entry));
        changed |= inst.forwarded_ports.len() != before;
    }
    if all_resolved && let Some(entries) = state.global_forwarded_ports.get_mut(project) {
        let before = entries.len();
        entries.retain(|entry, _| global_keys.contains(entry));
        changed |= entries.len() != before;
        if entries.is_empty() {
            state.global_forwarded_ports.remove(project);
        }
    }
    changed
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
    use crate::state::Instance;
    use std::net::{Ipv4Addr, SocketAddr};

    // ---- pure: configured forwards ----

    const CONFIG: &str = r#"
[services.db]
image = "postgres"

[services.redis]
image = "redis"
scope = "global"

[sandbox.api]
services = ["db", "redis"]
forwardPorts = [3000, "8080:3000", "db:5432", "redis:6379"]

[sandbox.web]
services = ["redis"]
forwardPorts = ["redis:6379"]
"#;

    fn inst(sandbox: &str, project: &str, saved: &[(&str, u16)]) -> Instance {
        let mut i: Instance = toml::from_str(&format!(
            "sandbox = \"{sandbox}\"\nproject = \"{project}\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n"
        ))
        .unwrap();
        i.forwarded_ports = saved.iter().map(|(k, p)| (k.to_string(), *p)).collect();
        i
    }

    fn cfg(owner: Owner, entry: &str) -> Configured {
        Configured { owner, entry: entry.into() }
    }

    fn fixture() -> (Config, State) {
        let config = Config::parse(CONFIG).unwrap();
        let mut state = State::default();
        state.instances.insert("api".into(), inst("api", "p1", &[("3000", 3001)]));
        state.instances.insert("api-2".into(), inst("api", "p1", &[]));
        state.instances.insert("web".into(), inst("web", "p1", &[]));
        state.instances.insert("elsewhere".into(), inst("api", "p2", &[("3000", 3000)]));
        (config, state)
    }

    #[test]
    fn wanted_per_instance_and_one_global_forward_per_root() {
        let (config, state) = fixture();
        let running: BTreeSet<String> = ["api", "web", "elsewhere"].map(String::from).into();
        let keys: Vec<Configured> = wanted(&config, &state, &running, "p1").into_keys().collect();
        let api = || Owner::Instance("api".into());
        assert_eq!(
            keys,
            vec![
                cfg(api(), "3000"),
                cfg(api(), "8080:3000"),
                cfg(api(), "db:5432"),
                // Declared by both `api` and `web`, forwarded once.
                cfg(Owner::Global, "redis:6379"),
            ]
        );
        // Another root's instance and stopped instances contribute nothing.
        assert!(wanted(&config, &state, &BTreeSet::new(), "p1").is_empty());
    }

    #[test]
    fn candidates_try_saved_then_scan_up_skipping_reserved() {
        let reserved: BTreeSet<u16> = [3001, 3003].into();
        let got = candidates(Some(3002), 3000, &reserved);
        assert_eq!(&got[..4], &[3002, 3000, 3004, 3005]);
        assert_eq!(got.len(), PREFER_SPAN as usize - 2 - 1 + 1);
        assert_eq!(candidates(None, u16::MAX, &BTreeSet::new()), vec![u16::MAX]);
    }

    #[test]
    fn saved_and_reserved_ports_exclude_only_the_forward_itself() {
        let (_, mut state) = fixture();
        let api3000 = cfg(Owner::Instance("api".into()), "3000");
        let api2 = cfg(Owner::Instance("api-2".into()), "3000");
        assert_eq!(saved_port(&state, "p1", &api3000), Some(3001));
        assert_eq!(saved_port(&state, "p1", &api2), None);
        // `api-2` must not take `api`'s port, nor the other root's.
        assert_eq!(reserved_ports(&state, "p1", &api2), [3000, 3001].into());
        assert_eq!(reserved_ports(&state, "p1", &api3000), [3000].into());

        let global = cfg(Owner::Global, "redis:6379");
        assert!(save_port(&mut state, "p1", &global, 6379));
        assert!(!save_port(&mut state, "p1", &global, 6379), "unchanged");
        assert_eq!(saved_port(&state, "p1", &global), Some(6379));
        assert_eq!(saved_port(&state, "p2", &global), None);
        assert!(reserved_ports(&state, "p1", &api2).contains(&6379));
        assert!(!save_port(&mut state, "p1", &cfg(Owner::Instance("ghost".into()), "3000"), 1));
    }

    #[test]
    fn prune_drops_entries_no_longer_declared() {
        let (config, mut state) = fixture();
        state.instances.get_mut("api").unwrap().forwarded_ports.insert("4000".into(), 4000);
        state.global_forwarded_ports.entry("p1".into()).or_default().insert("redis:6379".into(), 6379);
        state.global_forwarded_ports.entry("p1".into()).or_default().insert("redis:1".into(), 1);
        assert!(prune_saved(&mut state, &config, "p1"));
        assert_eq!(state.instances["api"].forwarded_ports, BTreeMap::from([("3000".into(), 3001)]));
        assert_eq!(state.global_forwarded_ports["p1"], BTreeMap::from([("redis:6379".into(), 6379)]));
        // Other roots' instances are left alone.
        assert_eq!(state.instances["elsewhere"].forwarded_ports["3000"], 3000);
        assert!(!prune_saved(&mut state, &config, "p1"), "idempotent");
    }

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
        let row = status_to_row(7, &status(ForwardState::Active, Some("node (pid 412)"), 3), true);
        assert_eq!(row.id, 7);
        assert_eq!(row.local, "127.0.0.1:3000");
        assert_eq!(row.target, "api:3000");
        assert_eq!(row.process.as_deref(), Some("node (pid 412)"));
        assert_eq!(row.state, "active");
        assert_eq!(row.conns, 3);
        assert!(row.configured);

        assert_eq!(status_to_row(1, &status(ForwardState::Connecting, None, 0), false).state, "connecting");
        assert_eq!(
            status_to_row(1, &status(ForwardState::Error("no such host db".into()), None, 0), false).state,
            "error: no such host db"
        );
        // Unknown process → None (rendered as `-` by the table).
        assert_eq!(status_to_row(1, &status(ForwardState::Connecting, None, 0), false).process, None);
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
        let a = vec![status_to_row(1, &status(ForwardState::Connecting, None, 0), false)];
        let b = a.clone();
        assert_eq!(a, b, "identical rows compare equal (no update sent)");
        let c = vec![status_to_row(1, &status(ForwardState::Active, None, 0), false)];
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
