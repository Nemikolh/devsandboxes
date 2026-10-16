//! The daemon's port forwards (docs/architecture/daemon/forwards.md,
//! docs/reference/api.md *Forwards*): every live [`Forward`], ad-hoc
//! (`forwards.add`) and configured (the sandboxes' `forwardPorts`), owned by
//! one `serve-forwards` thread so they outlive any dashboard and count as
//! idle holders.
//!
//! Threading: the [`Registry`] thread blocks on its command channel with a
//! [`POLL`] timeout, so it wakes both for requests ([`Handle`]: the API's
//! `forwards.*`, each with a reply channel, and the host poll's running
//! containers) and for the periodic status poll. All config/state work, route
//! resolution and binding happens there, never on a connection thread; a
//! request only waits for its reply (up to [`REPLY_TIMEOUT`]). Dropping the
//! [`Registry`] stops the thread and drops every forward (closing its
//! listener, killing its bridge) before it returns.
//!
//! Forwards are grouped per config root ([`Root`], keyed by the canonical
//! `dir`); ids are unique daemon-wide. Configured forwards are reconciled for
//! every config root recorded in `state.toml` against the host's poll of the
//! running containers (`docs/architecture/daemon/forwards.md`, _Configured
//! forwards (`forwardPorts`)_), reusing the host port saved in state. Status
//! goes out through [`Event`]s (the API's `forwards.changed` /
//! `forwards.status`) and `serve.log`; nothing else is printed.
//!
//! Ad-hoc forwards persist in `forwards.toml` ([`forward_store`]): an add
//! appends, an rm removes, and the thread restores them first thing, on
//! their saved host ports, so they survive a handoff or restart. Stopping
//! the thread leaves the file alone: dropping forwards on exit isn't removal.
//!
//! Forwards into a machine's instance or service ([`Remote`], `forwards.add`
//! with `machine`) are ad-hoc only and live apart from the [`Root`]s (which
//! load local config): bound here, each bridge an ssh running `devsandbox
//! tunnel` on the machine (`tunnel::remote_opener`). They persist like local
//! ones; a sync drops those of a machine gone from `machines.toml`.
//!
//! Each forward keeps its own self-healing bridge (`Forward::start`); riding
//! the instance's daemon bridge mux instead is a later optimization.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::api::ApiError;
use super::forward_store::{self, Entry};
use super::machines;
use crate::commands::{port, services, tunnel};
use crate::config::{Config, ForwardPort, ServiceScope};
use crate::devsbd::forward::{local_opener, Forward, ForwardSpec, ForwardState, ForwardStatus, HostPort, PREFER_SPAN};
use crate::state::State;

/// How often the registry re-polls every forward's `status()` while idle, so
/// a state change (Connecting → Active, a note) reaches subscribers within a
/// poll. Also the cadence at which per-connection notes are drained.
pub const POLL: Duration = Duration::from_millis(500);
/// How long an API request waits for the registry's answer (a sync binding
/// several configured forwards may be ahead of it in the queue).
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a restore retries a saved host port that's taken before it
/// falls back to the next free one.
const RESTORE_PORT_WAIT: Duration = Duration::from_secs(1);

/// One row of `forwards.list`: the dashboard's Ports-tab row plus the forward's
/// config root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardRow {
    /// Daemon-wide id, what `forwards.rm` takes.
    pub id: u64,
    /// Canonical config root; with `machine`, the root on that machine.
    pub dir: PathBuf,
    /// Host side, e.g. `"127.0.0.1:3000"`.
    pub local: String,
    /// Route label, e.g. `"api:3000"` or `"postgres:5432 (via instance api)"`;
    /// empty until the first route resolve. A remote forward's is
    /// `<machine>/<label>`, best effort from the request until the far side
    /// first answers.
    pub target: String,
    /// Listening process (`node (pid 412)`), when known.
    pub process: Option<String>,
    /// `"active"`, `"connecting"`, or `"error: …"`.
    pub state: String,
    /// Open connections.
    pub conns: usize,
    /// Started from a sandbox's `forwardPorts`, not by `forwards.add`.
    pub configured: bool,
    /// The machine whose instance or service it reaches; absent for this
    /// host's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// A validated `forwards.add` (the API handler parses and checks the params).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRequest {
    /// Absolute config root; canonicalized by the registry. With `machine`,
    /// the root on that machine, taken as is.
    pub dir: PathBuf,
    /// Instance name as the user typed it; `None` for a service-only forward.
    /// With `machine`, its name there (resolved by the far `tunnel`).
    pub instance: Option<String>,
    pub service: Option<String>,
    pub bind: IpAddr,
    pub host_port: HostPort,
    pub container_port: u16,
    /// A registered machine: the forward still binds here.
    pub machine: Option<String>,
}

/// `forwards.add`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Added {
    pub id: u64,
    /// The bound host address.
    pub local: String,
    /// Route label, best effort this early (see [`route_label_or`]).
    pub target: String,
}

/// What `forwards.rm` stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub local: String,
    pub configured: bool,
}

/// From the registry to the daemon's subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The rows changed: `forwards.changed`.
    Changed,
    /// A one-line status (a configured forward started or failed, a
    /// connection note): `forwards.status`.
    Status(String),
}

pub type OnEvent = Box<dyn Fn(Event) + Send>;

/// Builds a remote forward's argv from its machine's registry entry, root,
/// instance, service and port: [`batch_tunnel_argv`] outside tests.
type TunnelArgvFn = fn(&machines::Entry, &Path, Option<&str>, Option<&str>, u16) -> anyhow::Result<Vec<OsString>>;

/// `ssh::tunnel_argv` with `BatchMode`: the daemon has no TTY to prompt on.
fn batch_tunnel_argv(
    entry: &machines::Entry,
    root: &Path,
    instance: Option<&str>,
    service: Option<&str>,
    port: u16,
) -> anyhow::Result<Vec<OsString>> {
    machines::ssh::tunnel_argv(entry, true, root, instance, service, port)
}

enum Cmd {
    List { dir: Option<PathBuf>, machines: bool, reply: Sender<Vec<ForwardRow>> },
    Add { req: AddRequest, reply: Sender<Result<Added, ApiError>> },
    Remove { id: u64, reply: Sender<Result<Removed, ApiError>> },
    /// The runtime's running containers, from the host poll.
    Sync(Vec<String>),
    Stop,
}

/// The registry thread's handle, owned by the daemon. Dropping it stops the
/// thread and drops every forward.
pub struct Registry {
    tx: Sender<Cmd>,
    active: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

/// A cheap clone of the registry's command channel: the API and the host
/// poll talk through it. Requests after the registry stopped fail.
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Cmd>,
}

