//! The shared poll's snapshots (docs/reference/api.md `instances.watch`):
//! dashboards watch a config root, and while at least one does, the host poll
//! (`host.rs`) asks the runtime once per tick, joins every watched root
//! against that one pass, and the daemon pushes each root's snapshot as
//! `instances.snapshot`. With nothing watched, nothing is collected.
//!
//! Here: the per-root cache (the last pushed params, plus each running
//! container's last cpu/mem so a listing-only tick still pushes a complete
//! snapshot) and the cadence, as pure fns over [`Instant`]s. Who watches
//! what lives with the connections (`daemon.rs`'s `Pending`); the poll reads
//! it each tick. The join is injected ([`Join`]), so tests never ask the
//! runtime or read the user's state.
//!
//! The cache is keyed by config root only: this host's rows. Machines
//! (docs/architecture/daemon/machines.md, *Federated snapshots*) are the
//! second dimension, [`Remotes`]: each registered machine's link state and
//! its last snapshot per remote root, kept while its link is down. A watcher
//! with `machines: true` gets [`merge`] of a local root's params and every
//! machine's rows; any other watcher gets the local params untouched.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::api::InstancesList;
use super::capacity::Capacity;
use super::machines::{Entry, MachineKind};
use crate::snapshot::{ContainerStatus, Depth, RuntimeView, Snapshot};

/// The poll's tick while anything is watched: the dashboard's listing
/// cadence.
pub const LISTING_INTERVAL: Duration = Duration::from_secs(2);
/// At most one stats collection per interval; the ticks in between are
/// listing-only. `stats --no-stream` samples every running container twice,
/// the bulk of the runtime load a dashboard causes.
pub const STATS_INTERVAL: Duration = Duration::from_secs(7);

/// One root's snapshot from one runtime pass (`snapshot::collect_from` plus
/// `thread_children` in production).
pub type Join = Arc<dyn Fn(&Path, &RuntimeView) -> InstancesList + Send + Sync>;

/// Container name → its last (cpu, mem).
type Usage = BTreeMap<String, (Option<String>, Option<String>)>;

/// The cache, shared by the poll (writes) and the daemon (a new watch reads
/// it).
#[derive(Default)]
pub struct Snapshots {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    roots: BTreeMap<PathBuf, Root>,
    /// The last pass that asked for stats.
    last_stats: Option<Instant>,
    /// This host's capacity, set by the poll before each collection.
    capacity: Option<Value>,
}

/// One root's entry. Its rows are this host's; a machine's join here later,
/// beside them.
struct Root {
    /// The last `instances.snapshot` params pushed for it.
    pushed: Arc<Value>,
    usage: Usage,
    /// It has had a snapshot with stats.
    stats: bool,
}

impl Snapshots {
    /// The last snapshot pushed for `dir` (`instances.snapshot` params), if
    /// it's still watched.
    pub fn cached(&self, dir: &Path) -> Option<Arc<Value>> {
        self.lock().roots.get(dir).map(|r| Arc::clone(&r.pushed))
    }

    /// Whether this tick asks for stats: see [`stats_due`].
    pub fn stats_due(&self, watched: &BTreeSet<PathBuf>, now: Instant) -> bool {
        let inner = self.lock();
        let fresh = watched.iter().any(|d| !inner.roots.get(d).is_some_and(|r| r.stats));
        stats_due(inner.last_stats, now, fresh)
    }

    /// Drop the roots nobody watches (each tick, before [`collect`]): a
    /// cached snapshot is only pushed while it's kept fresh.
    ///
    /// [`collect`]: Self::collect
    pub fn prune(&self, watched: &BTreeSet<PathBuf>) {
        self.lock().roots.retain(|dir, _| watched.contains(dir));
    }

    /// The roots in `watched` the poll hasn't collected yet: a new watch's
    /// root gets a listing pushed before the tick's stats pass.
    pub fn uncached(&self, watched: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
        let inner = self.lock();
        watched.iter().filter(|d| !inner.roots.contains_key(*d)).cloned().collect()
    }

