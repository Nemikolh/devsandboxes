//! The live host side the daemon runs (docs/serve.md "Bridges"): one bridge
//! per running instance with a helper (`devsbd::bridge::Bridges`, with the
//! notify sink and the control handler), so outboxes drain, popups fire and
//! control ops answer with the dashboard closed; plus the once-per-boot
//! `autostart = true` pass at start.
//!
//! Threading: a `serve-poll` thread lists the runtime's running containers
//! every [`POLL`], counts the live-instance holders and hands the list to the
//! bridge worker (`Bridges::spawn_worker`, its own thread, which owns every
//! bridge), and bumps [`Host::changes`] when the list moved (the API's
//! `instances.changed`), and hands the list to the forward registry too
//! (`forwards.rs`: configured `forwardPorts` follow it). A [`Waker`] (the API's `bridges.ensure`) cuts the
//! poll's sleep short and marks its container urgent, so a CLI command or a
//! dashboard terminal gets its bridge now rather than within [`POLL`]. A
//! `serve-autostart` thread runs the autostart pass once per config
//! root recorded in `state.toml`. Dropping [`Host`] stops the poll, drops the
//! worker (killing every bridge's `exec`) and joins both threads.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::devsbd::bridge::{self, BridgeBoard, Bridges, OnShown, Readiness};
use crate::state::State;

/// How often the daemon re-lists the running containers. A new instance gets
/// its bridge (and counts as a holder) within this; bridge deaths are retried
/// on their own gaps (`bridge::RETRY`), checked on each poll.
pub const POLL: Duration = Duration::from_secs(5);

/// Called with each successful poll's running container names.
pub type OnRunning = Box<dyn Fn(&[String]) + Send>;

/// What the poll thread sleeps on: stop, or containers a client asked a
/// bridge for.
#[derive(Default)]
struct Signal {
    stop: bool,
    /// `(container, ticket)`: see `bridge::BridgeBoard`.
    urgent: Vec<(String, u64)>,
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
    /// running containers (the forward registry's sync; it must not block).
    pub fn start(log: fn(&str), on_shown: OnShown, on_running: OnRunning) -> Host {
        let stop: Shared = Arc::default();
        let board = BridgeBoard::default();
        let live = Arc::new(AtomicUsize::new(0));
        // Counted before the thread runs, so the first idle check sees it.
        let autostarting = Arc::new(AtomicUsize::new(1));
        let changes = Arc::new(AtomicU64::new(0));
        let mut threads = Vec::new();
        let spawned = {
            let (stop, live, changes, board) = (Arc::clone(&stop), Arc::clone(&live), Arc::clone(&changes), board.clone());
            std::thread::Builder::new()
                .name("serve-poll".into())
                .spawn(move || poll(&stop, &live, &changes, log, on_shown, on_running, board))
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

/// The poll loop: list, count, reconcile, sleep (until [`POLL`] passes or a
/// [`Waker`] fires), until stopped. With the runtime unreachable the last list
/// stands (bridges and the holder count are kept, not torn down on a blip);
/// the outage is logged once, and urgent containers wait for the next list.
fn poll(
    stop: &(Mutex<Signal>, Condvar),
    live: &AtomicUsize,
    changes: &AtomicU64,
    log: fn(&str),
    on_shown: OnShown,
    on_running: OnRunning,
    board: BridgeBoard,
) {
    let worker = Bridges::spawn_worker(Some(Some(on_shown)), board);
    let mut down = false;
    let mut last: Vec<String> = Vec::new();
    let mut urgent: Vec<(String, u64)> = Vec::new();
    loop {
        match running_containers() {
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
        let (lock, cvar) = stop;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut guard, _) = cvar
            .wait_timeout_while(guard, POLL, |s| !s.stop && s.urgent.is_empty())
            .unwrap_or_else(|e| e.into_inner());
        if guard.stop {
            break;
        }
        urgent.append(&mut guard.urgent);
    }
    // `worker` drops here: closes its channel and joins, killing every bridge.
}

/// The runtime's running devsandbox containers. Quiet: the backend captures
/// stderr.
fn running_containers() -> anyhow::Result<Vec<String>> {
    let rows = crate::runtime::backend().list(false, crate::runtime::NAME_PREFIX)?;
    Ok(rows.into_iter().filter(|r| r.state == "running").map(|r| r.name).collect())
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
    }

    #[test]
    fn config_roots_are_distinct_and_skip_unrecorded() {
        let roots = config_roots(&state(STATE));
        assert_eq!(roots, [PathBuf::from("/cfg/a"), PathBuf::from("/cfg/b")]);
        assert!(config_roots(&State::default()).is_empty());
    }
}