impl Registry {
    /// Start the registry thread. `log` writes one `serve.log` line; `store`
    /// is the ad-hoc forwards' file ([`forward_store::path`] outside tests),
    /// restored before any request is served; `machines` is `machines.toml`,
    /// read for remote forwards (each add, each bridge spawn, each sync);
    /// `on_event` gets every [`Event`] (called on the registry thread: it must
    /// not block).
    pub fn spawn(log: fn(&str), store: PathBuf, machines: PathBuf, on_event: OnEvent) -> Registry {
        Self::spawn_with(log, store, machines, batch_tunnel_argv, on_event)
    }

    fn spawn_with(
        log: fn(&str),
        store: PathBuf,
        machines: PathBuf,
        tunnel_argv: TunnelArgvFn,
        on_event: OnEvent,
    ) -> Registry {
        let (tx, rx) = mpsc::channel();
        let active = Arc::new(AtomicUsize::new(0));
        let thread = {
            let active = Arc::clone(&active);
            std::thread::Builder::new().name("serve-forwards".into()).spawn(move || {
                let mut daemon = Daemon::new(log, store, machines, tunnel_argv, on_event, active);
                daemon.restore();
                daemon.poll();
                daemon.run(&rx);
            })
        };
        let thread = thread.map_err(|e| log(&format!("cannot start the forwards thread: {e}"))).ok();
        Registry { tx, active, thread }
    }

    pub fn handle(&self) -> Handle {
        Handle { tx: self.tx.clone() }
    }

    /// Live forwards, ad-hoc, configured and remote: the `forwards` holder
    /// count.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // Not a channel close: `Handle`s outlive us (in the daemon's shared
        // state), so the thread is told to stop explicitly.
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Handle {
    /// Hand the registry the running containers. Never blocks.
    pub fn sync(&self, running: Vec<String>) {
        let _ = self.tx.send(Cmd::Sync(running));
    }

    fn ask<T>(&self, cmd: impl FnOnce(Sender<T>) -> Cmd) -> Result<T, ApiError> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(cmd(reply)).map_err(|_| stopped())?;
        rx.recv_timeout(REPLY_TIMEOUT).map_err(|e| match e {
            RecvTimeoutError::Timeout => ApiError::internal(anyhow::anyhow!("the forwards registry didn't answer")),
            RecvTimeoutError::Disconnected => stopped(),
        })
    }
}

fn stopped() -> ApiError {
    ApiError::internal(anyhow::anyhow!("port forwarding is stopping"))
}

impl super::api::Forwarding for Handle {
    fn list(&self, dir: Option<PathBuf>, machines: bool) -> Result<Vec<ForwardRow>, ApiError> {
        self.ask(|reply| Cmd::List { dir, machines, reply })
    }

    fn add(&self, req: AddRequest) -> Result<Added, ApiError> {
        self.ask(|reply| Cmd::Add { req, reply })?
    }

    fn rm(&self, id: u64) -> Result<Removed, ApiError> {
        self.ask(|reply| Cmd::Remove { id, reply })?
    }
}

/// The registry thread's state.
struct Daemon {
    log: fn(&str),
    /// `forwards.toml`.
    store: PathBuf,
    /// `machines.toml`.
    machines: PathBuf,
    tunnel_argv: TunnelArgvFn,
    on_event: OnEvent,
    active: Arc<AtomicUsize>,
    roots: BTreeMap<PathBuf, Root>,
    /// Forwards into machines, by id.
    remote: BTreeMap<u64, Remote>,
    next_id: u64,
    /// Last rows seen, so an unchanged poll sends no [`Event::Changed`].
    last_rows: Vec<ForwardRow>,
    /// Last note sent, so a repeated one isn't re-sent.
    last_status: Option<String>,
}

