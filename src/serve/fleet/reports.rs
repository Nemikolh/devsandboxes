//! The published link graph (docs/architecture/daemon/fleet.md, *Link
//! graph*; fleet plan step 10b.2, decision 59): this host's own report built
//! from its links ([`Tracker`], pure), signed and sent to the hub
//! (`fleet.report`); every member's exchanges of the reports it holds
//! (`fleet.graph`): on a link coming up, and a flood of each change to
//! its linked peers ([`Flood`]), all on one `serve-fleet-graph` thread
//! ([`Publisher`]). [`Published`] is the graph a daemon holds, in memory
//! only: a restarted daemon rebuilds it from the reports its peers hold.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::serve::api::{ApiError, parse};
use crate::serve::fleet::graph::{self, Graph, LinkReport, Report};
use crate::serve::fleet::identity::Statement;
use crate::serve::fleet::member::{self, Hub};
use crate::serve::fleet::membership::Stored;
use crate::serve::fleet::{grants, membership_api, proof};
use crate::serve::machines::MachineKind;
use crate::serve::machines::service::Handle;
use crate::serve::snapshots::{LinkState, Machine};

/// How long after a link change this host's report waits for more changes
/// to join it (a link coming up often moves several states at once).
pub const DEBOUNCE: Duration = Duration::from_millis(1500);

/// How long the thread sleeps with nothing due: a stop is noticed by then
/// at the latest (it's also woken).
const IDLE_WAIT: Duration = Duration::from_secs(1);

/// A report as it travels: `{statement, report}` (`fleet.report`'s params,
/// each of `fleet.graph`'s).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signed {
    pub statement: Statement,
    pub report: Report,
}

/// `fleet.graph`'s params and answer, `graph.get`'s answer: reports, by
/// member name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reports {
    #[serde(default)]
    pub reports: Vec<Signed>,
}

/// `fleet.report`'s answer: whether the hub's graph took it as new.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Took {
    pub changed: bool,
}

/// One link as this host's report says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tracked {
    dials: bool,
    state: LinkState,
    since_ms: u64,
}

/// This host's links as its report says them, and when the next report is
/// due (pure: the caller passes the time).
#[derive(Debug, Default)]
pub struct Tracker {
    links: BTreeMap<String, Tracked>,
    due: Option<Instant>,
}

/// Each machine's link: its name, whether this host dials it (an ssh
/// machine: this host has a shell there; an inbound one dialed this host)
/// and its state.
pub fn links_of(machines: &[Machine]) -> Vec<(String, bool, LinkState)> {
    machines.iter().map(|m| (m.name.clone(), m.kind == MachineKind::Ssh, m.state)).collect()
}

impl Tracker {
    /// `links` ([`links_of`]) as they are at `at` (`now_ms` on the clock).
    /// A peer new or gone, a link that came up or went down, or one whose
    /// `dials` changed makes a report due [`DEBOUNCE`] after the first such
    /// change (later ones join it); `since_ms` is when the link last came
    /// up or went down. A down link moving between `idle`, `connecting`
    /// and `unreachable` keeps the state first recorded: an edge is only up
    /// or not, and a retrying link doesn't flap. The peers whose links came
    /// up (or are new and up).
    pub fn observe(&mut self, links: &[(String, bool, LinkState)], at: Instant, now_ms: u64) -> Vec<String> {
        let before = self.links.len();
        self.links.retain(|name, _| links.iter().any(|(n, ..)| n == name));
        let mut changed = self.links.len() != before;
        let mut up = Vec::new();
        for (peer, dials, state) in links {
            let is_up = *state == LinkState::Connected;
            let fresh = Tracked { dials: *dials, state: *state, since_ms: now_ms };
            match self.links.get_mut(peer) {
                Some(t) if (t.state == LinkState::Connected) == is_up => {
                    if t.dials != *dials {
                        t.dials = *dials;
                        changed = true;
                    }
                    continue;
                }
                Some(t) => *t = fresh,
                None => {
                    self.links.insert(peer.clone(), fresh);
                }
            }
            changed = true;
            if is_up {
                up.push(peer.clone());
            }
        }
        if changed && self.due.is_none() {
            self.due = Some(at + DEBOUNCE);
        }
        up
    }

    /// A report due at `at`, whatever was pending (the hub link came up).
    pub fn now(&mut self, at: Instant) {
        self.due = Some(at);
    }

