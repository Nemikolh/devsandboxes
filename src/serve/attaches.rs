//! Who attached to what, last (docs/architecture/daemon/fleet.md, *Last
//! attach*): what routes an instance's `open` record to the member whose
//! user is at it, and what that member checks before it opens one for an
//! instance on another host. In memory only: a successor daemon starts
//! with none, as a dropped attach connection ends its record.
//!
//! Two maps under one lock:
//!
//! - **inbound**, on the instance's host: per instance id, the [`Origin`]
//!   of the latest attach to begin ([`Attaches::begin_in`]): a local
//!   `exec`, dashboard tab or `devsandbox vscode` ([`Origin::Here`]), a
//!   `PTY` stream's proven origin member ([`Origin::Member`]). The latest
//!   begin wins whatever the others do: an older origin's attaches ending
//!   later never clobber a newer one's (each guard keeps the epoch it
//!   joined). One entry per instance id ever attached, never pruned: that
//!   is bounded by the instances this host had in a daemon's life.
//! - **outgoing**, on the attaching member: per `(machine, instance)` it
//!   attached to there ([`Attaches::begin_out`]), whether an attach is live
//!   or when the last one ended ([`Attaches::attached_out`]). An entry
//!   ended longer than [`OPEN_ATTACH_WINDOW`] ago is pruned on each begin.
//!
//! Pure over `Instant`s: an outgoing begin and the checks take `now`, a
//! [`Guard`] stamps its end when dropped (or at [`Guard::end`]'s `now`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::open::OPEN_ATTACH_WINDOW;

/// Where an attach to an instance on this host came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// This host's own user: a local `exec`, a dashboard tab, `devsandbox
    /// vscode` (`instances.attach` without `machine`).
    Here,
    /// A fleet member, by name: a `PTY` stream's proven origin
    /// (`fleet::pty::serve`'s `Peer`; a routed stream's origin, not its
    /// relay).
    Member(String),
}

/// An instance's latest attach on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Latest {
    origin: Origin,
    /// Live attaches of `origin` since it became the latest.
    live: usize,
    /// When the last of them ended, once none is live.
    ended: Option<Instant>,
    /// The begin that made `origin` the latest: a guard of an earlier one
    /// no longer counts.
    epoch: u64,
}

/// An attach this host made to an instance on another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Out {
    live: usize,
    ended: Option<Instant>,
}

#[derive(Debug, Default)]
struct Maps {
    inbound: HashMap<String, Latest>,
    outgoing: HashMap<(String, String), Out>,
    next_epoch: u64,
}

/// The daemon's attach records (one per daemon, in its `Shared`; clones
/// share it).
#[derive(Debug, Clone, Default)]
pub struct Attaches(Arc<Mutex<Maps>>);

/// One attach while it lasts: dropped (or [`Guard::end`]ed), it ends.
#[must_use = "an attach ends when its guard drops"]
#[derive(Debug)]
pub struct Guard {
    maps: Arc<Mutex<Maps>>,
    key: Key,
    done: bool,
}

#[derive(Debug)]
enum Key {
    In { id: String, epoch: u64 },
    Out { machine: String, instance: String },
}

fn lock(maps: &Mutex<Maps>) -> MutexGuard<'_, Maps> {
    maps.lock().unwrap_or_else(|e| e.into_inner())
}

impl Attaches {
    /// An attach to instance `id` (its `instance_id`) on this host from
    /// `origin` begins: the latest now. The same origin as the latest
    /// joins it; another replaces it.
    pub fn begin_in(&self, id: &str, origin: Origin) -> Guard {
        let mut m = lock(&self.0);
        m.next_epoch += 1;
        let fresh = m.next_epoch;
        let entry = m.inbound.entry(id.to_string()).or_insert_with(|| Latest { origin: origin.clone(), live: 0, ended: None, epoch: fresh });
        if entry.origin != origin {
            *entry = Latest { origin, live: 0, ended: None, epoch: fresh };
        }
        entry.live += 1;
        entry.ended = None;
        let epoch = entry.epoch;
        Guard { maps: Arc::clone(&self.0), key: Key::In { id: id.to_string(), epoch }, done: false }
    }

    /// Where the latest attach to instance `id` came from, live or ended;
    /// `None` when none was seen.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latest(&self, id: &str) -> Option<Origin> {
        lock(&self.0).inbound.get(id).map(|l| l.origin.clone())
    }

    /// Whether an attach of instance `id`'s latest origin is live.
    #[cfg(test)]
    pub fn live_in(&self, id: &str) -> bool {
        lock(&self.0).inbound.get(id).is_some_and(|l| l.live > 0)
    }

    /// This host attaches to instance `instance` (an exact name or id, as
    /// its caller named it) on machine `machine`. Entries ended past
    /// [`OPEN_ATTACH_WINDOW`] are pruned first.
    pub fn begin_out(&self, machine: &str, instance: &str, now: Instant) -> Guard {
        let mut m = lock(&self.0);
        m.outgoing.retain(|_, o| o.live > 0 || o.ended.is_some_and(|e| now.saturating_duration_since(e) < OPEN_ATTACH_WINDOW));
        let o = m.outgoing.entry((machine.to_string(), instance.to_string())).or_insert(Out { live: 0, ended: None });
        o.live += 1;
        o.ended = None;
        Guard { maps: Arc::clone(&self.0), key: Key::Out { machine: machine.to_string(), instance: instance.to_string() }, done: false }
    }

    /// Whether this host is attached to `instance` on `machine`, or was
    /// until less than `window` before `now`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn attached_out(&self, machine: &str, instance: &str, now: Instant, window: Duration) -> bool {
        let m = lock(&self.0);
        let Some(o) = m.outgoing.get(&(machine.to_string(), instance.to_string())) else { return false };
        o.live > 0 || o.ended.is_some_and(|e| now.saturating_duration_since(e) < window)
    }
}

