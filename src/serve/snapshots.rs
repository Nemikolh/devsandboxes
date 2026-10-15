//! The shared poll's snapshots (docs/api.md `instances.watch`): dashboards
//! watch a config root, and while at least one does, the host poll
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
//! Keyed by config root only. Machines (docs/client-server.md *Machines*)
//! will add a remote machine's rows to a root's entry beside this host's
//! ([`Root`]), merged when pushed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::api::InstancesList;
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

    /// Join each of `roots` against `view`, carry cpu/mem over a
    /// listing-only pass, cache, and return each root's params to push.
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
            let Some(pushed) = params(dir, &list) else { continue };
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
    let mut value = serde_json::to_value(list).ok()?;
    value.as_object_mut()?.insert("dir".into(), Value::String(dir.to_string_lossy().into_owned()));
    Some(Arc::new(value))
}

/// Stats run at most every [`STATS_INTERVAL`] (`last`: the last pass that
/// asked), and at once when a watched root has never had them (`fresh`), so
/// a new dashboard's cpu/mem show on its first full tick.
pub fn stats_due(last: Option<Instant>, now: Instant, fresh: bool) -> bool {
    fresh || last.is_none_or(|last| now >= last + STATS_INTERVAL)
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

        // A listing-only tick still pushes, cpu carried.
        let pushed = cache.collect(&view(Depth::Listing), &both, &join);
        assert_eq!(pushed.len(), 2);
        assert_eq!(pushed[1].1["instances"][0]["cpu"], "5%");
        assert_eq!(cache.cached(&b).unwrap()["instances"][0]["cpu"], "5%");

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
