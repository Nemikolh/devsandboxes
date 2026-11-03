//! The daemon's relayed agents from members reached by name
//! (docs/architecture/daemon/fleet.md, *AGENT*; fleet plan step 10c.4,
//! decision 66): an `agent-route-<member>.sock` (`endpoint::route_agent_socket`)
//! for each member whose link report lists this host among its agent
//! grantees ([`wanted`]), probed and bound by `fleet::agent::serve` over
//! `fleet::route::connect` (`agent::ToMember`: the member's
//! `fleet-<member>.sock` when a link to it binds one here, else its
//! `route-<member>.sock`, a routed stream). One `serve-agent-route` thread
//! per member, started and stopped as the graph and the links move by the
//! `serve-agent-routes` thread ([`AgentRoutes`]).
//!
//! A member that dials this host has its `fleet-link --peer` here probe it
//! and bind `agent-link-<member>.sock` (`fleet::agent`): no loop for it.
//! The granting member checks its grant on every stream regardless; a
//! report only says whom to ask.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use super::agent::{self, ToMember};
use super::graph::Graph;
use super::membership::Stored;
use super::proof;
use crate::serve::endpoint;

/// The members to probe for agent (pure): every one whose report in
/// `graph` lists `me` among its grantees and that still resolves to the key
/// that signed it in `stored`, but `me` and those `dials_me` says are an
/// inbound neighbor here (their `fleet-link --peer` binds
/// `agent-link-<member>.sock`).
pub fn wanted(graph: &Graph, stored: &Stored, me: &str, dials_me: impl Fn(&str) -> bool) -> BTreeSet<String> {
    graph
        .reports()
        .filter(|(s, r)| {
            r.member != me
                && r.agent.iter().any(|n| n == me)
                && proof::resolve(stored, &r.member).is_some_and(|(k, _)| k == s.key)
                && !dials_me(&r.member)
        })
        .map(|(_, r)| r.member.clone())
        .collect()
}

/// What the `serve-agent-routes` thread works with.
pub struct Io {
    /// The socket dir: where the agent sockets are bound and the members'
    /// link and route sockets are.
    pub dir: PathBuf,
    /// The members to probe now ([`wanted`] over the daemon's graph and
    /// inbound neighbors), read at each change.
    pub wanted: Box<dyn Fn() -> BTreeSet<String> + Send>,
    /// The probe period (`agent::PROBE` outside tests).
    pub every: Duration,
    pub log: fn(&str),
}

#[derive(Default)]
struct State {
    dirty: bool,
    stop: bool,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    cv: Condvar,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Tells the `serve-agent-routes` thread to look at the graph and links
/// again.
#[derive(Clone)]
pub struct Poke(Arc<Inner>);

impl Poke {
    /// Doesn't block.
    pub fn poke(&self) {
        self.0.lock().dirty = true;
        self.0.cv.notify_all();
    }
}

/// The `serve-agent-routes` thread, owned by the daemon's `run`; dropping
/// it stops and joins it and every member's loop, their sockets unlinked.
pub struct AgentRoutes {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl AgentRoutes {
    /// Every `agent-route-*.sock` left in `io.dir` removed (the caller
    /// holds the daemon's start lock: they're a dead daemon's), then the
    /// thread.
    pub fn spawn(io: Io) -> AgentRoutes {
        sweep(&io.dir);
        let inner = Arc::new(Inner { state: Mutex::new(State { dirty: true, stop: false }), cv: Condvar::new() });
        let i = Arc::clone(&inner);
        let thread = std::thread::Builder::new().name("serve-agent-routes".into()).spawn(move || run(&i, &io));
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                crate::serve::daemon::log(&format!("cannot start the agent route sockets' thread: {e}"));
                None
            }
        };
        AgentRoutes { inner, thread }
    }

    pub fn poke(&self) -> Poke {
        Poke(Arc::clone(&self.inner))
    }
}