impl Daemon {
    fn new(
        log: fn(&str),
        store: PathBuf,
        machines: PathBuf,
        tunnel_argv: TunnelArgvFn,
        on_event: OnEvent,
        active: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            log,
            store,
            machines,
            tunnel_argv,
            on_event,
            active,
            roots: BTreeMap::new(),
            remote: BTreeMap::new(),
            next_id: 1,
            last_rows: Vec::new(),
            last_status: None,
        }
    }

    fn run(&mut self, rx: &Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Cmd::List { dir, machines, reply }) => {
                    let _ = reply.send(self.rows(dir.as_deref(), machines));
                }
                Ok(Cmd::Add { req, reply }) => {
                    let _ = reply.send(self.add(req));
                }
                Ok(Cmd::Remove { id, reply }) => {
                    let _ = reply.send(self.remove(id));
                }
                Ok(Cmd::Sync(running)) => self.sync(&running),
                Err(RecvTimeoutError::Timeout) => {}
                Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            }
            self.poll();
        }
        // Every Forward drops here: listeners close, bridges die. The store
        // keeps them, for the successor.
        self.roots.clear();
        self.remote.clear();
        self.active.store(0, Ordering::Release);
    }

    fn status(&mut self, line: String) {
        (self.log)(&format!("forwards: {line}"));
        (self.on_event)(Event::Status(line));
    }

    /// Rows of every local root, or of `dir` (canonicalized; a dir that
    /// doesn't resolve has none), plus with `machines` every remote forward
    /// (whatever `dir`), by id.
    fn rows(&self, dir: Option<&Path>, machines: bool) -> Vec<ForwardRow> {
        let dir = dir.map(|d| d.canonicalize().unwrap_or_else(|_| d.to_path_buf()));
        let mut rows: Vec<ForwardRow> = self
            .roots
            .values()
            .filter(|r| dir.as_ref().is_none_or(|d| *d == r.dir))
            .flat_map(Root::rows)
            .collect();
        if machines {
            rows.extend(self.remote.iter().map(|(&id, r)| r.row(id)));
        }
        rows.sort_by_key(|r| r.id);
        rows
    }

    fn root(&mut self, dir: &Path) -> &mut Root {
        self.roots.entry(dir.to_path_buf()).or_insert_with(|| Root::new(dir.to_path_buf()))
    }

    fn add(&mut self, req: AddRequest) -> Result<Added, ApiError> {
        if let Some(machine) = req.machine.clone() {
            return self.add_remote(machine, &req);
        }
        let dir = req
            .dir
            .canonicalize()
            .map_err(|e| ApiError::not_found(format!("no directory {}: {e}", req.dir.display())))?;
        let mut next_id = self.next_id;
        let started = self.root(&dir).start(&req, &mut next_id);
        self.next_id = next_id;
        let (added, entry) = started?;
        self.persist(|entries| entries.push(entry));
        Ok(added)
    }

    /// `forwards.add` into `machine`: it must be registered; the far side
    /// resolves the rest, on the forward's own thread.
    fn add_remote(&mut self, machine: String, req: &AddRequest) -> Result<Added, ApiError> {
        let known = machines::load(&self.machines).map_err(ApiError::internal)?;
        if !known.iter().any(|m| m.name == machine) {
            return Err(ApiError::not_found(machines::no_machine(&machine).to_string()));
        }
        let entry = Entry {
            dir: req.dir.clone(),
            instance: req.instance.clone(),
            service: req.service.clone(),
            address: Entry::address_of(req.bind),
            container_port: req.container_port,
            host_port: 0,
            machine: Some(machine),
        };
        let (id, entry) = self.start_remote(entry, req.host_port).map_err(|e| bind_failed(req.host_port, &e))?;
        let forward = &self.remote[&id].forward;
        let added = Added { id, local: forward.status().local_addr.to_string(), target: route_label_or(forward, req) };
        self.persist(|entries| entries.push(entry));
        Ok(added)
    }

    /// Bind `entry`'s remote forward (its `machine` set) on `host_port` and
    /// register it. Returns its id and the entry with the bound host port.
    fn start_remote(&mut self, mut entry: Entry, host_port: HostPort) -> std::io::Result<(u64, Entry)> {
        let machine = entry.machine.clone().unwrap_or_default();
        let registry = self.machines.clone();
        let build = self.tunnel_argv;
        let (name, root, instance, service, port) =
            (machine.clone(), entry.dir.clone(), entry.instance.clone(), entry.service.clone(), entry.container_port);
        // Re-read on every spawn: a changed target heals, a removed machine
        // is the bridge's error until the next sync drops the forward.
        let argv: tunnel::TunnelArgv = Box::new(move || {
            let entry = machines::load(&registry)
                .map_err(|e| format!("{e:#}"))?
                .into_iter()
                .find(|m| m.name == name)
                .ok_or_else(|| machines::no_machine(&name).to_string())?;
            build(&entry, &root, instance.as_deref(), service.as_deref(), port).map_err(|e| format!("{e:#}"))
        });
        let open = tunnel::remote_opener(machine, argv);
        // No probe fallback: the far helper is the only listening-process lookup.
        let probe = Box::new(|_: &str, _: u16| None);
        let forward = Forward::start(ForwardSpec { bind: entry.bind(), host_port, open, probe })?;
        entry.host_port = forward.status().local_addr.port();
        let id = self.next_id;
        self.next_id += 1;
        self.remote.insert(id, Remote { forward, entry: entry.clone() });
        Ok((id, entry))
    }

    /// The registered machines' names.
    fn registered(&self) -> anyhow::Result<BTreeSet<String>> {
        Ok(machines::load(&self.machines)?.into_iter().map(|m| m.name).collect())
    }

    /// Drop the remote forwards, and their store entries, of machines no
    /// longer in `machines.toml` (`machine rm`). A registry that doesn't load
    /// drops nothing; with no remote forwards it isn't read.
    fn drop_unregistered(&mut self) {
        if self.remote.is_empty() {
            return;
        }
        let Ok(names) = self.registered() else { return };
        let gone: Vec<u64> =
            self.remote.iter().filter(|(_, r)| !names.contains(r.machine())).map(|(&id, _)| id).collect();
        if gone.is_empty() {
            return;
        }
        let mut entries = Vec::new();
        for id in gone {
            let Some(r) = self.remote.remove(&id) else { continue };
            (self.log)(&format!(
                "forwards: dropping {}: machine `{}` is no longer registered",
                describe(&r.entry),
                r.machine()
            ));
            entries.push(r.entry);
        }
        self.persist(|saved| {
            for entry in &entries {
                forward_store::remove(saved, entry);
            }
        });
    }

    fn remove(&mut self, id: u64) -> Result<Removed, ApiError> {
        let found = self.roots.values_mut().find_map(|root| root.remove(id)).or_else(|| {
            let r = self.remote.remove(&id)?;
            let local = r.forward.status().local_addr.to_string();
            Some((Removed { local, configured: false }, Some(r.entry)))
        });
        let Some((removed, entry)) = found else {
            return Err(ApiError::not_found(format!("no forward {id}")));
        };
        if let Some(entry) = entry {
            self.persist(|entries| {
                forward_store::remove(entries, &entry);
            });
        }
        Ok(removed)
    }

    /// Apply `f` to the store. A failure is logged, never the request's.
    fn persist(&self, f: impl FnOnce(&mut Vec<Entry>)) {
        if let Err(e) = forward_store::update(&self.store, f) {
            (self.log)(&format!("forwards: cannot save {}: {e:#}", self.store.display()));
        }
    }

    /// Recreate the saved ad-hoc forwards: each on its saved host port, else
    /// (taken now) on an automatic one near it, which the store then records.
    /// Entries of instances gone from state, and remote ones of machines no
    /// longer registered, are dropped from the store; a stopped instance's
    /// forward is restored anyway (it heals when the instance runs). Other
    /// failures (a `machines.toml` that doesn't load included) are logged and
    /// the entry kept.
    fn restore(&mut self) {
        let saved = match forward_store::load(&self.store) {
            Ok(saved) if saved.is_empty() => return,
            Ok(saved) => saved,
            Err(e) => return (self.log)(&format!("forwards: cannot restore: {e:#}")),
        };
        let state = match State::load() {
            Ok(state) => state,
            Err(e) => return (self.log)(&format!("forwards: cannot restore: {e:#}")),
        };
        let (keep, dropped) = restorable(saved, &state);
        // (old entry, its replacement; `None` drops it), applied as diffs so a
        // write by an overlapping daemon since the load isn't undone.
        let mut changes: Vec<(Entry, Option<Entry>)> = Vec::new();
        for entry in dropped {
            (self.log)(&format!(
                "forwards: dropping saved {}: instance {} no longer exists",
                describe(&entry),
                entry.instance.as_deref().unwrap_or_default()
            ));
            changes.push((entry, None));
        }
        let registered = match keep.iter().any(|e| e.machine.is_some()).then(|| self.registered()) {
            Some(Err(e)) => {
                (self.log)(&format!("forwards: cannot restore remote forwards: {e:#}"));
                None
            }
            Some(Ok(names)) => Some(names),
            None => None,
        };
        for entry in keep {
            if let Some(machine) = &entry.machine {
                let Some(names) = &registered else { continue };
                if !names.contains(machine) {
                    (self.log)(&format!(
                        "forwards: dropping saved {}: machine `{machine}` is no longer registered",
                        describe(&entry)
                    ));
                    changes.push((entry, None));
                    continue;
                }
            }
            if let Some(now) = self.restore_one(&entry)
                && now != entry
            {
                changes.push((entry, Some(now)));
            }
        }
        if !changes.is_empty() {
            self.persist(|entries| {
                for (old, new) in changes {
                    let Some(i) = entries.iter().position(|e| *e == old) else { continue };
                    match new {
                        Some(new) => entries[i] = new,
                        None => {
                            entries.remove(i);
                        }
                    }
                }
            });
        }
    }

    /// Start one saved forward; the entry as it now runs, `None` when it
    /// couldn't start (logged).
    fn restore_one(&mut self, entry: &Entry) -> Option<Entry> {
        let log = self.log;
        // A machine's root is a path there: nothing to resolve here.
        let dir = match &entry.machine {
            Some(_) => entry.dir.clone(),
            None => match entry.dir.canonicalize() {
                Ok(dir) => dir,
                Err(_) => {
                    log(&format!("forwards: cannot restore {}: no directory {}", describe(entry), entry.dir.display()));
                    return None;
                }
            },
        };
        // Retried briefly before falling back: right after a handoff, or while
        // some process's fork briefly holds the old listener, the saved port
        // frees within moments, and moving the forward would surprise.
        let deadline = std::time::Instant::now() + RESTORE_PORT_WAIT;
        let mut first = self.start_saved(&dir, entry, HostPort::Fixed(entry.host_port));
        while matches!(&first, Err(e) if e.kind() == std::io::ErrorKind::AddrInUse)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
            first = self.start_saved(&dir, entry, HostPort::Fixed(entry.host_port));
        }
        let started = match first {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                self.start_saved(&dir, entry, HostPort::Prefer(entry.host_port)).map(|now| (now, true))
            }
            started => started.map(|now| (now, false)),
        };
        match started {
            Ok((now, fell_back)) => {
                if fell_back {
                    log(&format!(
                        "forwards: restored {} on port {} (port {} is taken)",
                        describe(entry),
                        now.host_port,
                        entry.host_port
                    ));
                } else {
                    log(&format!("forwards: restored {}", describe(entry)));
                }
                Some(now)
            }
            Err(e) => {
                log(&format!("forwards: cannot restore {}: {}", describe(entry), bind_reason(&e)));
                None
            }
        }
    }

    /// Start one saved forward (local in root `dir`, or remote) on
    /// `host_port`; the entry as it now runs.
    fn start_saved(&mut self, dir: &Path, entry: &Entry, host_port: HostPort) -> std::io::Result<Entry> {
        if entry.machine.is_some() {
            return self.start_remote(entry.clone(), host_port).map(|(_, now)| now);
        }
        let mut next_id = self.next_id;
        let started = self.root(dir).start_adhoc(entry.clone(), host_port, &mut next_id);
        self.next_id = next_id;
        started.map(|(_, now)| now)
    }

    /// Drop the remote forwards of machines gone from `machines.toml`, then
    /// reconcile configured forwards for every config root recorded in state
    /// (plus roots that only have ad-hoc forwards, whose configured ones then
    /// stop if their instances went away).
    fn sync(&mut self, running: &[String]) {
        self.drop_unregistered();
        let Ok(state) = State::load() else { return };
        for dir in super::host::config_roots(&state) {
            self.root(&dir);
        }
        let mut next_id = self.next_id;
        let mut notes = Vec::new();
        for root in self.roots.values_mut() {
            let keys = running_keys(&state, running, &root.dir);
            root.sync(&keys, &mut next_id, &mut notes);
        }
        self.next_id = next_id;
        for line in notes {
            self.status(line);
        }
    }

    /// Rebuild the rows and send [`Event::Changed`] only when they changed,
    /// then drain each forward's notes and send the newest as a deduped
    /// [`Event::Status`].
    fn poll(&mut self) {
        let rows = self.rows(None, true);
        self.active.store(rows.len(), Ordering::Release);
        if rows != self.last_rows {
            self.last_rows = rows;
            (self.on_event)(Event::Changed);
        }
        // The newest note per forward, prefixed with the local port, is the one
        // worth showing; dedup so a retry loop doesn't repeat the same line.
        let mut newest: Option<String> = None;
        let local = self.roots.values().flat_map(|r| r.live.values());
        for f in local.chain(self.remote.values().map(|r| &r.forward)) {
            let local_port = f.status().local_addr.port();
            for note in f.drain_notes() {
                newest = Some(format!("{local_port}: {note}"));
            }
        }
        if let Some(line) = newest
            && self.last_status.as_deref() != Some(line.as_str())
        {
            self.last_status = Some(line.clone());
            self.status(line);
        }
    }
}