    /// This host's capacity: every params [`collect`](Self::collect)
    /// returns from now on carries it as `capacity` (one host-wide value,
    /// the same in every root's snapshot).
    pub fn set_capacity(&self, capacity: &Capacity) {
        self.lock().capacity = serde_json::to_value(capacity).ok();
    }

    /// Join each of `roots` against `view`, carry cpu/mem over a
    /// listing-only pass, cache, and return each root's params to push
    /// (with the [`set_capacity`](Self::set_capacity) value, if any).
    /// Every collection is returned, changed or not, so a watcher can judge
    /// staleness by arrival.
    pub fn collect(
        &self,
        view: &RuntimeView,
        roots: &BTreeSet<PathBuf>,
        join: &(dyn Fn(&Path, &RuntimeView) -> InstancesList + Send + Sync),
    ) -> Vec<(PathBuf, Arc<Value>)> {
        if view.depth == Depth::Full {
            self.lock().last_stats = Some(view.at);
        }
        let mut out = Vec::new();
        for dir in roots {
            // The join reads config and state: not under the lock a new
            // watch reads the cache through.
            let prev = self.lock().roots.get(dir).map(|r| r.usage.clone()).unwrap_or_default();
            let mut list = join(dir, view);
            carry_over(&mut list.snapshot, &prev);
            let usage = usage(&list.snapshot);
            let stats = list.snapshot.stats;
            let capacity = self.lock().capacity.clone();
            let Some(pushed) = params_with(dir, &list, capacity) else { continue };
            let mut inner = self.lock();
            let had = inner.roots.get(dir).is_some_and(|r| r.stats);
            inner.roots.insert(dir.clone(), Root { pushed: Arc::clone(&pushed), usage, stats: had || stats });
            out.push((dir.clone(), pushed));
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `instances.snapshot`'s params: `{"dir", …InstancesList}`. `None` only if
/// it won't serialize (it always does).
pub fn params(dir: &Path, list: &InstancesList) -> Option<Arc<Value>> {
    params_with(dir, list, None)
}

/// [`params`] plus `capacity` when there's one.
fn params_with(dir: &Path, list: &InstancesList, capacity: Option<Value>) -> Option<Arc<Value>> {
    let mut value = serde_json::to_value(list).ok()?;
    let obj = value.as_object_mut()?;
    obj.insert("dir".into(), Value::String(dir.to_string_lossy().into_owned()));
    if let Some(capacity) = capacity {
        obj.insert("capacity".into(), capacity);
    }
    Some(Arc::new(value))
}

/// Stats run at most every [`STATS_INTERVAL`] (`last`: the last pass that
/// asked), and at once when a watched root has never had them (`fresh`), so
/// a new dashboard's cpu/mem show on its first full tick.
pub fn stats_due(last: Option<Instant>, now: Instant, fresh: bool) -> bool {
    fresh || last.is_none_or(|last| now >= last + STATS_INTERVAL)
}

/// Where a machine's link stands (`machines.list`'s and a federated
/// snapshot's `state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkState {
    /// Not attempted yet: every link is held from the start
    /// (docs/architecture/daemon/machines.md, *Held links: `Machines`*), so
    /// only until its thread's first attempt (or where no service runs).
    #[default]
    Idle,
    Connecting,
    Connected,
    /// The last attempt failed or the link was lost: `error` says why.
    Unreachable,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Idle => "idle",
            LinkState::Connecting => "connecting",
            LinkState::Connected => "connected",
            LinkState::Unreachable => "unreachable",
        }
    }
}

/// One registered machine as federation sees it.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub name: String,
    /// The registry's kind: ssh, or an offered fleet worker.
    pub kind: MachineKind,
    /// The ssh target; `None` for an offered machine.
    pub target: Option<String>,
    /// The registry's `command` (the remote devsandbox), for an ssh argv.
    pub command: Option<String>,
    /// The registry's `agent`, for the dashboard to show.
    pub agent: bool,
    /// The registered roots, in order: the first one shows the instances
    /// whose root isn't registered.
    pub roots: Vec<PathBuf>,
    pub state: LinkState,
    pub error: Option<String>,
    /// Its devsandbox semver, from the last hello.
    pub version: Option<String>,
    /// Remote root → the last `instances.snapshot` params it pushed. Kept
    /// while the link is down: last known (a hub also saves them,
    /// `machines::saved`).
    pub rows: BTreeMap<PathBuf, Arc<Value>>,
    /// Unix seconds of the last push stored in `rows`.
    pub seen: Option<u64>,
    /// `rows` were restored from a hub's `remotes.json` and the link hasn't
    /// connected since: shown `unreachable` ([`Machine::set_link`]).
    pub restored: bool,
}