impl Drop for AgentRoutes {
    fn drop(&mut self) {
        self.inner.lock().stop = true;
        self.inner.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The agent route sockets' files in `dir` removed.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(endpoint::ROUTE_AGENT_PREFIX) && name.ends_with(".sock") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// One member's probe-and-bind loop: `over` ends it.
struct Loop {
    over: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

fn run(inner: &Inner, io: &Io) {
    let mut loops: BTreeMap<String, Loop> = BTreeMap::new();
    loop {
        {
            let mut st = inner.lock();
            while !st.stop && !st.dirty {
                // A loop that ended on its own (its socket failed) is
                // restarted at the next look, a change or this timeout.
                st = inner.cv.wait_timeout(st, Duration::from_secs(5)).unwrap_or_else(|e| e.into_inner()).0;
                if loops.values().any(|l| l.thread.is_finished()) {
                    break;
                }
            }
            if st.stop {
                break;
            }
            st.dirty = false;
        }
        reconcile(&mut loops, (io.wanted)(), io);
    }
    for l in loops.values() {
        l.over.store(true, Ordering::Relaxed);
    }
    for (_, l) in loops {
        let _ = l.thread.join();
    }
}

/// `loops` made `wanted`'s: the others told to stop (each unbinds its
/// socket as it returns), the missing and ended ones started.
fn reconcile(loops: &mut BTreeMap<String, Loop>, wanted: BTreeSet<String>, io: &Io) {
    loops.retain(|name, l| {
        let keep = wanted.contains(name) && !l.thread.is_finished();
        if !keep {
            l.over.store(true, Ordering::Relaxed);
        }
        keep
    });
    for name in wanted {
        if loops.contains_key(&name) {
            continue;
        }
        let over = Arc::new(AtomicBool::new(false));
        let path = endpoint::route_agent_socket(&io.dir, &name);
        let opener = ToMember { dir: io.dir.clone(), member: name.clone() };
        let (flag, every, log) = (Arc::clone(&over), io.every, io.log);
        let spawned = std::thread::Builder::new().name("serve-agent-route".into()).spawn(move || {
            if let Err(e) = agent::serve(&path, &opener, &flag, every) {
                log(&format!("fleet: the agent route socket for {}: {e:#}", opener.member));
            }
        });
        match spawned {
            Ok(thread) => {
                loops.insert(name, Loop { over, thread });
            }
            Err(e) => (io.log)(&format!("fleet: cannot start the agent route loop for {name}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::fleet::graph::{Report, sign_report};
    use crate::serve::fleet::identity::{Signer, SigningKey};
    use crate::serve::fleet::membership::{Log, Op};
    use crate::worker_policy::Trust;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    /// `lap` (1, the authority), `k` (2), `b` (3), `m` (4).
    fn stored() -> Stored {
        let mut log = Log::default();
        log.append(Op::genesis_with_id([1; 16], "lap", key(1).public_key(), "k"), "lap", &key(1)).unwrap();
        for (name, k) in [("k", 2), ("b", 3), ("m", 4)] {
            log.append(Op::Add { name: name.into(), key: key(k).public_key() }, "lap", &key(1)).unwrap();
        }
        Stored { log, introductions: vec![] }
    }

    fn report(s: &Stored, graph: &mut Graph, member: &str, k: u8, agent: &[&str]) {
        let agent = agent.iter().map(|a| a.to_string()).collect();
        let r = Report { member: member.into(), trust: Trust::default(), always_on: false, links: vec![], agent };
        let genesis = s.log.membership().unwrap().genesis;
        let (st, r) = sign_report(&key(k), &genesis, &r, 0, 10);
        graph.apply(&st, &r, s).unwrap();
    }

    fn names(w: BTreeSet<String>) -> Vec<String> {
        w.into_iter().collect()
    }

    #[test]
    fn members_granting_this_host_are_probed_but_those_that_dial_it() {
        let s = stored();
        let mut g = Graph::default();
        report(&s, &mut g, "lap", 1, &["b", "k"]);
        report(&s, &mut g, "k", 2, &["lap"]);
        report(&s, &mut g, "m", 4, &["k"]);
        assert_eq!(names(wanted(&g, &s, "b", |_| false)), ["lap"]);
        assert_eq!(names(wanted(&g, &s, "k", |_| false)), ["lap", "m"]);
        // `lap` dials `k`: its `fleet-link --peer` there binds the agent-link.
        assert_eq!(names(wanted(&g, &s, "k", |n| n == "lap")), ["m"]);
        assert_eq!(names(wanted(&g, &s, "lap", |_| false)), ["k"]);
        // No one grants `m`; no reports, nothing.
        assert!(wanted(&g, &s, "m", |_| false).is_empty());
        assert!(wanted(&Graph::default(), &s, "b", |_| false).is_empty());
    }

    #[test]
    fn a_member_revoked_since_its_report_is_not_probed() {
        let mut s = stored();
        let mut g = Graph::default();
        report(&s, &mut g, "m", 4, &["b"]);
        assert_eq!(names(wanted(&g, &s, "b", |_| false)), ["m"]);
        s.log.append(Op::Revoke { name: "m".into(), key: key(4).public_key() }, "lap", &key(1)).unwrap();
        assert!(wanted(&g, &s, "b", |_| false).is_empty());
    }

    /// The thread starts a loop per wanted member (nothing answers at
    /// these names: never bound) and stops them all when dropped; a dead
    /// daemon's leftover is swept.
    #[test]
    fn the_thread_follows_what_is_wanted_and_sweeps_leftovers() {
        let dir = std::env::temp_dir().join(format!("dsv-agent-routes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(endpoint::route_agent_socket(&dir, "old"), "").unwrap();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let routes = AgentRoutes::spawn(Io {
            dir: dir.clone(),
            wanted: Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                ["b".to_string()].into()
            }),
            every: Duration::from_millis(20),
            log: |_| {},
        });
        assert!(!endpoint::route_agent_socket(&dir, "old").exists());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while asked.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "never reconciled");
            std::thread::sleep(Duration::from_millis(10));
        }
        routes.poke().poke();
        while asked.load(Ordering::SeqCst) < 2 {
            assert!(std::time::Instant::now() < deadline, "a poke reconciles");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(routes);
        assert!(!endpoint::route_agent_socket(&dir, "b").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