/// The running instances (state keys) that may belong to config root `root`:
/// recorded there, or recorded nowhere (from before roots were recorded;
/// [`wanted`] then matches them by sandbox name).
fn running_keys(state: &State, running: &[String], root: &Path) -> BTreeSet<String> {
    state
        .instances
        .iter()
        .filter(|(_, i)| running.contains(&i.container))
        .filter(|(_, i)| i.config_dir.as_deref().is_none_or(|d| d == root))
        .map(|(key, _)| key.clone())
        .collect()
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

/// One forward into a machine's instance or service: ad-hoc only (a
/// machine's `forwardPorts` are never mirrored here), bound here, its bridge
/// an ssh running `devsandbox tunnel` there.
struct Remote {
    forward: Forward,
    /// Its store entry (`machine` set): what `forwards.rm` removes.
    entry: Entry,
}

impl Remote {
    fn machine(&self) -> &str {
        self.entry.machine.as_deref().unwrap_or_default()
    }

    fn row(&self, id: u64) -> ForwardRow {
        let mut row = status_to_row(id, &self.entry.dir, &self.forward.status(), false);
        if row.target.is_empty() {
            let e = &self.entry;
            row.target =
                format!("{}/{}", self.machine(), target_label(e.service.as_deref(), e.instance.as_deref(), e.container_port));
        }
        row.machine = Some(self.machine().to_string());
        row
    }
}

/// One config root's forwards.
struct Root {
    dir: PathBuf,
    /// Config-root id (`services::project_id`), the key for global forwards'
    /// saved ports. `None` when `dir` doesn't resolve: no configured forwards.
    project: Option<String>,
    live: BTreeMap<u64, Forward>,
    /// Live ad-hoc forwards' ids -> their store entry, what `forwards.rm`
    /// removes from the store.
    adhoc: BTreeMap<u64, Entry>,
    /// Live configured forwards -> their id in `live`.
    configured: BTreeMap<Configured, u64>,
    /// Configured forwards stopped by `forwards.rm`: not restarted until their
    /// owner stops (drops out of the wanted set) and comes back.
    suppressed: BTreeSet<Configured>,
    /// Configured forwards that failed to start (reported once); retried when
    /// their owner stops and comes back.
    failed: BTreeSet<Configured>,
}

impl Root {
    fn new(dir: PathBuf) -> Self {
        let project = services::project_id(&dir).ok();
        Self {
            dir,
            project,
            live: BTreeMap::new(),
            adhoc: BTreeMap::new(),
            configured: BTreeMap::new(),
            suppressed: BTreeSet::new(),
            failed: BTreeSet::new(),
        }
    }

    fn rows(&self) -> Vec<ForwardRow> {
        let configured: BTreeSet<u64> = self.configured.values().copied().collect();
        self.live
            .iter()
            .map(|(&id, f)| status_to_row(id, &self.dir, &f.status(), configured.contains(&id)))
            .collect()
    }

    fn insert(&mut self, forward: Forward, next_id: &mut u64) -> u64 {
        let id = *next_id;
        *next_id += 1;
        self.live.insert(id, forward);
        id
    }

    /// Start one ad-hoc forward: resolve the instance name non-interactively,
    /// bind the listener. The route resolves on the forward's own thread.
    /// Returns the answer and the store entry to save.
    fn start(&mut self, req: &AddRequest, next_id: &mut u64) -> Result<(Added, Entry), ApiError> {
        let instance_key = match &req.instance {
            Some(name) => {
                let state = State::load().map_err(ApiError::internal)?;
                Some(
                    crate::commands::resolve_instance_noninteractive(&state, name)
                        .map_err(|e| ApiError::not_found(format!("{e:#}")))?,
                )
            }
            None => None,
        };
        let entry = Entry {
            dir: self.dir.clone(),
            instance: instance_key,
            service: req.service.clone(),
            address: Entry::address_of(req.bind),
            container_port: req.container_port,
            host_port: 0,
            machine: None,
        };
        let (id, entry) = self.start_adhoc(entry, req.host_port, next_id).map_err(|e| bind_failed(req.host_port, &e))?;
        let forward = &self.live[&id];
        let local = forward.status().local_addr.to_string();
        let target = route_label_or(forward, req);
        Ok((Added { id, local, target }, entry))
    }

    /// Bind `entry`'s forward on `host_port` (its saved `host_port` is
    /// ignored) and register it as ad-hoc. Returns its id and the entry with
    /// the bound host port.
    fn start_adhoc(&mut self, mut entry: Entry, host_port: HostPort, next_id: &mut u64) -> std::io::Result<(u64, Entry)> {
        entry.dir = self.dir.clone();
        let open = local_opener(port::resolver(self.dir.clone(), entry.instance.clone(), entry.service.clone(), entry.container_port));
        let probe = Box::new(port::listening_procs);
        let forward = Forward::start(ForwardSpec { bind: entry.bind(), host_port, open, probe })?;
        entry.host_port = forward.status().local_addr.port();
        let id = self.insert(forward, next_id);
        self.adhoc.insert(id, entry.clone());
        Ok((id, entry))
    }

    /// Drop the forward with `id` (its `Drop` kills the bridge/`exec`), `None`
    /// when this root has no such forward. A configured forward stays stopped
    /// until its owner restarts instead of coming straight back on the next
    /// sync. An ad-hoc one comes with its store entry.
    fn remove(&mut self, id: u64) -> Option<(Removed, Option<Entry>)> {
        let forward = self.live.remove(&id)?;
        let local = forward.status().local_addr.to_string();
        drop(forward);
        let entry = self.adhoc.remove(&id);
        let configured = self.configured.iter().find(|(_, v)| **v == id).map(|(c, _)| c.clone());
        if let Some(c) = &configured {
            self.configured.remove(c);
            self.suppressed.insert(c.clone());
        }
        Some((Removed { local, configured: configured.is_some() }, entry))
    }

    /// Reconcile configured forwards with the running instances: stop those
    /// no longer wanted, start the missing ones on their saved (or a newly
    /// allocated) host port, and save newly chosen ports. Config or state that
    /// doesn't load leaves everything as it is. Status lines go to `notes`.
    fn sync(&mut self, running: &BTreeSet<String>, next_id: &mut u64, notes: &mut Vec<String>) {
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
        self.suppressed.retain(|c| wanted.contains_key(c));

        // Stale saved ports would block allocation forever; prune before
        // computing reservations.
        let mut dirty = prune_saved(&mut state, &config, &project);
        let mut chosen = Vec::new();
        for (c, fp) in &wanted {
            if self.configured.contains_key(c) || self.suppressed.contains(c) || self.failed.contains(c) {
                continue;
            }
            match self.start_configured(&state, &project, c, fp, next_id) {
                Ok((id, port, label)) => {
                    self.configured.insert(c.clone(), id);
                    notes.push(format!("forwarding 127.0.0.1:{port} -> {label}"));
                    // Later allocations in this pass must see it as taken.
                    dirty |= save_port(&mut state, &project, c, port);
                    chosen.push((c.clone(), port));
                }
                Err(msg) => {
                    self.failed.insert(c.clone());
                    notes.push(msg);
                }
            }
        }
        if !dirty {
            return;
        }
        // Applied to the current file so a `run`/`rm` since the load isn't undone.
        let saved = State::update(|fresh| {
            prune_saved(fresh, &config, &project);
            for (c, port) in &chosen {
                save_port(fresh, &project, c, *port);
            }
            Ok(())
        });
        if let Err(e) = saved {
            notes.push(format!("forwardPorts: {e:#}"));
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
        next_id: &mut u64,
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
                open: local_opener(port::resolver(self.dir.clone(), instance.clone(), fp.service.clone(), fp.port)),
                probe: Box::new(port::listening_procs),
            };
            match Forward::start(spec) {
                Ok(forward) => {
                    let label = match (&fp.service, &instance) {
                        (Some(svc), _) => format!("{svc}:{}", fp.port),
                        (None, Some(key)) => format!("{key}:{}", fp.port),
                        (None, None) => fp.port.to_string(),
                    };
                    return Ok((self.insert(forward, next_id), port, label));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => return Err(format!("forwardPorts `{}`: port {port}: {}", c.entry, bind_reason(&e))),
            }
        }
        Err(format!("forwardPorts `{}`: no free host port from {} up", c.entry, fp.base()))
    }
}

/// Project a `ForwardStatus` into a wire row. Pure, so the state-string
/// mapping is unit-tested without a forwarder.
fn status_to_row(id: u64, dir: &Path, status: &ForwardStatus, configured: bool) -> ForwardRow {
    ForwardRow {
        id,
        dir: dir.to_path_buf(),
        local: status.local_addr.to_string(),
        target: status.route_label.clone(),
        process: status.process.clone(),
        state: state_string(&status.state),
        conns: status.open_conns,
        configured,
        machine: None,
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
/// whole root; sandboxes that don't resolve are skipped.
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

/// The row's state string: `"active"`, `"connecting"`, or `"error: …"`.
fn state_string(state: &ForwardState) -> String {
    match state {
        ForwardState::Active => "active".into(),
        ForwardState::Connecting => "connecting".into(),
        ForwardState::Error(reason) => format!("error: {reason}"),
    }
}

/// `forwards.add`'s error for a failed bind: the immediate, precise one worth
/// naming a port for.
fn bind_failed(host_port: HostPort, e: &std::io::Error) -> ApiError {
    let port = match host_port {
        HostPort::Fixed(p) | HostPort::Prefer(p) => p,
    };
    ApiError::bind_failed(format!("port {port}: {}", bind_reason(e)))
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

/// Split saved entries into those to restore and those whose instance is
/// gone from state (a service-only entry always stays, a remote one too: its
/// instance is in the machine's state, not this one's). Pure over `state`.
fn restorable(entries: Vec<Entry>, state: &State) -> (Vec<Entry>, Vec<Entry>) {
    entries.into_iter().partition(|e| {
        e.machine.is_some() || e.instance.as_ref().is_none_or(|key| state.instances.contains_key(key))
    })
}

/// A forward's best-effort `target:port` before its route resolves.
fn target_label(service: Option<&str>, instance: Option<&str>, port: u16) -> String {
    match (service, instance) {
        (Some(svc), _) => format!("{svc}:{port}"),
        (None, Some(instance)) => format!("{instance}:{port}"),
        (None, None) => port.to_string(),
    }
}

/// A saved forward for `serve.log`: `db:5432 on 127.0.0.1:15432`,
/// `box/web:3000 on 127.0.0.1:3000`.
fn describe(entry: &Entry) -> String {
    let target = target_label(entry.service.as_deref(), entry.instance.as_deref(), entry.container_port);
    let target = match &entry.machine {
        Some(machine) => format!("{machine}/{target}"),
        None => target,
    };
    format!("{target} on {}", std::net::SocketAddr::new(entry.bind(), entry.host_port))
}

/// The route label for `forwards.add`'s answer: the resolved label once the
/// supervisor has published one, else a best-effort `target:port` from the
/// request, `<machine>/target:port` for a remote one (the resolve runs on
/// the forward's thread, so it's usually empty this early).
fn route_label_or(forward: &Forward, req: &AddRequest) -> String {
    let label = forward.status().route_label;
    if !label.is_empty() {
        return label;
    }
    let target = target_label(req.service.as_deref(), req.instance.as_deref(), req.container_port);
    match &req.machine {
        Some(machine) => format!("{machine}/{target}"),
        None => target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::forward::{ForwardState, ForwardStatus};
    use crate::serve::api::Forwarding;
    use crate::state::Instance;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Mutex;

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
    fn running_keys_are_this_roots_and_unrecorded_running_instances() {
        let mut state = State::default();
        let mut add = |key: &str, container: &str, root: Option<&str>| {
            let mut i = inst("api", "p1", &[]);
            i.container = container.into();
            i.config_dir = root.map(PathBuf::from);
            state.instances.insert(key.into(), i);
        };
        add("a", "devsandbox-a", Some("/cfg/a"));
        add("b", "devsandbox-b", Some("/cfg/b"));
        add("old", "devsandbox-old", None);
        add("stopped", "devsandbox-stopped", Some("/cfg/a"));
        let running: Vec<String> = ["devsandbox-a", "devsandbox-b", "devsandbox-old"].map(String::from).into();
        let keys = running_keys(&state, &running, Path::new("/cfg/a"));
        assert_eq!(keys, ["a", "old"].map(String::from).into());
        assert!(running_keys(&state, &[], Path::new("/cfg/a")).is_empty());
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

    // ---- pure: ForwardStatus -> ForwardRow ----

    #[test]
    fn status_to_row_maps_state_strings() {
        let dir = Path::new("/cfg");
        let row = status_to_row(7, dir, &status(ForwardState::Active, Some("node (pid 412)"), 3), true);
        assert_eq!(row.id, 7);
        assert_eq!(row.dir, dir);
        assert_eq!(row.local, "127.0.0.1:3000");
        assert_eq!(row.target, "api:3000");
        assert_eq!(row.process.as_deref(), Some("node (pid 412)"));
        assert_eq!(row.state, "active");
        assert_eq!(row.conns, 3);
        assert!(row.configured);

        assert_eq!(status_to_row(1, dir, &status(ForwardState::Connecting, None, 0), false).state, "connecting");
        assert_eq!(
            status_to_row(1, dir, &status(ForwardState::Error("no such host db".into()), None, 0), false).state,
            "error: no such host db"
        );
        // Unknown process → None (rendered as `-` by the table).
        assert_eq!(status_to_row(1, dir, &status(ForwardState::Connecting, None, 0), false).process, None);
    }

    #[test]
    fn row_wire_shape() {
        let row = status_to_row(7, Path::new("/cfg"), &status(ForwardState::Active, None, 1), false);
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "id": 7, "dir": "/cfg", "local": "127.0.0.1:3000", "target": "api:3000",
                "process": null, "state": "active", "conns": 1, "configured": false,
            })
        );
        assert_eq!(serde_json::from_value::<ForwardRow>(v).unwrap(), row);
    }

    // ---- pure: bind_reason ----

    #[test]
    fn bind_reason_names_addr_in_use() {
        let in_use = std::io::Error::new(std::io::ErrorKind::AddrInUse, "os says busy");
        assert_eq!(bind_reason(&in_use), "address in use");
        let other = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        assert_eq!(bind_reason(&other), "nope");
    }

    /// The `rows != last_rows` gate: identical rows produce no update; a change
    /// (a new state string) does.
    #[test]
    fn rows_changed_gate() {
        let dir = Path::new("/cfg");
        let a = vec![status_to_row(1, dir, &status(ForwardState::Connecting, None, 0), false)];
        let b = a.clone();
        assert_eq!(a, b, "identical rows compare equal (no update sent)");
        let c = vec![status_to_row(1, dir, &status(ForwardState::Active, None, 0), false)];
        assert_ne!(a, c, "a state change makes rows differ (update sent)");
    }

    // ---- the registry thread, without docker ----

    fn quiet(_: &str) {}

    /// A registry on a store of its own (in a fresh temp dir).
    fn registry() -> (Registry, Arc<Mutex<Vec<Event>>>) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        registry_at(&tmp(&format!("store-{n}")).join("forwards.toml"))
    }

    /// `machines.toml` is the store's sibling; remote bridges run [`no_ssh`].
    fn registry_at(store: &Path) -> (Registry, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let reg = Registry::spawn_with(
            quiet,
            store.to_path_buf(),
            store.with_file_name("machines.toml"),
            no_ssh,
            Box::new(move |e| sink.lock().unwrap().push(e)),
        );
        (reg, events)
    }

    /// A remote bridge that dies at once: no ssh to anything.
    fn no_ssh(_: &machines::Entry, _: &Path, _: Option<&str>, _: Option<&str>, _: u16) -> anyhow::Result<Vec<OsString>> {
        Ok(vec!["false".into()])
    }

    /// A fresh empty dir under the test tmp dir.
    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-fwd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request(dir: &Path, instance: Option<&str>, service: Option<&str>) -> AddRequest {
        AddRequest {
            dir: dir.to_path_buf(),
            instance: instance.map(String::from),
            service: service.map(String::from),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            host_port: HostPort::Prefer(0),
            container_port: 5432,
            machine: None,
        }
    }

    /// An unknown instance is `not-found` (state is read, nothing else: no
    /// docker), a missing dir too; dropping the registry joins promptly.
    #[test]
    fn add_unknown_instance_or_dir_is_not_found_then_drop_joins() {
        let dir = std::env::temp_dir();
        let (reg, _) = registry();
        let h = reg.handle();
        // The name is chosen to be absurd: a machine that had it would start it.
        let e = h.add(request(&dir, Some("ghost-instance-xyz"), None)).unwrap_err();
        assert_eq!(e.code, "not-found", "{e:?}");
        let e = h.add(request(Path::new("/nonexistent/devsandbox-fwd"), None, Some("db"))).unwrap_err();
        assert_eq!(e.code, "not-found", "{e:?}");
        assert_eq!(h.rm(42).unwrap_err().code, "not-found");
        drop(reg);
        // Stopped: requests fail instead of hanging.
        assert_eq!(h.list(None, false).unwrap_err().code, "internal");
    }

    /// A service-only forward on a root without a config binds (the route
    /// fails on its own thread: config load, before any docker), lists under
    /// its canonical root, counts as a holder, announces `Changed`, and goes
    /// away on `rm`.
    #[test]
    fn add_list_rm_round_trip_counts_holders_and_announces_changes() {
        let dir = std::env::temp_dir().join(format!("devsandbox-fwd-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let canonical = dir.canonicalize().unwrap();
        let (reg, events) = registry();
        let h = reg.handle();

        let added = h.add(request(&dir, None, Some("db"))).unwrap();
        assert_eq!(added.target, "db:5432");
        assert!(added.local.starts_with("127.0.0.1:"), "{added:?}");
        let rows = h.list(Some(dir.clone()), false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].id, &rows[0].dir, rows[0].configured), (added.id, &canonical, false));
        // Ids only: the row's state moves on its own (the resolver fails here,
        // no config) between the two lists.
        let ids = |rows: &[ForwardRow]| rows.iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(&h.list(None, false).unwrap()), ids(&rows));
        assert!(h.list(Some(PathBuf::from("/nonexistent/other")), false).unwrap().is_empty());
        // A second add gets the next daemon-wide id.
        let second = h.add(request(&dir, None, Some("db"))).unwrap();
        assert_eq!(second.id, added.id + 1);
        // The poll after each command counts them; a request behind it waits.
        h.list(None, false).unwrap();
        assert_eq!(reg.active(), 2);
        assert!(events.lock().unwrap().contains(&Event::Changed));

        events.lock().unwrap().clear();
        assert_eq!(h.rm(added.id).unwrap(), Removed { local: added.local.clone(), configured: false });
        assert_eq!(h.rm(added.id).unwrap_err().code, "not-found");
        assert_eq!(h.list(None, false).unwrap().len(), 1);
        assert_eq!(reg.active(), 1);
        assert!(events.lock().unwrap().contains(&Event::Changed));
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fixed host port another listener holds is `bind-failed`, phrased
    /// like the dashboard's status line was.
    #[test]
    fn a_taken_fixed_port_is_bind_failed() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let (reg, _) = registry();
        let req = AddRequest { host_port: HostPort::Fixed(port), ..request(&std::env::temp_dir(), None, Some("db")) };
        let e = reg.handle().add(req).unwrap_err();
        assert_eq!((e.code, e.message), ("bind-failed", format!("port {port}: address in use")));
        assert!(reg.handle().list(None, false).unwrap().is_empty());
        assert_eq!(reg.active(), 0);
    }

    // ---- persistence (forwards.toml) ----

    fn saved(dir: &Path, instance: Option<&str>, host_port: u16) -> Entry {
        Entry {
            dir: dir.to_path_buf(),
            instance: instance.map(String::from),
            service: Some("db".into()),
            address: None,
            container_port: 5432,
            host_port,
            machine: None,
        }
    }

    fn port_of(local: &str) -> u16 {
        local.parse::<std::net::SocketAddr>().unwrap().port()
    }

    #[test]
    fn restorable_drops_only_entries_of_removed_instances() {
        let mut state = State::default();
        state.instances.insert("api".into(), inst("api", "p1", &[]));
        let dir = Path::new("/cfg");
        let (keep, dropped) =
            restorable(vec![saved(dir, Some("api"), 1), saved(dir, Some("gone"), 2), saved(dir, None, 3)], &state);
        assert_eq!(keep, vec![saved(dir, Some("api"), 1), saved(dir, None, 3)]);
        assert_eq!(dropped, vec![saved(dir, Some("gone"), 2)]);
        assert_eq!(describe(&saved(dir, None, 3)), "db:5432 on 127.0.0.1:3");
        // A machine's instance isn't in this host's state.
        let far = Entry { machine: Some("box".into()), service: None, ..saved(dir, Some("gone"), 4) };
        assert_eq!(restorable(vec![far.clone()], &state), (vec![far.clone()], vec![]));
        assert_eq!(describe(&far), "box/gone:5432 on 127.0.0.1:4");
    }

    // ---- remote forwards (machines.toml) ----

    const MACHINES: &str = "[machine.box]\ntarget = \"me@box.invalid\"\nroots = [\"/far/cfg\"]\n";

    /// A forward into `box`'s `web` (a root that doesn't exist here: it's
    /// never resolved locally).
    fn remote_request() -> AddRequest {
        AddRequest { machine: Some("box".into()), ..request(Path::new("/far/cfg"), Some("web"), None) }
    }

    fn remote_saved(host_port: u16) -> Entry {
        Entry { machine: Some("box".into()), service: None, ..saved(Path::new("/far/cfg"), Some("web"), host_port) }
    }

    #[test]
    fn a_remote_add_needs_a_registered_machine() {
        let store = tmp("remote-unknown").join("forwards.toml");
        let (reg, _) = registry_at(&store);
        let e = reg.handle().add(remote_request()).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), ("not-found", "no machine named `box` (devsandbox machine ls)"));
        assert!(reg.handle().list(None, true).unwrap().is_empty());
        assert_eq!(forward_store::load(&store).unwrap(), vec![]);
    }

    /// A remote forward binds here, lists only with `machines` (whatever
    /// `dir`), counts as a holder, persists with its machine, comes back on
    /// its port in a successor, and goes away on `rm`.
    #[test]
    fn remote_forwards_list_persist_restore_and_rm() {
        let store = tmp("remote").join("forwards.toml");
        std::fs::write(store.with_file_name("machines.toml"), MACHINES).unwrap();
        let (reg, _) = registry_at(&store);
        let h = reg.handle();
        let added = h.add(remote_request()).unwrap();
        // The bridge never answers (no header): the best effort stays.
        assert_eq!(added.target, "box/web:5432");
        assert!(h.list(None, false).unwrap().is_empty(), "local only by default");
        let rows = h.list(Some(std::env::temp_dir()), true).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(
            (row.id, row.dir.as_path(), row.local.as_str(), row.target.as_str(), row.machine.as_deref(), row.configured),
            (added.id, Path::new("/far/cfg"), added.local.as_str(), "box/web:5432", Some("box"), false)
        );
        assert_eq!(reg.active(), 1);
        let port = port_of(&added.local);
        assert_eq!(forward_store::load(&store).unwrap(), vec![remote_saved(port)]);
        drop(reg);

        let (successor, _) = registry_at(&store);
        let h = successor.handle();
        let rows = h.list(None, true).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!((rows[0].local.as_str(), rows[0].machine.as_deref()), (added.local.as_str(), Some("box")));
        assert_eq!(forward_store::load(&store).unwrap(), vec![remote_saved(port)], "restored as saved");
        assert_eq!(h.rm(rows[0].id).unwrap(), Removed { local: added.local.clone(), configured: false });
        assert!(h.list(None, true).unwrap().is_empty());
        assert_eq!(successor.active(), 0);
        assert_eq!(forward_store::load(&store).unwrap(), vec![]);
    }

    /// A saved remote forward of a machine no longer registered isn't
    /// restored and leaves the store.
    #[test]
    fn restore_drops_forwards_of_unregistered_machines() {
        let store = tmp("remote-restore").join("forwards.toml");
        forward_store::update(&store, |e| e.push(remote_saved(1))).unwrap();
        let (reg, _) = registry_at(&store);
        assert!(reg.handle().list(None, true).unwrap().is_empty());
        assert_eq!(forward_store::load(&store).unwrap(), vec![]);
    }

    /// `machine rm`: what each sync runs first drops the machine's forwards
    /// and their store entries, and only theirs. Called on a [`Daemon`]
    /// directly: a whole sync also reconciles this host's real state.
    #[test]
    fn a_removed_machines_forwards_are_dropped() {
        let store = tmp("remote-drop").join("forwards.toml");
        let machines_toml = store.with_file_name("machines.toml");
        let two = format!("{MACHINES}[machine.other]\ntarget = \"other.invalid\"\n");
        std::fs::write(&machines_toml, &two).unwrap();
        let changed = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&changed);
        let on_event: OnEvent = Box::new(move |e| {
            if e == Event::Changed {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        });
        let mut d = Daemon::new(quiet, store.clone(), machines_toml.clone(), no_ssh, on_event, Arc::default());
        let a = d.add(remote_request()).unwrap();
        let b = d.add(AddRequest { machine: Some("other".into()), ..remote_request() }).unwrap();
        d.poll();
        assert_eq!(d.rows(None, true).len(), 2);

        d.drop_unregistered();
        assert_eq!(d.rows(None, true).len(), 2, "both still registered");
        std::fs::write(&machines_toml, two.replace(MACHINES, "")).unwrap();
        let before = changed.load(Ordering::Relaxed);
        d.drop_unregistered();
        d.poll();
        let rows = d.rows(None, true);
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [b.id]);
        assert_ne!(rows[0].id, a.id);
        assert_eq!(changed.load(Ordering::Relaxed), before + 1);
        let left = forward_store::load(&store).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].machine.as_deref(), Some("other"));
        // A registry that doesn't load drops nothing.
        std::fs::write(&machines_toml, "not toml [").unwrap();
        d.drop_unregistered();
        assert_eq!(d.rows(None, true).len(), 1);
    }

    /// An add saves the bound port (an automatic one here), an rm removes only
    /// its entry, stopping the registry leaves the file alone, and a successor
    /// on the same store brings the forward back on the same host port.
    #[test]
    fn adds_persist_rm_removes_its_entry_and_a_successor_restores_them() {
        let root = tmp("persist-root");
        let canonical = root.canonicalize().unwrap();
        let store = tmp("persist").join("forwards.toml");
        let (reg, _) = registry_at(&store);
        let h = reg.handle();
        let a = h.add(request(&root, None, Some("db"))).unwrap();
        let b = h.add(request(&root, None, Some("db"))).unwrap();
        assert_eq!(
            forward_store::load(&store).unwrap(),
            vec![saved(&canonical, None, port_of(&a.local)), saved(&canonical, None, port_of(&b.local))]
        );
        h.rm(a.id).unwrap();
        assert_eq!(forward_store::load(&store).unwrap(), vec![saved(&canonical, None, port_of(&b.local))]);

        let text = std::fs::read_to_string(&store).unwrap();
        drop(reg);
        assert_eq!(std::fs::read_to_string(&store).unwrap(), text, "stopping is not a removal");

        let (successor, _) = registry_at(&store);
        let rows = successor.handle().list(None, false).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!((rows[0].local.as_str(), &rows[0].dir, rows[0].configured), (b.local.as_str(), &canonical, false));
        assert_eq!(std::fs::read_to_string(&store).unwrap(), text, "restored as saved: no rewrite");
        // The restored forward is removable like any other.
        successor.handle().rm(rows[0].id).unwrap();
        assert_eq!(forward_store::load(&store).unwrap(), vec![]);
        drop(successor);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A saved port that's taken now: restored on an automatic port, which
    /// the store then records. An entry whose instance is gone from state is
    /// dropped (the name is chosen to be absurd, as above).
    #[test]
    fn restore_falls_back_from_a_taken_port_and_drops_removed_instances() {
        let root = tmp("restore-root").canonicalize().unwrap();
        let store = tmp("restore").join("forwards.toml");
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let ghost = saved(&root, Some("ghost-instance-xyz"), 1);
        forward_store::update(&store, |e| e.extend([saved(&root, None, port), ghost])).unwrap();

        let (reg, _) = registry_at(&store);
        let rows = reg.handle().list(None, false).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let now = port_of(&rows[0].local);
        assert_ne!(now, port);
        assert_eq!(forward_store::load(&store).unwrap(), vec![saved(&root, None, now)]);
        drop(reg);
        drop(taken);
        let _ = std::fs::remove_dir_all(&root);
    }
}