    /// When the next report is due, if one is.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Whether a report is due by `at`; taking it clears it.
    pub fn take_due(&mut self, at: Instant) -> bool {
        let due = self.due.is_some_and(|d| d <= at);
        if due {
            self.due = None;
        }
        due
    }

    /// `member`'s report of the links whose peer `known` (a member of the
    /// log or a held introduction: no registry entry outside the fleet),
    /// and of `agent`, the members it grants its agent to.
    pub fn report(&self, member: &str, trust: crate::worker_policy::Trust, always_on: bool, agent: Vec<String>, known: impl Fn(&str) -> bool) -> Report {
        let links = self
            .links
            .iter()
            .filter(|(peer, _)| peer.as_str() != member && known(peer))
            .map(|(peer, t)| LinkReport { peer: peer.clone(), dials: t.dials, state: t.state, since_ms: t.since_ms })
            .collect();
        Report { member: member.to_string(), trust, always_on, links, agent }
    }
}

/// `reports` taken into `graph` against `stored` one by one, the graph
/// pruned first (a log changed since drops what no longer resolves). A
/// report of `me`'s (its own, back from a peer after this daemon restarted)
/// moves `last` past it, so the next one signed is newer. Whether any
/// changed the graph, and each refusal (older ones included: not fatal,
/// each verifies alone).
pub fn take_into(graph: &mut Graph, last: &mut u64, stored: &Stored, me: &str, reports: &[Signed]) -> (bool, Vec<String>) {
    let mut changed = graph.prune(stored);
    let mut refused = Vec::new();
    for s in reports {
        match graph.apply(&s.statement, &s.report, stored) {
            Ok(c) => {
                changed |= c;
                if s.report.member == me {
                    *last = (*last).max(s.statement.version);
                }
            }
            Err(e) => refused.push(format!("{e:#}")),
        }
    }
    (changed, refused)
}

/// The graph a daemon holds, and the `serve-fleet-graph` thread's work.
#[derive(Default)]
pub struct Published {
    state: Mutex<State>,
    cv: Condvar,
    /// Called on each [`poke`](Self::poke) and each change of the graph
    /// (the daemon's route sockets follow both: `fleet::routes`).
    on_change: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct State {
    graph: Graph,
    /// The version of this host's last report (signed here, or taken back
    /// from a peer): the next one is past it.
    last: u64,
    tracker: Tracker,
    /// The links changed since the thread last looked.
    dirty: bool,
    /// Peers to exchange reports with (`fleet.graph`): each link that came up.
    exchange: BTreeSet<String>,
    /// The graph changed: send it on to every linked peer ([`Flood`]).
    push: Flood,
    stop: bool,
}

/// The graph's changes not yet sent on, and who the flood skips (pure): a
/// host whose graph took something newer sends every report it holds to
/// each peer whose link is up (`fleet.graph`), as a `fleet.sync` floods a
/// log. Only strictly newer reports change a graph, so the flood ends once
/// every host connected holds the newest ones. Skipped: the one peer every
/// change since the last flood came from (it holds them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Flood {
    /// Nothing changed.
    #[default]
    None,
    /// Every change came from this peer.
    From(String),
    /// Changes from several peers, or this host's own report: to every one.
    All,
}

impl Flood {
    /// A change taken from `from` (`None`: this host's own).
    pub fn add(&mut self, from: Option<&str>) {
        *self = match (std::mem::take(self), from) {
            (Flood::None, Some(f)) => Flood::From(f.to_string()),
            (Flood::From(p), Some(f)) if p == f => Flood::From(p),
            _ => Flood::All,
        };
    }

    /// Whether a flood is due.
    pub fn due(&self) -> bool {
        *self != Flood::None
    }

