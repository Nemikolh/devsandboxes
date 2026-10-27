//! The live host side the daemon runs (docs/architecture/daemon/host-poll.md
//! "Bridges"): one bridge per running instance with a helper
//! (`devsbd::bridge::Bridges`, with the notify sink and the control handler),
//! so outboxes drain, popups fire and control ops answer with the dashboard
//! closed; plus the once-per-boot `autostart = true` pass at start.
//!
//! Threading: a `serve-poll` thread is the daemon's one container poll. Each
//! tick it lists every devsandbox container once; from the running ones it
//! counts the live-instance holders, hands the list to the bridge worker
//! (`Bridges::spawn_worker`, its own thread, which owns every bridge), bumps
//! [`Host::changes`] when the list moved (the API's `instances.changed`),
//! and hands the list to the forward registry too (`forwards.rs`: configured
//! `forwardPorts` follow it). Then, while a dashboard watches a config root
//! ([`Observers`], `instances.watch`), it asks for stats when due and joins
//! every watched root against that one listing (`snapshots.rs`), each
//! snapshot handed to the daemon to push, with this host's capacity report
//! (`capacity.rs`: the runtime's `info` at most every
//! `capacity::ENGINE_TTL`, free memory and disk, the running instances,
//! `lend.toml`). The poll also owns the runtime's
//! event stream ([`EventWatcher`], `events.rs`): each relevant container
//! event wakes it at once. The sleep is [`tick`]:
//! [`snapshots::LISTING_INTERVAL`] while anything is watched, else
//! [`QUIET_POLL`] while the event stream is live and every bridge is settled,
//! else [`POLL`]. A [`Waker`] (the API's `bridges.ensure`) cuts the poll's sleep
//! short and marks its container urgent, so a CLI command or a dashboard
//! terminal gets its bridge now rather than within [`POLL`]; the bridges are
//! handed their list before any stats or snapshot work.
//! [`Waker::refresh`] (and a container event) cuts it short with no container
//! urgent (a `state.toml` write, a new watch). A `serve-autostart` thread runs the autostart pass
//! once per config root recorded in `state.toml`. Dropping [`Host`] stops
//! the poll, which drops the event watcher (killing its child) and the worker
//! (killing every bridge's `exec`), and joins both threads.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::capacity::HostCapacity;
use super::events::{EVENTS_RETRY, EventWatcher};
use super::fleet::lend::Lend;
use super::snapshots::{self, Join, Snapshots};
use crate::devsbd::bridge::{self, BridgeBoard, Bridges, OnShown, Readiness};
use crate::runtime::{ContainerRow, NAME_PREFIX};
use crate::snapshot::RuntimeView;
use crate::state::State;

/// How often the daemon re-lists the running containers. A new instance gets
/// its bridge (and counts as a holder) within this; bridge deaths are retried
/// on their own gaps (`bridge::RETRY`), checked on each poll.
pub const POLL: Duration = Duration::from_secs(5);

/// The poll's sleep while the runtime's event stream is live (nothing
/// watched, bridges settled): a safety net, since events wake it. It catches
/// what nothing announces: a missed event, a host agent appearing without a
/// `bridges.ensure`. (Config edits wake the poll through `serve-watch`.)
pub const QUIET_POLL: Duration = Duration::from_secs(60);

/// Called with each successful poll's running container names.
pub type OnRunning = Box<dyn Fn(&[String]) + Send>;

/// The watched roots' side of the poll, from the daemon.
pub struct Observers {
    /// The config roots some connection watches now.
    pub watched: Box<dyn Fn() -> BTreeSet<PathBuf> + Send>,
    pub cache: Arc<Snapshots>,
    pub join: Join,
    /// Each watched root's snapshot (`instances.snapshot` params), every
    /// tick; it must not block.
    pub on_snapshot: Box<dyn Fn(&Path, Arc<Value>) + Send>,
    /// `lend.toml` now (`None`: none), for the capacity report's lending
    /// state and soft caps.
    pub lend: Box<dyn Fn() -> Option<Lend> + Send>,
    /// This host's capacity: the poll keeps its running count, and asks it
    /// for the report while a root is watched (the daemon asks it too).
    pub capacity: Arc<HostCapacity>,
}

