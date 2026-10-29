//! Inbound neighbors (docs/architecture/daemon/machines.md, *Inbound
//! neighbors*): the machines whose link to this host is up, dialed from
//! their end (`machine add <this host>` run there), as their `fleet-link
//! --peer <dialer>` here announces them (`links.inbound`). Memory only,
//! never in `machines.toml`: a neighbor lasts while the connection that
//! announced it does ([`Guard`]). The newest announce of a name wins, as
//! the link's sockets do: an older connection's close doesn't take a newer
//! one's neighbor away.
//!
//! Each is a machine here while it's up (fleet plan decision 28): the
//! service (`service::Machines`) holds a link to it over `fleet-<name>.sock`
//! and drops it when it goes, told by the hook [`Neighbors::on_change`]
//! sets. Its roots are those its daemon answered `roots.list` with over the
//! link, filled in by the daemon after the announce. A name is either a
//! registry entry or an inbound neighbor, never both: `links.inbound`
//! refuses an entry's name, `machines.add` and `fleet.offer` a neighbor's.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::serve::api::ApiError;

/// One inbound neighbor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neighbor {
    pub name: String,
    /// Its link `Hello` (`fleet::update::hello_of`: build, platform, hub flag).
    pub version: String,
    /// It said it's the fleet's hub.
    pub hub: bool,
    /// Its config roots (`roots.list` over the link); empty until answered.
    pub roots: Vec<PathBuf>,
    /// Which announce this is: what its [`Guard`] removes.
    seq: u64,
}

/// What [`Neighbors`] calls once a neighbor came, went or got its roots:
/// never under its lock (it reads [`Neighbors::list`]).
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// The neighbors that are up now.
#[derive(Default)]
pub struct Neighbors {
    live: Mutex<BTreeMap<String, Neighbor>>,
    seq: AtomicU64,
    on_change: Mutex<Option<OnChange>>,
}

impl std::fmt::Debug for Neighbors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Neighbors").field("live", &self.live).finish_non_exhaustive()
    }
}

/// One announce, held by the connection that made it: dropping it removes
/// its neighbor unless a newer announce of the name replaced it.
#[derive(Debug)]
pub struct Guard {
    neighbors: Arc<Neighbors>,
    name: String,
    seq: u64,
}

impl Guard {
    /// What sets its neighbor's roots once its daemon answered, from a
    /// thread of its own: nothing once a newer announce replaced it (or it
    /// is gone). It doesn't keep the neighbor up.
    pub fn roots_setter(&self) -> impl FnOnce(Vec<PathBuf>) + Send + 'static {
        let (neighbors, name, seq) = (std::sync::Arc::downgrade(&self.neighbors), self.name.clone(), self.seq);
        move |roots| {
            if let Some(n) = neighbors.upgrade() {
                n.set_roots(&name, seq, roots);
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let removed = {
            let mut live = self.neighbors.live.lock().unwrap();
            live.get(&self.name).is_some_and(|n| n.seq == self.seq) && live.remove(&self.name).is_some()
        };
        if removed {
            self.neighbors.changed();
        }
    }
}

impl Neighbors {
    /// Announce `name` (replacing an older announce of it); up until the
    /// guard drops.
    pub fn announce(self: &Arc<Self>, name: &str, version: &str, hub: bool) -> Guard {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let neighbor = Neighbor { name: name.into(), version: version.into(), hub, roots: Vec::new(), seq };
        self.live.lock().unwrap().insert(name.into(), neighbor);
        self.changed();
        Guard { neighbors: Arc::clone(self), name: name.into(), seq }
    }

    /// Call `f` after every change from now on (the one hook: a second
    /// call replaces it).
    pub fn on_change(&self, f: OnChange) {
        *self.on_change.lock().unwrap() = Some(f);
    }

    fn changed(&self) {
        let hook = self.on_change.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn set_roots(&self, name: &str, seq: u64, roots: Vec<PathBuf>) {
        let moved = match self.live.lock().unwrap().get_mut(name).filter(|n| n.seq == seq) {
            Some(n) if n.roots != roots => {
                n.roots = roots;
                true
            }
            _ => false,
        };
        if moved {
            self.changed();
        }
    }

    /// Whether `name` is up now.
    pub fn contains(&self, name: &str) -> bool {
        self.live.lock().unwrap().contains_key(name)
    }

    /// Every neighbor up now, by name.
    pub fn list(&self) -> Vec<Neighbor> {
        self.live.lock().unwrap().values().cloned().collect()
    }
}

/// The refusal of a registry entry (`machines.add`, `fleet.offer`) named
/// like inbound neighbor `name`.
pub fn taken(name: &str) -> ApiError {
    ApiError::denied(format!(
        "`{name}` is linked to this host from its end (an inbound neighbor): a machine here needs another name"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_neighbor_lasts_while_its_newest_announce_is_held() {
        let n = Arc::new(Neighbors::default());
        let old = n.announce("lap", "0.7.0 a dev linux/x86_64 member", false);
        assert!(n.contains("lap"));
        old.roots_setter()(vec!["/r".into()]);
        assert_eq!(n.list()[0].roots, [PathBuf::from("/r")]);
        // A newer announce replaces it; the old one's late roots and close don't touch it.
        let new = n.announce("lap", "0.7.0 b dev linux/x86_64 hub", true);
        old.roots_setter()(vec!["/old".into()]);
        drop(old);
        let listed = n.list();
        assert_eq!((listed.len(), listed[0].hub, listed[0].roots.is_empty()), (1, true, true));
        assert_eq!(listed[0].version, "0.7.0 b dev linux/x86_64 hub");
        let set = new.roots_setter();
        set(vec!["/new".into()]);
        assert_eq!(n.list()[0].roots, [PathBuf::from("/new")]);
        let other = n.announce("srv", "v", false);
        assert_eq!(n.list().iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["lap", "srv"]);
        drop(new);
        assert!(!n.contains("lap"));
        drop(other);
        assert!(n.list().is_empty());
        assert_eq!(taken("lap").code, "denied");
    }

    /// The hook fires after each change, outside the lock (it can list),
    /// and not for what changes nothing.
    #[test]
    fn the_hook_follows_changes() {
        let n = Arc::new(Neighbors::default());
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let (weak, log) = (Arc::downgrade(&n), Arc::clone(&seen));
        n.on_change(Arc::new(move || {
            let now = weak.upgrade().map(|n| n.list().into_iter().map(|n| format!("{}{:?}", n.name, n.roots)).collect()).unwrap_or_default();
            log.lock().unwrap().push(now);
        }));
        let lap = n.announce("lap", "v", false);
        lap.roots_setter()(vec!["/r".into()]);
        lap.roots_setter()(vec!["/r".into()]);
        let old = n.announce("srv", "v", false);
        let new = n.announce("srv", "v", false);
        drop(old);
        drop((lap, new));
        let seen = seen.lock().unwrap().clone();
        let want: Vec<Vec<&str>> =
            vec![vec!["lap[]"], vec!["lap[\"/r\"]"], vec!["lap[\"/r\"]", "srv[]"], vec!["lap[\"/r\"]", "srv[]"], vec!["srv[]"], vec![]];
        assert_eq!(seen, want.into_iter().map(|v| v.into_iter().map(String::from).collect::<Vec<_>>()).collect::<Vec<_>>());
    }
}