impl Guard {
    /// End the attach at `now` (dropping it ends it at `Instant::now()`).
    #[cfg(test)]
    pub fn end(mut self, now: Instant) {
        self.finish(now);
    }

    fn finish(&mut self, now: Instant) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let mut m = lock(&self.maps);
        match &self.key {
            Key::In { id, epoch } => {
                if let Some(l) = m.inbound.get_mut(id)
                    && l.epoch == *epoch
                {
                    l.live = l.live.saturating_sub(1);
                    if l.live == 0 {
                        l.ended = Some(now);
                    }
                }
            }
            Key::Out { machine, instance } => {
                if let Some(o) = m.outgoing.get_mut(&(machine.clone(), instance.clone())) {
                    o.live = o.live.saturating_sub(1);
                    if o.live == 0 {
                        o.ended = Some(now);
                    }
                }
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.finish(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = OPEN_ATTACH_WINDOW;

    fn member(name: &str) -> Origin {
        Origin::Member(name.into())
    }

    fn inbound(a: &Attaches, id: &str) -> Option<(usize, Option<Instant>)> {
        lock(&a.0).inbound.get(id).map(|l| (l.live, l.ended))
    }

    #[test]
    fn the_latest_begin_wins_and_an_older_origin_ending_never_clobbers_it() {
        let (a, t0) = (Attaches::default(), Instant::now());
        assert_eq!(a.latest("id-1"), None);
        let lap = a.begin_in("id-1", member("lap"));
        assert_eq!((a.latest("id-1"), inbound(&a, "id-1")), (Some(member("lap")), Some((1, None))));
        let here = a.begin_in("id-1", Origin::Here);
        assert_eq!(a.latest("id-1"), Some(Origin::Here), "a new begin replaces the origin");
        lap.end(t0 + Duration::from_secs(2));
        assert_eq!((a.latest("id-1"), inbound(&a, "id-1")), (Some(Origin::Here), Some((1, None))), "the older one's end doesn't count");
        let at = t0 + Duration::from_secs(3);
        here.end(at);
        assert_eq!((a.latest("id-1"), inbound(&a, "id-1")), (Some(Origin::Here), Some((0, Some(at)))), "ended, still the latest");

        // The same origin joins; the last of its attaches ends it.
        let one = a.begin_in("id-1", member("lap"));
        let two = a.begin_in("id-1", member("lap"));
        assert_eq!(inbound(&a, "id-1"), Some((2, None)));
        one.end(t0);
        assert_eq!(inbound(&a, "id-1"), Some((1, None)));
        drop(two);
        assert!(matches!(inbound(&a, "id-1"), Some((0, Some(_)))), "a drop stamps its end");
        assert_eq!(a.latest("id-2"), None, "per instance");
    }

    #[test]
    fn an_outgoing_attach_counts_while_live_and_for_the_window_after() {
        let (a, t0) = (Attaches::default(), Instant::now());
        assert!(!a.attached_out("box", "web-1", t0, W));
        let g = a.begin_out("box", "web-1", t0);
        let g2 = a.begin_out("box", "web-1", t0);
        assert!(a.attached_out("box", "web-1", t0 + 2 * W, W), "live: any time");
        assert!(!a.attached_out("box", "web-2", t0, W), "another instance");
        assert!(!a.attached_out("srv", "web-1", t0, W), "another machine");
        g.end(t0);
        assert!(a.attached_out("box", "web-1", t0 + 2 * W, W), "one still live");
        g2.end(t0);
        assert!(a.attached_out("box", "web-1", t0 + W - Duration::from_millis(1), W), "ended, within the window");
        assert!(!a.attached_out("box", "web-1", t0 + W, W), "the window's edge: out");
    }

    #[test]
    fn a_begin_prunes_outgoing_entries_past_the_window() {
        let (a, t0) = (Attaches::default(), Instant::now());
        a.begin_out("box", "old", t0).end(t0);
        a.begin_out("box", "recent", t0).end(t0 + W);
        let _live = a.begin_out("box", "live", t0);
        let _g = a.begin_out("srv", "new", t0 + W + Duration::from_secs(1));
        let keys: std::collections::BTreeSet<String> = lock(&a.0).outgoing.keys().map(|(m, i)| format!("{m}/{i}")).collect();
        assert_eq!(keys, ["box/live", "box/recent", "srv/new"].map(String::from).into(), "the old one is gone");
    }
}