/// What the poll thread sleeps on: stop, containers a client asked a
/// bridge for, or a snapshot refresh.
#[derive(Default)]
struct Signal {
    stop: bool,
    /// `(container, ticket)`: see `bridge::BridgeBoard`.
    urgent: Vec<(String, u64)>,
    refresh: bool,
}

type Shared = Arc<(Mutex<Signal>, Condvar)>;

/// Wakes the poll thread for one container: it re-lists at once and the
/// bridge worker reconciles with that container urgent
/// (`Bridges::reconcile`). Cloneable; outlives nothing (a wake after the
/// poll stopped is ignored). Carries the bridges' [`BridgeBoard`], so the
/// API can wait for a bridge without touching the worker.
#[derive(Clone)]
pub struct Waker(Shared, BridgeBoard);

impl Waker {
    /// Queue `container` urgent; returns its ticket on the board.
    pub fn wake(&self, container: String) -> u64 {
        let ticket = self.1.ticket();
        let (lock, cvar) = &*self.0;
        lock.lock().unwrap_or_else(|e| e.into_inner()).urgent.push((container, ticket));
        cvar.notify_all();
        ticket
    }

    /// Poll now, no container urgent: a `state.toml` write, a new watch (the
    /// watched roots' snapshots); a container event takes the same path.
    pub fn refresh(&self) {
        refresh(&self.0);
    }
}

/// Cut the poll's sleep short for a full pass (re-list, reconcile, snapshots).
fn refresh(shared: &Shared) {
    let (lock, cvar) = &**shared;
    lock.lock().unwrap_or_else(|e| e.into_inner()).refresh = true;
    cvar.notify_all();
}

/// `bridges.ensure` against the real state and this daemon's bridges.
impl super::api::Bridging for Waker {
    fn container(&self, instance: &str) -> anyhow::Result<Option<String>> {
        Ok(State::load()?.instances.get(instance).map(|i| i.container.clone()))
    }

    fn report_agent(&self, path: PathBuf) {
        bridge::report_agent(path);
    }

    fn reconcile_now(&self, container: String) -> u64 {
        self.wake(container)
    }

    fn wait_ready(&self, container: &str, ticket: u64, timeout: Duration) -> Readiness {
        self.1.wait(container, ticket, timeout)
    }
}

pub struct Host {
    stop: Shared,
    board: BridgeBoard,
    live: Arc<AtomicUsize>,
    autostarting: Arc<AtomicUsize>,
    changes: Arc<AtomicU64>,
    threads: Vec<JoinHandle<()>>,
}

impl Host {
    /// Start the poll (bridges) and autostart threads. `log` writes one
    /// `serve.log` line; `on_shown` gets each stored container message's
    /// status line (the API's `inbox.shown`); `on_running` gets each poll's
    /// running containers (the forward registry's sync; it must not block);
    /// `observers` the watched roots the poll collects snapshots for.
    pub fn start(log: fn(&str), on_shown: OnShown, on_running: OnRunning, observers: Observers) -> Host {
        let stop: Shared = Arc::default();
        let board = BridgeBoard::default();
        // A failed bridge unsettles the board: wake the poll so it drops to
        // `POLL` and acts on the retry. Holds only the signal, not the poll.
        let shared = Arc::clone(&stop);
        board.on_failed(Box::new(move || refresh(&shared)));
        let live = Arc::new(AtomicUsize::new(0));
        // Counted before the thread runs, so the first idle check sees it.
        let autostarting = Arc::new(AtomicUsize::new(1));
        let changes = Arc::new(AtomicU64::new(0));
        let mut threads = Vec::new();
        let spawned = {
            let (stop, live, changes, board) = (Arc::clone(&stop), Arc::clone(&live), Arc::clone(&changes), board.clone());
            std::thread::Builder::new()
                .name("serve-poll".into())
                .spawn(move || poll(&stop, &live, &changes, log, on_shown, on_running, &observers, board))
        };
        match spawned {
            Ok(t) => threads.push(t),
            Err(e) => log(&format!("cannot start the bridge thread: {e}")),
        }
        let spawned = {
            let autostarting = Arc::clone(&autostarting);
            std::thread::Builder::new().name("serve-autostart".into()).spawn(move || {
                autostart_all();
                autostarting.store(0, Ordering::Release);
            })
        };
        match spawned {
            Ok(t) => threads.push(t),
            Err(e) => {
                autostarting.store(0, Ordering::Release);
                log(&format!("cannot start the autostart thread: {e}"));
            }
        }
        Host { stop, board, live, autostarting, changes, threads }
    }

