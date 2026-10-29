//! Inbound neighbors (docs/architecture/daemon/machines.md, *Inbound
//! neighbors*): the machines whose link to this host is up, dialed from
//! their end (`machine add <this host>` run there), as their `fleet-link
//! --peer <dialer>` here announces them (`links.inbound`). Memory only,
//! never in `machines.toml`: a neighbor lasts while the connection that
//! announced it does ([`Guard`]). The newest announce of a name wins, as
//! the link's sockets do: an older connection's close doesn't take a newer
//! one's neighbor away.
//!
//! `machines.list` shows them (with the roots their daemon answered
//! `roots.list` with over the link, filled in by the daemon after the
//! announce), and a name is either a registry entry or an inbound neighbor,
//! never both: `links.inbound` refuses an entry's name, `machines.add` and
//! `fleet.offer` a neighbor's.

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

/// The neighbors that are up now.
#[derive(Debug, Default)]
pub struct Neighbors {
    live: Mutex<BTreeMap<String, Neighbor>>,
    seq: AtomicU64,
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
        let mut live = self.neighbors.live.lock().unwrap();
        if live.get(&self.name).is_some_and(|n| n.seq == self.seq) {
            live.remove(&self.name);
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
        Guard { neighbors: Arc::clone(self), name: name.into(), seq }
    }

    fn set_roots(&self, name: &str, seq: u64, roots: Vec<PathBuf>) {
        if let Some(n) = self.live.lock().unwrap().get_mut(name).filter(|n| n.seq == seq) {
            n.roots = roots;
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
}