impl Machine {
    /// Its host's capacity, from the first of its roots' pushed snapshots
    /// that carries one (every root's is the same host's, pushed on the same
    /// poll).
    pub fn capacity(&self) -> Option<Capacity> {
        self.roots.iter().find_map(|r| serde_json::from_value(self.rows.get(r)?.get("capacity")?.clone()).ok())
    }

    /// Record the link's `state` and `error`; whether what's shown changed.
    /// A [`restored`](Self::restored) machine shows `unreachable` with the
    /// rows' time until the link connects; meanwhile `connecting` changes
    /// nothing, so a retrying link doesn't flap.
    pub fn set_link(&mut self, state: LinkState, error: Option<String>) -> bool {
        let (state, error) = match (self.restored, state) {
            (false, _) => (state, error),
            (true, LinkState::Connected) => {
                self.restored = false;
                (state, error)
            }
            (true, LinkState::Connecting) => return false,
            (true, _) => (LinkState::Unreachable, Some(restored_error(error.as_deref(), self.seen))),
        };
        let moved = self.state != state || self.error != error;
        self.state = state;
        self.error = error;
        moved
    }
}

/// A restored machine's `error`: why its link is down (or that it hasn't
/// reconnected), and when its rows are from.
pub fn restored_error(why: Option<&str>, seen: Option<u64>) -> String {
    let why = why.unwrap_or("not reconnected since this daemon started");
    match seen {
        Some(at) => format!("{why} (rows from {} UTC)", crate::inbox::clock::stamp(at, 0)),
        None => why.to_string(),
    }
}

/// Every registered machine's [`Machine`], by name. The machine links write
/// it, the daemon reads it to merge; only short in-memory work under its lock.
#[derive(Default)]
pub struct Remotes {
    inner: Mutex<BTreeMap<String, Machine>>,
}

impl Remotes {
    /// Follow the registry: new machines start [`LinkState::Idle`], removed
    /// ones go with their rows, and a root no longer registered loses its.
    pub fn sync(&self, entries: &[Entry]) {
        let mut inner = self.lock();
        inner.retain(|name, _| entries.iter().any(|e| e.name == *name));
        for e in entries {
            let m = inner.entry(e.name.clone()).or_insert_with(|| Machine { name: e.name.clone(), ..Machine::default() });
            (m.kind, m.target) = e.kind.wire();
            m.command.clone_from(&e.command);
            m.agent = e.agent;
            m.roots.clone_from(&e.roots);
            m.rows.retain(|root, _| e.roots.contains(root));
        }
    }

    /// Put machines restored from a hub's `remotes.json` in (before the
    /// first [`sync`](Self::sync), which drops the ones no longer
    /// registered); a machine already here is left alone.
    pub fn restore(&self, machines: Vec<Machine>) {
        let mut inner = self.lock();
        for m in machines {
            inner.entry(m.name.clone()).or_insert(m);
        }
    }

    /// Change machine `name` (if registered) under the lock.
    pub fn update(&self, name: &str, f: impl FnOnce(&mut Machine)) {
        if let Some(m) = self.lock().get_mut(name) {
            f(m);
        }
    }

    pub fn get(&self, name: &str) -> Option<Machine> {
        self.lock().get(name).cloned()
    }