    /// The poll's [`Waker`], for the API.
    pub fn waker(&self) -> Waker {
        Waker(Arc::clone(&self.stop), self.board.clone())
    }

    /// The bridges' board: their links, for `serve::helper`.
    pub fn board(&self) -> BridgeBoard {
        self.board.clone()
    }

    /// Bumped each time a poll finds the running containers changed; the
    /// daemon's notification watcher compares it.
    pub fn changes(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.changes)
    }

    /// Running instances that expect a live host, as of the last poll.
    pub fn live_instances(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// Control requests being served on this daemon's bridges.
    pub fn control(&self) -> usize {
        bridge::control_in_flight()
    }

    /// 1 while the startup autostart pass runs: killing it mid-`run` would
    /// leave a half-made instance, so it holds the daemon.
    pub fn autostarting(&self) -> usize {
        self.autostarting.load(Ordering::Acquire)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.stop;
        lock.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        cvar.notify_all();
        // The poll thread drops the bridge worker, which kills every bridge;
        // autostart is waited out (see `autostarting`).
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// The poll loop: list, count, reconcile, collect the watched roots'
/// snapshots, sleep (until [`tick`] passes, a [`Waker`] fires or a container
/// event arrives), until stopped. The event watcher lives as long as the
/// loop: its down transition wakes the loop too, so a `live` read just before
/// the stream dies never holds the sleep at [`QUIET_POLL`]. With the runtime unreachable the last list stands (bridges and
/// the holder count are kept, not torn down on a blip); the outage is logged
/// once, and urgent containers wait for the next list. Watched roots still
/// get a snapshot then, saying the runtime is down.
#[allow(clippy::too_many_arguments)]
fn poll(
    stop: &Shared,
    live: &AtomicUsize,
    changes: &AtomicU64,
    log: fn(&str),
    on_shown: OnShown,
    on_running: OnRunning,
    observers: &Observers,
    board: BridgeBoard,
) {
    let worker = Bridges::spawn_worker(Some(Some(on_shown)), board.clone());
    let rt = crate::runtime::backend();
    // Declared after `worker`, so it drops (child killed, thread joined) first.
    let events = {
        let argv = rt.events_argv().map(|a| a.into_iter().map(String::from).collect());
        let shared = Arc::clone(stop);
        EventWatcher::start(rt.bin(), argv, Box::new(move || refresh(&shared)), log, EVENTS_RETRY)
    };
    let mut down = false;
    let mut last: Vec<String> = Vec::new();
    let mut urgent: Vec<(String, u64)> = Vec::new();
    loop {
        // Quiet: the backend captures stderr.
        let listing = rt.list(true, NAME_PREFIX);
        match listing.as_ref().map(|rows| running(rows)) {
            Ok(mut running) => {
                running.sort();
                if running != last {
                    changes.fetch_add(1, Ordering::Release);
                    last.clone_from(&running);
                }
                if down {
                    log("the container runtime answers again");
                    down = false;
                }
                // A state that won't load keeps the last count, like the
                // runtime being down.
                if let Ok(state) = State::load() {
                    live.store(live_instances(&running, &state, expects_host), Ordering::Release);
                    // Instances running as of the last listing that loaded with its state.
                    observers.capacity.set_running(running_instances(&running, &state));
                }
                on_running(&running);
                worker.send(running, std::mem::take(&mut urgent));
            }
            Err(e) => {
                if !down {
                    log(&format!("cannot list containers ({e:#}); retrying every {}s", POLL.as_secs()));
                    down = true;
                }
            }
        }
        // After the bridges' hand-off: an urgent bridge never waits on stats.
        let watched = (observers.watched)();
        observers.cache.prune(&watched);
        if !watched.is_empty() {
            let publish = |pushed: Vec<(PathBuf, Arc<Value>)>| {
                for (dir, params) in pushed {
                    (observers.on_snapshot)(&dir, params);
                }
            };
            // The capacity report rides every root's snapshot: only built
            // while someone watches.
            let lend = (observers.lend)();
            observers.cache.set_capacity(&observers.capacity.report(lend.as_ref()));
            let due = listing.is_ok() && observers.cache.stats_due(&watched, Instant::now());
            let mut view = RuntimeView::from_parts(rt, Some(listing), None);
            if due {
                // A new watch's root shows its statuses before the stats
                // pass (~1.5 s on docker): Disk, Listing, then Full.
                publish(observers.cache.collect(&view, &observers.cache.uncached(&watched), &*observers.join));
                view.add_stats(rt.stats());
            }
            publish(observers.cache.collect(&view, &watched, &*observers.join));
        }
        // Read before the lock: a stream lost after this read sets `refresh`,
        // which the wait sees at once.
        let sleep = tick(!watched.is_empty(), events.is_live(), board.settled());
        let (lock, cvar) = &**stop;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut guard, _) = cvar
            .wait_timeout_while(guard, sleep, |s| !s.stop && s.urgent.is_empty() && !s.refresh)
            .unwrap_or_else(|e| e.into_inner());
        if guard.stop {
            break;
        }
        guard.refresh = false;
        urgent.append(&mut guard.urgent);
    }
    // `events` drops here (kills its child, joins), then `worker`: closes its
    // channel and joins, killing every bridge.
}

/// The poll's sleep: the dashboards' cadence while a root is watched; else
/// the long [`QUIET_POLL`] while container events wake the poll and the
/// bridges are settled (`BridgeBoard::settled`), since a pending handshake
/// and a dead bridge's retry (`bridge::retry_decision`, `bridge::RETRY`) are
/// only acted on by a poll pass; else [`POLL`]. A bridge failing during a
/// quiet sleep wakes the poll (`BridgeBoard::on_failed`), so its retry
/// keeps the `POLL` schedule.
fn tick(watched: bool, events_live: bool, bridges_settled: bool) -> Duration {
    if watched {
        snapshots::LISTING_INTERVAL
    } else if events_live && bridges_settled {
        QUIET_POLL
    } else {
        POLL
    }
}

/// The running containers of a listing.
fn running(rows: &[ContainerRow]) -> Vec<String> {
    rows.iter().filter(|r| r.is_running()).map(|r| r.name.clone()).collect()
}

/// Whether instance `key` expects a live host while it runs (see
/// [`holds_host`]). One state + config load for both rules.
fn expects_host(key: &str) -> bool {
    crate::commands::dispatch::current_sandbox(key).is_some_and(|(info, props)| holds_host(&info, &props))
}

/// The holder rule: the instance declares `dispatcher` (children never count
/// as one) or `inbox = true` (its threads' events need a live host).
fn holds_host(info: &crate::state::Instance, props: &crate::config::SandboxProperties) -> bool {
    use crate::commands::dispatch::{is_dispatcher, is_inbox_owner};
    is_dispatcher(info, props) || is_inbox_owner(props)
}

/// The live-instance holder count: instances in `state` whose container is
/// in `running` and that `expects` a live host.
fn live_instances(running: &[String], state: &State, expects: impl Fn(&str) -> bool) -> usize {
    state
        .instances
        .iter()
        .filter(|(key, info)| running.contains(&info.container) && expects(key))
        .count()
}

/// The capacity report's `running`: instances in `state` whose container is
/// in `running` (services and strays aren't instances).
fn running_instances(running: &[String], state: &State) -> usize {
    live_instances(running, state, |_| true)
}

/// Every distinct config root recorded in `state`, sorted. Instances from
/// before roots were recorded have none and are skipped.
pub(super) fn config_roots(state: &State) -> Vec<PathBuf> {
    let roots: BTreeSet<PathBuf> = state.instances.values().filter_map(|i| i.config_dir.clone()).collect();
    roots.into_iter().collect()
}

/// The `autostart = true` pass for every known config root. Idempotent per
/// boot (`State.autostart_boot`), so the clients that still call it too are
/// harmless. Its notes go to stderr/stdout, i.e. `serve.log`.
fn autostart_all() {
    let Ok(state) = State::load() else { return };
    for root in config_roots(&state) {
        crate::commands::autostart::autostart(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(text: &str) -> State {
        toml::from_str(text).unwrap()
    }

    const STATE: &str = r#"
        [instance.disp]
        sandbox = "disp"
        container = "devsandbox-disp"
        folder = "/w/disp"
        workspace = "/workspaces/disp"
        created_unix = 0
        config_dir = "/cfg/a"

        [instance.disp-pr-1]
        sandbox = "web"
        container = "devsandbox-disp-pr-1"
        folder = "/w/web"
        workspace = "/workspaces/web"
        created_unix = 0
        dispatcher = "disp"
        config_dir = "/cfg/a"

        [instance.other]
        sandbox = "disp"
        container = "devsandbox-other"
        folder = "/w/disp"
        workspace = "/workspaces/disp"
        created_unix = 0
        config_dir = "/cfg/b"

        [instance.old]
        sandbox = "web"
        container = "devsandbox-old"
        folder = "/w/web"
        workspace = "/workspaces/web"
        created_unix = 0
    "#;

    #[test]
    fn holders_declare_dispatcher_or_inbox() {
        let s = state(STATE);
        let props = |toml: &str| {
            let config = crate::config::Config::parse(&format!("[sandbox.x]\n{toml}")).unwrap();
            config.resolve_sandbox("x").unwrap().properties
        };
        let (top, child) = (&s.instances["disp"], &s.instances["disp-pr-1"]);
        let dispatcher = props("dispatcher = { spawn = [] }\n");
        let inbox = props("inbox = true\n");
        assert!(holds_host(top, &dispatcher));
        assert!(holds_host(top, &inbox));
        assert!(!holds_host(top, &props("inbox = false\n")));
        assert!(!holds_host(top, &props("")));
        // A child is never a dispatcher, but may own threads.
        assert!(!holds_host(child, &dispatcher));
        assert!(holds_host(child, &inbox));
    }

    #[test]
    fn live_instances_are_running_ones_that_expect_a_host() {
        let s = state(STATE);
        let dispatchers = |k: &str| k == "disp" || k == "other";
        let running = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(live_instances(&running(&[]), &s, dispatchers), 0);
        assert_eq!(live_instances(&running(&["devsandbox-disp", "devsandbox-disp-pr-1"]), &s, dispatchers), 1);
        assert_eq!(live_instances(&running(&["devsandbox-disp", "devsandbox-other"]), &s, dispatchers), 2);
        // Running but not a dispatcher (a child, a plain sandbox): not a holder.
        assert_eq!(live_instances(&running(&["devsandbox-disp-pr-1", "devsandbox-old"]), &s, dispatchers), 0);
        // A running container no state entry knows: not a holder.
        assert_eq!(live_instances(&running(&["devsandbox-stray"]), &s, |_| true), 0);
        // The capacity report counts every running instance.
        let all = running(&["devsandbox-disp", "devsandbox-disp-pr-1", "devsandbox-old", "devsandbox-svc-pg"]);
        assert_eq!(running_instances(&all, &s), 3);
    }

    #[test]
    fn one_listing_feeds_the_running_list_and_the_tick_follows_watches() {
        let row = |name: &str, state: &str| ContainerRow { name: name.into(), state: state.into(), ..Default::default() };
        let rows = [row("devsandbox-a", "running"), row("devsandbox-b", "exited"), row("devsandbox-c", "running")];
        assert_eq!(running(&rows), ["devsandbox-a", "devsandbox-c"]);
        for (live, settled) in [(false, false), (false, true), (true, false), (true, true)] {
            assert_eq!(tick(true, live, settled), snapshots::LISTING_INTERVAL, "watched wins");
        }
        assert_eq!(tick(false, true, true), QUIET_POLL);
        assert_eq!(tick(false, false, true), POLL, "no event stream");
        assert_eq!(tick(false, true, false), POLL, "a bridge pending or failed");
        assert_eq!(tick(false, false, false), POLL);
    }

    #[test]
    fn config_roots_are_distinct_and_skip_unrecorded() {
        let roots = config_roots(&state(STATE));
        assert_eq!(roots, [PathBuf::from("/cfg/a"), PathBuf::from("/cfg/b")]);
        assert!(config_roots(&State::default()).is_empty());
    }
}