    /// Whether the flood sends to `peer`.
    pub fn sends_to(&self, peer: &str) -> bool {
        match self {
            Flood::None => false,
            Flood::From(p) => p != peer,
            Flood::All => true,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Published {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A machine's state or rows changed (`service::Hooks::changed`): the
    /// thread looks at the links. Doesn't block.
    pub fn poke(&self) {
        self.lock().dirty = true;
        self.cv.notify_all();
        self.changed();
    }

    /// This host's report due at once, whatever its links did (its trust
    /// changed: `member.set`; an agent grant: `machines.set {agent}`), and
    /// the thread woken. Doesn't block.
    pub fn republish(&self) {
        self.lock().tracker.now(Instant::now());
        self.cv.notify_all();
    }

    /// `f` called on each poke and graph change from now on (once: a
    /// second one is ignored). Must not block.
    pub fn on_change(&self, f: Box<dyn Fn() + Send + Sync>) {
        let _ = self.on_change.set(f);
    }

    fn changed(&self) {
        if let Some(f) = self.on_change.get() {
            f();
        }
    }

    /// The graph held now.
    pub fn graph(&self) -> Graph {
        self.lock().graph.clone()
    }

    /// [`take_into`] this daemon's graph; a change is sent on to its other
    /// linked peers ([`Flood`]). Refusals logged with `from`.
    pub fn take(&self, stored: &Stored, me: &str, reports: &[Signed], from: &str, log: fn(&str)) -> bool {
        let mut st = self.lock();
        let State { graph, last, .. } = &mut *st;
        let (changed, refused) = take_into(graph, last, stored, me, reports);
        if changed {
            st.push.add(Some(from));
            self.cv.notify_all();
        }
        drop(st);
        if changed {
            self.changed();
        }
        for why in refused {
            log(&format!("link reports from {from}: refused one: {why}"));
        }
        changed
    }

    /// Every report held, by member name; pruned against `stored` first
    /// when there's one.
    pub fn reports(&self, stored: Option<&Stored>) -> Reports {
        let mut st = self.lock();
        if let Some(stored) = stored {
            st.graph.prune(stored);
        }
        let reports = st.graph.reports().map(|(s, r)| Signed { statement: s.clone(), report: r.clone() }).collect();
        Reports { reports }
    }
}

fn to_value(x: impl Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(x).map_err(|e| ApiError::internal(e.into()))
}

/// `fleet.report {statement, report}` on the hub, from link peer `from`
/// (admitted, `lent::admit_fleet`): its own report only, taken into the
/// hub's graph (`invalid` saying why when refused, an older one included);
/// a change is sent on to the hub's other linked peers ([`Flood`]).
pub fn report_call(params: Value, from: &str, published: &Published, stored: &Stored, me: &str, log: fn(&str)) -> Result<Value, ApiError> {
    let s: Signed = parse(params)?;
    if s.report.member != from {
        return Err(ApiError::invalid(format!(
            "`fleet.report` from `{from}` carries `{}`'s report: a member reports its own links",
            s.report.member
        )));
    }
    let mut st = published.lock();
    let State { graph, last, .. } = &mut *st;
    graph.prune(stored);
    let changed = graph.apply(&s.statement, &s.report, stored).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    if s.report.member == me {
        *last = (*last).max(s.statement.version);
    }
    if changed {
        st.push.add(Some(from));
        published.cv.notify_all();
        drop(st);
        published.changed();
        log(&format!("link report from {from} (version {})", s.statement.version));
    }
    to_value(Took { changed })
}

/// `fleet.graph {reports?}` from link peer `from` (admitted): the reports
/// it carries taken (each verifies itself; a refusal is logged), answered
/// with every report this host holds after. A change is sent on to this
/// host's other linked peers ([`Flood`]), on any member.
pub fn graph_call(params: Value, from: &str, published: &Published, stored: &Stored, me: &str, log: fn(&str)) -> Result<Value, ApiError> {
    let theirs: Reports = parse(params)?;
    published.take(stored, me, &theirs.reports, from, log);
    to_value(published.reports(Some(stored)))
}

/// What the thread reads and calls through.
pub struct Io {
    /// `member.toml`: its `trust` and agent grants, and beside it
    /// `membership.json` and `member.key`.
    pub member: PathBuf,
    pub hostname: String,
    pub links: Handle,
    /// This host's hub, as the daemon keeps it (`Shared::hub`).
    pub hub: Arc<dyn Fn() -> Hub + Send + Sync>,
    /// Whether this daemon stays up: a keep-alive daemon or the hub, which
    /// never idles out.
    pub always_on: Arc<dyn Fn() -> bool + Send + Sync>,
    pub log: fn(&str),
}

/// The `serve-fleet-graph` thread, owned by the daemon's `run`; dropping it
/// stops and joins it (before the machine links go).
pub struct Publisher {
    published: Arc<Published>,
    thread: Option<JoinHandle<()>>,
}

impl Publisher {
    pub fn spawn(published: Arc<Published>, io: Io) -> Publisher {
        let p = Arc::clone(&published);
        let thread = std::thread::Builder::new().name("serve-fleet-graph".into()).spawn(move || run(&p, &io));
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                crate::serve::daemon::log(&format!("cannot start the link-report thread: {e}"));
                None
            }
        };
        Publisher { published, thread }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.published.lock().stop = true;
        self.published.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// This host in its fleet, read per round: its name, key and stored log
/// (none: in no fleet's log, nothing to report).
struct Own {
    me: String,
    key: crate::serve::fleet::identity::SigningKey,
    stored: Stored,
}

fn own(io: &Io) -> Option<Own> {
    let stored = membership_api::load(&io.member).ok().flatten()?;
    stored.log.membership()?;
    let (me, key) = membership_api::own(&io.member, &io.hostname).ok()?;
    Some(Own { me, key, stored })
}

/// `name`'s link is up here.
fn up(io: &Io, name: &str) -> bool {
    io.links.remotes().get(name).is_some_and(|m| m.state == LinkState::Connected)
}

fn run(p: &Published, io: &Io) {
    let mut st = p.lock();
    loop {
        if st.stop {
            return;
        }
        let at = Instant::now();
        let due = st.tracker.due().is_some_and(|d| d <= at);
        if !(st.dirty || st.push.due() || due || !st.exchange.is_empty()) {
            let wait = st.tracker.due().map_or(IDLE_WAIT, |d| d.saturating_duration_since(at).min(IDLE_WAIT));
            st = p.cv.wait_timeout(st, wait).unwrap_or_else(|e| e.into_inner()).0;
            continue;
        }
        let hub = (io.hub)();
        if std::mem::take(&mut st.dirty) {
            let came_up = st.tracker.observe(&links_of(&io.links.remotes().all()), at, now_ms());
            if let Hub::Other(h) = &hub
                && came_up.contains(h)
            {
                st.tracker.now(at);
            }
            st.exchange.extend(came_up);
        }
        let report = st.tracker.take_due(Instant::now());
        let exchange = std::mem::take(&mut st.exchange);
        let push = std::mem::take(&mut st.push);
        drop(st);
        if let Some(own) = own(io) {
            if report {
                publish_own(p, io, &own, &hub);
            }
            for peer in &exchange {
                exchange_with(p, io, &own, peer);
            }
            // The changes before this round, flooded; one taken during it
            // (an own report, an exchange's answer) is the next round's.
            for e in io.links.entries() {
                if push.sends_to(&e.name) && up(io, &e.name) {
                    exchange_with(p, io, &own, &e.name);
                }
            }
        }
        st = p.lock();
    }
}

/// This host's report signed, taken into its own graph, and sent to its
/// hub while that link is up; when it changed the graph, flooded to every
/// linked peer too ([`Flood`]).
fn publish_own(p: &Published, io: &Io, own: &Own, hub: &Hub) {
    let Some(genesis) = own.stored.log.membership().map(|m| m.genesis) else { return };
    let file = member::load(&io.member).ok().flatten().unwrap_or_default();
    let agent = grants::granted_names(&file.agent, &own.stored).into_iter().collect();
    let mut st = p.lock();
    let report = st.tracker.report(&own.me, file.trust, (io.always_on)(), agent, |peer| proof::resolve(&own.stored, peer).is_some());
    let (statement, report) = graph::sign_report(&own.key, &genesis, &report, st.last, now_ms());
    st.last = statement.version;
    let signed = Signed { statement, report };
    let State { graph, last, .. } = &mut *st;
    let (changed, refused) = take_into(graph, last, &own.stored, &own.me, std::slice::from_ref(&signed));
    if changed {
        st.push.add(None);
    }
    drop(st);
    if changed {
        p.changed();
    }
    for why in refused {
        (io.log)(&format!("this host's own link report: refused: {why}"));
    }
    let Hub::Other(hub) = hub else { return };
    if !up(io, hub) {
        return;
    }
    let params = match serde_json::to_value(&signed) {
        Ok(v) => v,
        Err(e) => return (io.log)(&format!("cannot encode this host's link report: {e}")),
    };
    if let Err(e) = crate::serve::machines::leave::call_on(&io.links, hub, "fleet.report", params) {
        (io.log)(&format!("link report to the hub {hub}: {e}"));
    }
}

/// Every report held sent to `peer` (`fleet.graph`), and its answer taken.
fn exchange_with(p: &Published, io: &Io, own: &Own, peer: &str) {
    let params = match serde_json::to_value(p.reports(Some(&own.stored))) {
        Ok(v) => v,
        Err(e) => return (io.log)(&format!("cannot encode the link reports: {e}")),
    };
    let answer = crate::serve::machines::leave::call_on(&io.links, peer, "fleet.graph", params)
        .and_then(|v| serde_json::from_value::<Reports>(v).map_err(|e| format!("fleet.graph: unexpected answer: {e}")));
    match answer {
        Ok(theirs) => {
            p.take(&own.stored, &own.me, &theirs.reports, peer, io.log);
        }
        Err(e) => (io.log)(&format!("link reports with {peer}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::fleet::identity::{Signer, SigningKey};
    use crate::serve::fleet::membership_api::test_support;
    use crate::worker_policy::Trust;

    fn machine(name: &str, kind: MachineKind, state: LinkState) -> Machine {
        Machine { name: name.into(), kind, state, ..Machine::default() }
    }

    fn link(peer: &str, dials: bool, state: LinkState) -> (String, bool, LinkState) {
        (peer.into(), dials, state)
    }

    #[test]
    fn links_say_who_dials() {
        let ms = [machine("box", MachineKind::Ssh, LinkState::Connected), machine("pi", MachineKind::Inbound, LinkState::Idle)];
        assert_eq!(links_of(&ms), [link("box", true, LinkState::Connected), link("pi", false, LinkState::Idle)]);
    }

    #[test]
    fn a_change_makes_a_report_due_after_the_debounce() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        assert!(t.observe(&[], t0, 1).is_empty());
        assert_eq!(t.due(), None, "no links, no change");
        assert!(t.observe(&[link("box", true, LinkState::Connecting)], t0, 1).is_empty());
        assert_eq!(t.due(), Some(t0 + DEBOUNCE));
        // Another change joins the pending report.
        let up = t.observe(&[link("box", true, LinkState::Connected)], t0 + Duration::from_millis(500), 2);
        assert_eq!(up, ["box"]);
        assert_eq!(t.due(), Some(t0 + DEBOUNCE));
        assert!(!t.take_due(t0 + Duration::from_millis(1000)), "not yet");
        assert!(t.take_due(t0 + DEBOUNCE));
        assert!(!t.take_due(t0 + DEBOUNCE), "taken");
        // Nothing moved: nothing due.
        assert!(t.observe(&[link("box", true, LinkState::Connected)], t0 + DEBOUNCE, 3).is_empty());
        assert_eq!(t.due(), None);
        // Sent now (the hub link came up), whatever was pending.
        t.observe(&[link("box", true, LinkState::Unreachable)], t0 + DEBOUNCE, 4);
        t.now(t0);
        assert!(t.take_due(t0));
    }

    #[test]
    fn a_report_says_up_or_down_since_when() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.observe(&[link("box", true, LinkState::Connecting), link("pi", false, LinkState::Connected), link("x", true, LinkState::Connected)], t0, 10);
        assert!(t.take_due(t0 + DEBOUNCE));
        // A down link retrying doesn't flap: its first down state stays.
        assert!(t.observe(&[link("box", true, LinkState::Unreachable), link("pi", false, LinkState::Connected), link("x", true, LinkState::Connected)], t0, 20).is_empty());
        assert_eq!(t.due(), None);
        let r = t.report("lap", Trust::Worker, true, vec![], |p| p != "x");
        let want = vec![
            LinkReport { peer: "box".into(), dials: true, state: LinkState::Connecting, since_ms: 10 },
            LinkReport { peer: "pi".into(), dials: false, state: LinkState::Connected, since_ms: 10 },
        ];
        assert_eq!(r, Report { member: "lap".into(), trust: Trust::Worker, always_on: true, links: want, agent: vec![] }, "x isn't in the fleet");
        // Up, then a peer gone: since moves, the gone one goes.
        assert_eq!(t.observe(&[link("box", true, LinkState::Connected)], t0, 30), ["box"]);
        let r = t.report("lap", Trust::Full, false, vec!["pi".into()], |_| true);
        assert_eq!(r.links, [LinkReport { peer: "box".into(), dials: true, state: LinkState::Connected, since_ms: 30 }]);
        assert_eq!((r.trust, r.agent), (Trust::Full, vec!["pi".to_string()]));
    }

    #[test]
    fn a_republish_makes_a_report_due_now_whatever_the_links() {
        let p = Published::default();
        assert_eq!(p.lock().tracker.due(), None, "nothing changed");
        p.republish();
        let at = Instant::now();
        let mut st = p.lock();
        assert!(st.tracker.due().is_some_and(|d| d <= at), "due at once, no debounce");
        assert!(st.tracker.take_due(at));
    }

    /// `lap` (1) the authority, `box` (2) the hub, `pi` (3).
    fn stored() -> Stored {
        let k = |n: u8| SigningKey::from_seed([n; 32]);
        test_support::fleet("lap", &k(1), "box", &[("box", k(2).public_key()), ("pi", k(3).public_key())])
    }

    fn signed(stored: &Stored, member: &str, n: u8, version: u64) -> Signed {
        let g = stored.log.membership().unwrap().genesis;
        let r = Report { member: member.into(), trust: Trust::Worker, always_on: false, links: vec![], agent: vec![] };
        let (statement, report) = graph::sign_report(&SigningKey::from_seed([n; 32]), &g, &r, version - 1, 0);
        Signed { statement, report }
    }

    #[test]
    fn reports_are_taken_one_by_one_and_an_own_one_moves_last() {
        let s = stored();
        let mut g = Graph::default();
        let mut last = 0;
        let forged = Signed { report: Report { member: "box".into(), ..signed(&s, "pi", 3, 5).report }, ..signed(&s, "pi", 3, 5) };
        let (changed, refused) = take_into(&mut g, &mut last, &s, "pi", &[signed(&s, "box", 2, 4), forged, signed(&s, "pi", 3, 9)]);
        assert!(changed);
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(last, 9, "pi's own report, back from a peer");
        assert_eq!(g.reports().count(), 2);
        let (changed, refused) = take_into(&mut g, &mut last, &s, "pi", &[signed(&s, "box", 2, 4)]);
        assert!(!changed && refused.is_empty(), "the one held");
        let (_, refused) = take_into(&mut g, &mut last, &s, "pi", &[signed(&s, "box", 2, 3)]);
        assert!(refused[0].contains("rollback"), "{refused:?}");
    }

    #[test]
    fn a_flood_skips_the_one_peer_its_changes_came_from() {
        let mut f = Flood::default();
        assert!(!f.due() && !f.sends_to("box"));
        f.add(Some("box"));
        f.add(Some("box"));
        assert_eq!(f, Flood::From("box".into()));
        assert!(f.due() && !f.sends_to("box") && f.sends_to("pi"));
        f.add(Some("pi"));
        assert_eq!(f, Flood::All, "changes from two peers: each may lack the other's");
        assert!(f.sends_to("box") && f.sends_to("pi"));
        let mut own = Flood::From("box".into());
        own.add(None);
        assert_eq!(own, Flood::All, "this host's own report goes to every peer");
        f.add(Some("box"));
        assert_eq!(f, Flood::All);
    }

    /// A [`Pair`](crate::serve::fleet::dialer::tests::Pair): L (`lap`, the
    /// authority) dials K (`box`, the hub). L's report reaches K
    /// (`fleet.report`), K's reaches L (K's push, or L's exchange on its link
    /// coming up), each saying who dials.
    #[test]
    fn a_members_report_reaches_the_hub_and_the_hubs_reaches_the_member() {
        use crate::serve::fleet::dialer::tests::{Pair, connect, wait_for};
        use serde_json::json;
        let pair = Pair::build("reports-pair", Vec::new(), |_| {}, |_| {}, |k, l| {
            test_support::fleet_of("box", &[("lap", l), ("box", k)]);
            for (toml, hub) in [(k, "self"), (l, "box")] {
                let mut m = member::load(toml).unwrap().unwrap();
                m.hub = hub.into();
                member::write_for_test(toml, &m);
            }
        });
        let get = |dir: &std::path::Path| connect(dir).call("graph.get", json!({})).unwrap().result.unwrap();
        let link = |g: &Value, member: &str, peer: &str| {
            let r = g["reports"].as_array()?.iter().find(|r| r["report"]["member"] == member)?;
            r["report"]["links"].as_array()?.iter().find(|l| l["peer"] == peer && l["state"] == "connected").cloned()
        };
        wait_for("lap's report on K", Duration::from_secs(20), || link(&get(&pair.k_dir), "lap", "box").is_some());
        wait_for("box's report on L", Duration::from_secs(20), || link(&get(&pair.l_dir), "box", "lap").is_some());
        let on_l = get(&pair.l_dir);
        crate::serve::dts::assert_matches("FleetGraph", &on_l);
        assert_eq!(link(&on_l, "box", "lap").unwrap()["dials"], json!(false), "lap dialed box");
        let on_k = get(&pair.k_dir);
        assert_eq!(link(&on_k, "lap", "box").unwrap()["dials"], json!(true));
        let always_on = |g: &Value, member: &str| g["reports"].as_array().unwrap().iter().find(|r| r["report"]["member"] == member).unwrap()["report"]["always_on"].clone();
        assert_eq!((always_on(&on_l, "box"), always_on(&on_k, "lap")), (json!(true), json!(false)), "the hub never idles out");

        // A trust change on L reaches K's graph with no link change.
        let trust = |g: &Value, member: &str| {
            g["reports"].as_array().and_then(|rs| rs.iter().find(|r| r["report"]["member"] == member)).map(|r| r["report"]["trust"].clone())
        };
        assert_eq!(trust(&on_k, "lap"), Some(json!("worker")));
        let set = connect(&pair.l_dir).call("member.set", json!({"trust": "full"})).unwrap();
        assert_eq!(set.result.unwrap()["trust"], "full");
        wait_for("lap's full trust on K", Duration::from_secs(20), || trust(&get(&pair.k_dir), "lap") == Some(json!("full")));

        // So does an agent grant (decision 66): lap's report lists box.
        let agent = |g: &Value, member: &str| {
            g["reports"].as_array().and_then(|rs| rs.iter().find(|r| r["report"]["member"] == member)).map(|r| r["report"]["agent"].clone())
        };
        assert_eq!(agent(&get(&pair.k_dir), "lap"), Some(json!([])));
        let set = connect(&pair.l_dir).call("machines.set", json!({"name": "box", "agent": true})).unwrap();
        assert!(set.error.is_none(), "{set:?}");
        wait_for("lap's grant of box on K", Duration::from_secs(20), || agent(&get(&pair.k_dir), "lap") == Some(json!(["box"])));
        crate::serve::dts::assert_matches("FleetGraph", &get(&pair.k_dir));
        let set = connect(&pair.l_dir).call("machines.set", json!({"name": "box", "agent": false})).unwrap();
        assert!(set.error.is_none(), "{set:?}");
        wait_for("withdrawn on K", Duration::from_secs(20), || agent(&get(&pair.k_dir), "lap") == Some(json!([])));
        pair.finish();
    }

    #[test]
    fn the_wire_shapes() {
        use crate::serve::dts::{assert_matches, prop_type};
        let s = stored();
        let one = signed(&s, "box", 2, 4);
        let wire = serde_json::to_value(&one).unwrap();
        assert_matches("FleetReport", &wire);
        assert_matches("FleetLinkReport", &wire["report"]);
        assert_eq!(serde_json::from_value::<Signed>(wire).unwrap(), one);
        let mut r = one.report.clone();
        r.links.push(LinkReport { peer: "lap".into(), dials: true, state: LinkState::Connected, since_ms: 1 });
        assert_matches("FleetLink", &serde_json::to_value(&r).unwrap()["links"][0]);
        assert_matches("FleetGraph", &serde_json::to_value(Reports { reports: vec![one] }).unwrap());
        assert_matches("FleetReported", &serde_json::to_value(Took { changed: true }).unwrap());
        assert_eq!(serde_json::from_value::<Reports>(serde_json::json!({})).unwrap(), Reports::default(), "a pull carries none");
        for (method, result) in [("fleet.report", "FleetReported"), ("fleet.graph", "FleetGraph"), ("graph.get", "FleetGraph")] {
            let ty = prop_type("ApiMethods", method).unwrap_or_else(|| panic!("ApiMethods lacks `{method}`"));
            assert!(ty.contains(&format!("result: {result} ")), "{method}: {ty}");
        }
    }
}