    /// Every machine, sorted by name (the rows are `Arc`s: cheap).
    pub fn all(&self) -> Vec<Machine> {
        self.lock().values().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Machine>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One `machines` entry of a federated snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMachine {
    pub name: String,
    pub kind: MachineKind,
    /// `None` for an offered machine (no ssh to it).
    pub target: Option<String>,
    pub state: LinkState,
    pub error: Option<String>,
    pub version: Option<String>,
    /// The remote's runtime (`runtime_name`), once a snapshot came.
    pub runtime: Option<String>,
    /// The remote devsandbox (`None`: `devsandbox`), so the dashboard builds
    /// a terminal's ssh argv (`ssh::tty_argv`) with no call.
    pub command: Option<String>,
    /// The registry's `agent`: this host's ssh-agent is relayed there.
    pub agent: bool,
    /// Its host's capacity, from its last pushed snapshot (last known while
    /// the link is down); `None` before one carried it.
    #[serde(default)]
    pub capacity: Option<Capacity>,
}

/// A `machines: true` watcher's `instances.snapshot`: `local` (a local
/// root's params) as is, then each machine's instances, sandboxes and
/// services tagged `machine` (sandboxes and services also `root`, the remote
/// root they were read from), plus a top-level `machines` with every
/// machine's link state.
///
/// Each remote instance appears once per machine: every remote root's
/// snapshot lists all of that host's instances, so an instance is taken
/// from the snapshot of its own `root` when that root is registered, else
/// from the machine's first root's. Remote `thread_children` aren't merged:
/// the Inbox is this host's only.
pub fn merge(local: &Value, machines: &[Machine]) -> Value {
    let mut out = local.clone();
    let Some(obj) = out.as_object_mut() else { return out };
    let mut instances = take_array(obj, "instances");
    let mut sandboxes = take_array(obj, "sandboxes");
    let mut services = take_array(obj, "services");
    for m in machines {
        for (i, root) in m.roots.iter().enumerate() {
            let Some(params) = m.rows.get(root) else { continue };
            let root_text = root.to_string_lossy();
            for row in rows(params, "instances") {
                let own = row.get("root").and_then(Value::as_str).map(Path::new);
                let here = match own {
                    Some(own) if m.roots.iter().any(|r| r == own) => own == root,
                    _ => i == 0,
                };
                if here {
                    instances.push(tag(row, &m.name, None));
                }
            }
            sandboxes.extend(rows(params, "sandboxes").map(|row| tag(row, &m.name, Some(&root_text))));
            services.extend(rows(params, "services").map(|row| tag(row, &m.name, Some(&root_text))));
        }
    }
    obj.insert("instances".into(), Value::Array(instances));
    // The header's "N sandboxes" counts the machines' too.
    obj.insert("sandbox_count".into(), json!(sandboxes.len()));
    obj.insert("sandboxes".into(), Value::Array(sandboxes));
    obj.insert("services".into(), Value::Array(services));
    let entries: Vec<Value> = machines.iter().map(|m| json!(snapshot_machine(m))).collect();
    obj.insert("machines".into(), Value::Array(entries));
    out
}

fn snapshot_machine(m: &Machine) -> SnapshotMachine {
    let runtime = m
        .roots
        .iter()
        .find_map(|r| m.rows.get(r)?.get("runtime_name")?.as_str().map(str::to_string));
    SnapshotMachine {
        capacity: m.capacity(),
        name: m.name.clone(),
        kind: m.kind,
        target: m.target.clone(),
        state: m.state,
        error: m.error.clone(),
        version: m.version.clone(),
        runtime,
        command: m.command.clone(),
        agent: m.agent,
    }
}

fn take_array(obj: &mut Map<String, Value>, key: &str) -> Vec<Value> {
    match obj.remove(key) {
        Some(Value::Array(rows)) => rows,
        _ => Vec::new(),
    }
}

fn rows<'a>(params: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    params.get(key).and_then(Value::as_array).into_iter().flatten()
}

fn tag(row: &Value, machine: &str, root: Option<&str>) -> Value {
    let mut row = row.clone();
    if let Some(obj) = row.as_object_mut() {
        obj.insert("machine".into(), machine.into());
        if let Some(root) = root {
            obj.insert("root".into(), root.into());
        }
    }
    row
}

/// A remote root's push carrying no runtime pass yet (every instance's
/// status `unknown`: the first, disk-depth push after a link's
/// (re)connect) keeps each container's status, cpu and mem from `prev`,
/// that root's last push, so remote rows don't flicker to `unknown` on
/// every reconnect. Any other push is taken as is.
pub fn carry_remote_status(prev: &Value, next: &mut Value) {
    let unknown = |row: &Value| row.pointer("/status/state").and_then(Value::as_str) == Some("unknown");
    let Some(next_rows) = next.get_mut("instances").and_then(Value::as_array_mut) else { return };
    if next_rows.is_empty() || !next_rows.iter().all(unknown) {
        return;
    }
    let before: BTreeMap<&str, &Value> =
        rows(prev, "instances").filter_map(|r| Some((r.get("container")?.as_str()?, r))).collect();
    for row in next_rows {
        let Some(old) = row.get("container").and_then(Value::as_str).and_then(|c| before.get(c)) else { continue };
        if unknown(old) {
            continue;
        }
        let Some(obj) = row.as_object_mut() else { continue };
        for key in ["status", "cpu", "mem"] {
            if let Some(v) = old.get(key) {
                obj.insert(key.into(), v.clone());
            }
        }
    }
}

/// A snapshot without stats keeps the last cpu/mem of containers still
/// running, so the columns don't blank between stats passes (what the
/// dashboard did itself before).
fn carry_over(snapshot: &mut Snapshot, prev: &Usage) {
    if snapshot.stats {
        return;
    }
    for row in &mut snapshot.instances {
        if !matches!(row.status, ContainerStatus::Running(_)) {
            continue;
        }
        if let Some((cpu, mem)) = prev.get(&row.container) {
            row.cpu.clone_from(cpu);
            row.mem.clone_from(mem);
        }
    }
}

/// What the next pass carries over: the running containers' cpu/mem as
/// pushed (a stopped container's are dropped, as the dashboard did).
fn usage(snapshot: &Snapshot) -> Usage {
    snapshot
        .instances
        .iter()
        .filter(|r| matches!(r.status, ContainerStatus::Running(_)))
        .map(|r| (r.container.clone(), (r.cpu.clone(), r.mem.clone())))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::snapshot::InstanceRow;

    #[test]
    fn stats_are_due_every_interval_or_for_a_fresh_root() {
        let t0 = Instant::now();
        assert!(stats_due(None, t0, false));
        assert!(!stats_due(Some(t0), t0 + Duration::from_secs(2), false));
        assert!(stats_due(Some(t0), t0 + Duration::from_secs(2), true));
        assert!(!stats_due(Some(t0), t0 + STATS_INTERVAL - Duration::from_millis(1), false));
        assert!(stats_due(Some(t0), t0 + STATS_INTERVAL, false));
    }

    pub(crate) fn row(container: &str, running: bool, cpu: Option<&str>) -> InstanceRow {
        let status = if running { ContainerStatus::Running("Up".into()) } else { ContainerStatus::Exited("Exited".into()) };
        let json = serde_json::json!({
            "name": container, "sandbox": "s", "container": container, "status": status,
            "uptime_secs": 0, "cpu": cpu, "mem": cpu.map(|c| format!("{c}MiB")), "folder": "/w",
            "worktree": false, "services": [], "workspace": "/w", "remote_user": null,
            "remote_env_len": 0, "base_folder": "/w", "drift": false, "instance_id": container,
            "dispatcher": null, "done": false, "phase": null,
        });
        serde_json::from_value(json).unwrap()
    }

    pub(crate) fn snapshot(instances: Vec<InstanceRow>, stats: bool) -> Snapshot {
        let json = serde_json::json!({
            "instances": instances, "sandboxes": [], "services": [], "sandbox_count": 0,
            "runtime_name": "docker", "runtime_version": null, "error": null,
        });
        let mut s: Snapshot = serde_json::from_value(json).unwrap();
        s.stats = stats;
        s
    }

    #[test]
    fn listing_only_snapshots_carry_running_cpu_mem_over() {
        let full = snapshot(vec![row("a", true, Some("1%")), row("b", true, Some("2%"))], true);
        let prev = usage(&full);
        // b stopped meanwhile; c is new.
        let mut listing = snapshot(vec![row("a", true, None), row("b", false, None), row("c", true, None)], false);
        carry_over(&mut listing, &prev);
        let cpu: Vec<_> = listing.instances.iter().map(|r| r.cpu.as_deref()).collect();
        assert_eq!(cpu, [Some("1%"), None, None]);
        assert_eq!(listing.instances[0].mem.as_deref(), Some("1%MiB"));
        // b's stop drops its usage: a restart before the next stats shows none.
        let prev = usage(&listing);
        let mut restarted = snapshot(vec![row("b", true, None)], false);
        carry_over(&mut restarted, &prev);
        assert_eq!(restarted.instances[0].cpu, None);
        // A stats pass is taken as is.
        let mut fresh = snapshot(vec![row("a", true, None)], true);
        carry_over(&mut fresh, &usage(&full));
        assert_eq!(fresh.instances[0].cpu, None);
    }

    /// A remote root's pushed params: its instances (name, root), one sandbox
    /// and one service named after the root's last component.
    fn remote(dir: &str, instances: &[(&str, Option<&str>)]) -> Arc<Value> {
        let rows: Vec<Value> = instances.iter().map(|(n, r)| serde_json::json!({"name": n, "root": r})).collect();
        let tag = Path::new(dir).file_name().unwrap().to_string_lossy().into_owned();
        Arc::new(serde_json::json!({
            "dir": dir, "instances": rows, "sandboxes": [{"name": format!("sb-{tag}"), "root": "/elsewhere"}],
            "services": [{"name": format!("svc-{tag}")}], "runtime_name": "podman", "thread_children": {"x": {}},
        }))
    }

    fn machine(name: &str, roots: &[&str], state: LinkState) -> Machine {
        Machine {
            name: name.into(),
            target: Some(format!("me@{name}")),
            roots: roots.iter().map(PathBuf::from).collect(),
            state,
            ..Machine::default()
        }
    }

    fn names(v: &Value, key: &str) -> Vec<String> {
        v[key].as_array().unwrap().iter().map(|r| format!("{}@{}", r["name"].as_str().unwrap(), r["machine"].as_str().unwrap_or("-"))).collect()
    }

    #[test]
    fn merge_tags_remote_rows_and_takes_each_instance_once() {
        let local = serde_json::json!({
            "dir": "/here", "instances": [{"name": "mine", "root": "/here"}], "sandboxes": [{"name": "s"}],
            "services": [], "runtime_name": "docker", "thread_children": {"local": {}},
        });
        let mut a = machine("a", &["/r/one", "/r/two"], LinkState::Connected);
        a.version = Some("0.6.1".into());
        // Every root's snapshot lists all of the machine's instances, and
        // carries its host's capacity.
        let all = [("in-one", Some("/r/one")), ("in-two", Some("/r/two")), ("orphan", Some("/r/gone")), ("old", None)];
        let capacity = Capacity { cpus: Some(8), running: 2, lending: Some(crate::serve::fleet::lend::State::On), ..Capacity::default() };
        let with_capacity = |dir: &str| {
            let mut p = (*remote(dir, &all)).clone();
            p["capacity"] = serde_json::to_value(&capacity).unwrap();
            Arc::new(p)
        };
        a.rows.insert("/r/one".into(), with_capacity("/r/one"));
        a.rows.insert("/r/two".into(), with_capacity("/r/two"));
        // Down: its last rows stay, with its state.
        let mut b = machine("b", &["/b"], LinkState::Unreachable);
        b.error = Some("b: Connection refused".into());
        b.rows.insert("/b".into(), remote("/b", &[("bee", Some("/b"))]));
        // Never reached: its state only.
        let mut c = machine("c", &["/c"], LinkState::Unreachable);
        c.error = Some("c: Permission denied".into());
        c.command = Some("~/bin/devsandbox".into());
        // An offered worker: no target.
        let w = Machine { kind: MachineKind::Offered, target: None, ..machine("w", &[], LinkState::Idle) };

        let merged = merge(&local, &[a, b, c, w]);
        // From its own root's snapshot when registered, else the first's.
        assert_eq!(names(&merged, "instances"), ["mine@-", "in-one@a", "orphan@a", "old@a", "in-two@a", "bee@b"]);
        assert_eq!(names(&merged, "sandboxes"), ["s@-", "sb-one@a", "sb-two@a", "sb-b@b"]);
        assert_eq!(merged["sandbox_count"], 4);
        assert_eq!(names(&merged, "services"), ["svc-one@a", "svc-two@a", "svc-b@b"]);
        // Sandboxes and services say the remote root they came from; an
        // instance keeps its own (null included).
        assert_eq!(merged["sandboxes"][2]["root"], "/r/two");
        assert_eq!(merged["services"][0]["root"], "/r/one");
        assert_eq!(merged["instances"][3]["root"], Value::Null);
        assert!(merged["instances"][0].get("machine").is_none(), "local rows untouched");
        // The local rest as is: no remote thread_children.
        assert_eq!((&merged["dir"], &merged["runtime_name"]), (&serde_json::json!("/here"), &serde_json::json!("docker")));
        assert_eq!(merged["thread_children"], serde_json::json!({"local": {}}));
        let cap = serde_json::to_value(&capacity).unwrap();
        assert_eq!(
            merged["machines"],
            serde_json::json!([
                {"name": "a", "kind": "ssh", "target": "me@a", "state": "connected", "error": null, "version": "0.6.1", "runtime": "podman", "command": null, "agent": false, "capacity": cap},
                {"name": "b", "kind": "ssh", "target": "me@b", "state": "unreachable", "error": "b: Connection refused", "version": null, "runtime": "podman", "command": null, "agent": false, "capacity": null},
                {"name": "c", "kind": "ssh", "target": "me@c", "state": "unreachable", "error": "c: Permission denied", "version": null, "runtime": null, "command": "~/bin/devsandbox", "agent": false, "capacity": null},
                {"name": "w", "kind": "offered", "target": null, "state": "idle", "error": null, "version": null, "runtime": null, "command": null, "agent": false, "capacity": null},
            ])
        );
        crate::serve::dts::assert_matches("SnapshotMachine", &merged["machines"][0]);
        crate::serve::dts::assert_matches("Capacity", &merged["machines"][0]["capacity"]);
        // No machines: the local params plus an empty `machines`.
        let alone = merge(&local, &[]);
        let mut want = local.clone();
        want["machines"] = serde_json::json!([]);
        want["sandbox_count"] = serde_json::json!(want["sandboxes"].as_array().unwrap().len());
        assert_eq!(alone, want);
    }

    #[test]
    fn the_first_roots_snapshot_missing_drops_its_unregistered_instances_for_now() {
        let mut a = machine("a", &["/r/one", "/r/two"], LinkState::Connecting);
        a.rows.insert("/r/two".into(), remote("/r/two", &[("in-one", Some("/r/one")), ("in-two", Some("/r/two")), ("old", None)]));
        let merged = merge(&serde_json::json!({"instances": [], "sandboxes": [], "services": []}), &[a]);
        // `in-one` waits for its own root's snapshot; `old` for the first's.
        assert_eq!(names(&merged, "instances"), ["in-two@a"]);
    }

    #[test]
    fn remotes_follow_the_registry() {
        let remotes = Remotes::default();
        let entry = |name: &str, roots: &[&str]| Entry { roots: roots.iter().map(PathBuf::from).collect(), ..Entry::ssh(name, name) };
        remotes.sync(&[entry("a", &["/1", "/2"]), entry("b", &["/b"]), Entry::offered("w")]);
        let w = remotes.get("w").unwrap();
        assert_eq!((w.kind, w.target), (MachineKind::Offered, None));
        assert_eq!(remotes.get("a").unwrap().target.as_deref(), Some("a"));
        remotes.update("a", |m| {
            m.state = LinkState::Connected;
            m.rows.insert("/1".into(), Arc::new(Value::Null));
            m.rows.insert("/2".into(), Arc::new(Value::Null));
        });
        remotes.update("nope", |_| panic!("not registered"));
        remotes.sync(&[entry("a", &["/2"])]);
        let all = remotes.all();
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].state, all[0].rows.keys().collect::<Vec<_>>()), (LinkState::Connected, vec![&PathBuf::from("/2")]));
        assert!(remotes.get("b").is_none());
    }

    #[test]
    fn a_disk_depth_remote_push_keeps_the_last_statuses() {
        let unknown = |c: &str| serde_json::json!({"container": c, "status": {"state": "unknown"}, "cpu": null, "mem": null});
        let prev = serde_json::json!({"instances": [
            {"container": "a", "status": {"state": "running", "text": "Up"}, "cpu": "1%", "mem": "2MiB"},
            {"container": "b", "status": {"state": "unknown"}, "cpu": null, "mem": null},
        ]});
        let mut next = serde_json::json!({"instances": [unknown("a"), unknown("b"), unknown("new")]});
        carry_remote_status(&prev, &mut next);
        assert_eq!(next["instances"][0]["status"], serde_json::json!({"state": "running", "text": "Up"}));
        assert_eq!((&next["instances"][0]["cpu"], &next["instances"][0]["mem"]), (&"1%".into(), &"2MiB".into()));
        assert_eq!(next["instances"][1]["status"]["state"], "unknown");
        assert_eq!(next["instances"][2]["status"]["state"], "unknown");

        // A push with one real status is a runtime pass: taken as is.
        let mut real = serde_json::json!({"instances": [unknown("a"), {"container": "b", "status": {"state": "missing"}}]});
        let want = real.clone();
        carry_remote_status(&prev, &mut real);
        assert_eq!(real, want);
    }

    fn view(depth: Depth) -> RuntimeView {
        RuntimeView {
            depth,
            ps: Vec::new(),
            stats: BTreeMap::new(),
            runtime_name: "docker".into(),
            runtime_version: None,
            errors: Vec::new(),
            at: Instant::now(),
        }
    }

    #[test]
    fn collect_joins_watched_roots_caches_and_carries_over() {
        let cache = Snapshots::default();
        let (a, b) = (PathBuf::from("/cfg/a"), PathBuf::from("/cfg/b"));
        let both: BTreeSet<PathBuf> = [a.clone(), b.clone()].into();
        let join = |_: &Path, v: &RuntimeView| {
            let full = v.depth == Depth::Full;
            InstancesList {
                snapshot: snapshot(vec![row("x", true, full.then_some("5%"))], full),
                thread_children: Default::default(),
            }
        };
        let t0 = Instant::now();
        assert!(cache.stats_due(&both, t0), "fresh roots");
        let first = view(Depth::Full);
        let pushed = cache.collect(&first, &both, &join);
        assert_eq!(pushed.iter().map(|(d, _)| d).collect::<Vec<_>>(), [&a, &b]);
        assert_eq!(pushed[0].1["dir"], "/cfg/a");
        assert_eq!(pushed[0].1["instances"][0]["cpu"], "5%");
        assert!(!cache.stats_due(&both, first.at + Duration::from_secs(2)));

        assert!(pushed[0].1.get("capacity").is_none(), "no capacity set yet");

        // A listing-only tick still pushes, cpu carried; with this host's
        // capacity in every root's params.
        let capacity = Capacity { cpus: Some(4), running: 1, ..Capacity::default() };
        cache.set_capacity(&capacity);
        let pushed = cache.collect(&view(Depth::Listing), &both, &join);
        assert_eq!(pushed.len(), 2);
        assert_eq!(pushed[1].1["instances"][0]["cpu"], "5%");
        assert_eq!(cache.cached(&b).unwrap()["instances"][0]["cpu"], "5%");
        for (_, p) in &pushed {
            assert_eq!(p["capacity"], serde_json::to_value(&capacity).unwrap());
        }
        assert_eq!(cache.cached(&a).unwrap()["capacity"]["cpus"], 4);
        crate::serve::dts::assert_matches("Capacity", &pushed[0].1["capacity"]);

        // A new root has never had stats: due at once.
        let c = PathBuf::from("/cfg/c");
        let three: BTreeSet<PathBuf> = [a.clone(), c.clone()].into();
        assert!(cache.stats_due(&three, first.at + Duration::from_secs(2)));
        assert_eq!(cache.uncached(&three), [c.clone()].into());
        // Unwatched roots leave the cache.
        cache.prune(&three);
        cache.collect(&view(Depth::Listing), &three, &join);
        assert!(cache.cached(&b).is_none());
        assert!(cache.cached(&c).is_some());
        cache.prune(&BTreeSet::new());
        assert!(cache.cached(&a).is_none());
    }
}
