//! The held link per machine (docs/architecture/daemon/machines.md, *Held
//! links: `Machines`*): one `serve-machine-<name>` thread per registered
//! machine and per inbound neighbor up now (`inbound::Neighbors`, whose
//! hook reloads the service as one comes or goes), owned
//! by the [`Machines`] service the daemon runs for its whole life.
//!
//! Every link is held for as long as the service runs (fleet plan decision
//! 7), an inbound neighbor's while it's up. What carries it depends on the
//! machine's kind: an ssh machine's carrier is its peer link, held by a
//! `serve-machine-<name>-link` thread running the [`Hold`]
//! (`fleet::dialer`: `fleet-link --peer <this host>` there through the
//! transport, this end binding `fleet-<name>.sock` here), redialed after
//! the backoff below whenever it ends; an inbound neighbor's is the
//! `fleet-link --peer <it>` that announced it. Once the carrier
//! is up the link thread dials `fleet-<name>.sock` (the [`Dial`]) and says
//! hello, subscribes to `instances`, asks its `roots.list` (its live
//! roots, the machine's only ones: again on [`Handle::relist`]), watches
//! each of them while a local connection watches with `machines: true`
//! ([`Hooks::watching`]), and stores every pushed `instances.snapshot` in
//! the shared [`Remotes`] ([`Hooks::changed`] then re-publishes). The link
//! to this host's hub ([`Hooks::hub`]: another member) also watches the
//! hub's fleet root for this host (its `roots.list`'s `fleet` row) with
//! `machines: true`, the federated view, whose other machines are kept as
//! the hub's `snapshots::Machine::fleet` (`snapshots::fleet_view`). A carrier
//! that's down, a failed connect or a lost link is `unreachable` with the
//! reason, rows kept; a failed connect or lost link over a live carrier is
//! retried with a doubling backoff, a carrier coming back is dialed at
//! once. A failure is logged when it differs from the last one logged since
//! the link (or carrier) was last up, not per retry.
//!
//! A hub also saves `Remotes` to `remotes.json` (`machines::saved`) from a
//! `serve-remotes` thread, at most every `saved::SAVE_EVERY` while rows
//! change and once more at the stop, and restores it at the start: those
//! rows (and their machine's roots as last listed) show `unreachable`
//! until their machine's link connects, and stay until it lists. Whether
//! this host is the hub is asked each time ([`Hub::is_hub`]: `member.toml`'s
//! `hub` can change under a running daemon), so a host that stops being
//! the hub stops saving, one that becomes it saves from the next change.
//!
//! [`Handle::cycle`] ends every ssh machine's carrier and redials it at
//! once, with no backoff (this host's hub changed: its hello did).
//!
//! Forwarded calls reach the thread over its channel, one in flight
//! per machine, each with [`Timing::call`] to answer. A timed-out call drops
//! the link (its answer would come out of turn for the next call), not its
//! carrier. A call made while the carrier is down fails at once with why
//! (or waits for the carrier's first attempt to end).
//!
//! Routed machines ([`Handle::routed`], fleet plan decision 60: members of
//! this host's link graph with no link up here, or ones its hub's view
//! lists) have no thread: nothing holds or watches them (their rows are the
//! hub's view's). A call forwarded to one opens an `API` stream of its own
//! over the [`RouteDial`] (`fleet::route::connect`), says hello, makes the
//! call within [`Timing::call`] and closes it, on the caller's thread.
//!
//! Agent isn't the link's business: this host answers the machine's
//! `AGENT` streams per stream by its grants in `member.toml`
//! (`daemon::agent_for`), not the registry.
//!
//! Nothing here blocks a connection or the daemon's locks on the network:
//! the threads do all the I/O, a caller only waits for its reply. A stop
//! (`machines.rm`, daemon exit) shuts the link's socket and the carrier's
//! down, so even a handshake in progress ends at once, and joins both
//! threads.

use std::collections::{BTreeMap, VecDeque};
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::inbound::Neighbors;
use super::link::{self, Link};
use super::{Entry, Kind};
use super::saved;
use super::ssh::{Dial, Duplex, RouteDial};
use crate::serve::api::ApiError;
use crate::serve::endpoint::Stream;
use crate::serve::proto::Response;
use crate::serve::snapshots::{self, LinkState, Remotes};

/// The link's clocks; [`Timing::default`] in production, short in tests.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The first retry's delay; each failure doubles it, up to `backoff_cap`.
    pub backoff: Duration,
    pub backoff_cap: Duration,
    /// A forwarded call's wait for its answer.
    pub call: Duration,
    /// The hello's wait: for a carrier, ssh's connect and auth, the remote
    /// login shell and its `Hello`; for the link, a lazy start of the
    /// remote daemon.
    pub handshake: Duration,
    /// How often a link thread looks at its queue and its carrier.
    pub tick: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
            call: Duration::from_secs(15),
            handshake: Duration::from_secs(20),
            tick: Duration::from_millis(100),
        }
    }
}

/// What the service needs from the daemon. All are called on link threads
/// and must not block.
pub struct Hooks {
    /// Whether any local connection watches a root with `machines: true`.
    pub watching: Box<dyn Fn() -> bool + Send + Sync>,
    /// A machine's rows or state changed: re-publish the federated
    /// snapshots.
    pub changed: Box<dyn Fn() + Send + Sync>,
    /// This host's hub, when another member is (`member.toml`'s `hub`, as
    /// the daemon keeps it): asked at each look at the watchers.
    pub hub: Box<dyn Fn() -> Option<HubOf> + Send + Sync>,
    /// The members this host's link graph routes to (no link up here),
    /// each with the first hop of its first usable path or why there is
    /// none. Called on API threads (it may read `membership.json`), never
    /// on a link thread.
    pub routed: Box<dyn Fn() -> Vec<(String, Result<String, String>)> + Send + Sync>,
}

/// This host as a member of a fleet whose hub is another member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubOf {
    /// The hub's member name: the machine whose link watches with
    /// `machines: true`.
    pub hub: String,
    /// This host's own member name: never taken from the hub's view.
    pub me: String,
}

/// How an ssh machine's link is carried: hold `entry`'s peer link until it
/// ends, telling `held` how it stands; returns why it ended. The daemon's
/// is `fleet::dialer::Dialer::hold`; tests fake it.
pub type Hold = Arc<dyn Fn(&Entry, &Held) -> String + Send + Sync>;

/// Where a fleet hub's `Remotes` lives on, and whether this host is the
/// hub now (`member.toml`'s `hub = "self"`, as the daemon keeps it).
pub struct Hub {
    /// `remotes.json` (`saved::path`; a scratch file in tests).
    pub remotes: PathBuf,
    pub is_hub: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// The service, owned by the daemon's `run`. Dropping it stops and joins
/// every link (closing its connection, killing its carrier's ssh), then a
/// hub's last save.
pub struct Machines {
    inner: Arc<Inner>,
    saver: Option<JoinHandle<()>>,
}

/// A hub's save of `Remotes`: the file and the `serve-remotes` thread's
/// wake-ups.
struct HubSave {
    path: PathBuf,
    is_hub: Arc<dyn Fn() -> bool + Send + Sync>,
    state: Mutex<SaveState>,
    cv: Condvar,
}

#[derive(Default)]
struct SaveState {
    /// The rows changed since the last save.
    dirty: bool,
    /// The links are stopped: save what's dirty, then end.
    stop: bool,
}

/// A cheap clone for the API: forwarded calls, registry reloads, link
/// states. After the service stopped, calls fail and reloads do nothing.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<Inner>,
}

struct Inner {
    /// `machines.toml`.
    registry: PathBuf,
    /// The inbound neighbors: a machine each, beside the registry's.
    inbound: Arc<Neighbors>,
    /// One [`Handle::reload`] at a time: two reading the sources apart
    /// could leave a gone neighbor's link behind.
    reloading: Mutex<()>,
    dial: Dial,
    /// Opens a routed machine's `API` stream.
    route: RouteDial,
    /// Carries an ssh machine's link.
    hold: Hold,
    timing: Timing,
    hooks: Hooks,
    log: fn(&str),
    remotes: Arc<Remotes>,
    links: Mutex<BTreeMap<String, LinkThread>>,
    stopped: AtomicBool,
    /// `Some` on a daemon that may be a fleet hub ([`HubSave::is_hub`]).
    hub: Option<HubSave>,
}

/// One machine's threads, as the service holds them.
struct LinkThread {
    entry: Entry,
    tx: Sender<Cmd>,
    ctl: Arc<Ctl>,
    thread: Option<JoinHandle<()>>,
    /// An ssh machine's carrier thread.
    carrier: Option<JoinHandle<()>>,
}

/// The machine's link has a carrier of its own (an ssh machine): an
/// inbound neighbor's is a `fleet-link` here.
fn carried(entry: &Entry) -> bool {
    matches!(entry.kind, Kind::Ssh { .. })
}

/// What a machine's link and carrier threads share with the service: how
/// it stops them wherever they are blocked, and how the carrier stands.
#[derive(Default)]
struct Ctl {
    stop: AtomicBool,
    /// [`Handle::cycle`] cut the carrier: redial with no backoff.
    cycled: AtomicBool,
    /// The link's connection while one is open, to shut it down.
    stream: Mutex<Option<Stream>>,
    carrier: Mutex<Carrier>,
    /// Wakes the carrier thread's backoff at a stop.
    carrier_cv: Condvar,
}

/// An ssh machine's carrier as its [`Hold`] last reported it.
#[derive(Default)]
struct Carrier {
    /// Its `Hello`s are through: `fleet-<name>.sock` is bound.
    up: bool,
    /// Counts the times it came up, so the link sees a new carrier.
    generation: u64,
    /// Why it's down; `None` before its first attempt ended.
    why: Option<String>,
    /// Its transport's socket while one is open, to shut it down.
    stream: Option<Stream>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Ctl {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn cut(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(s) = lock(&self.stream).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(s) = lock(&self.carrier).stream.as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
        self.carrier_cv.notify_all();
    }

    fn hold(&self, stream: Option<Stream>) {
        *lock(&self.stream) = stream;
    }

    /// End the carrier (not the machine): [`carry`] redials at once.
    fn cycle(&self) {
        self.cycled.store(true, Ordering::SeqCst);
        if let Some(s) = lock(&self.carrier).stream.as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
        self.carrier_cv.notify_all();
    }

    /// Sleep up to `wait` unless stopped or cycled first.
    fn sleep(&self, wait: Duration) {
        let c = lock(&self.carrier);
        let _ = self.carrier_cv.wait_timeout_while(c, wait, |_| !self.stopped() && !self.cycled.load(Ordering::SeqCst));
    }
}

/// What a [`Hold`] reports to its machine's link thread while it runs.
pub struct Held<'a> {
    ctl: &'a Ctl,
    tx: &'a Sender<Cmd>,
    came_up: AtomicBool,
}

impl Held<'_> {
    /// The service stops this machine: end the hold.
    pub fn stopped(&self) -> bool {
        self.ctl.stopped()
    }

    /// The transport's socket, shut at a stop; `false` (nothing kept) when
    /// the stop already came.
    pub fn hold(&self, stream: Option<Stream>) -> bool {
        let mut c = lock(&self.ctl.carrier);
        if self.ctl.stopped() {
            return false;
        }
        c.stream = stream;
        true
    }

    /// The carrier is up (its sockets bound): the link dials it now.
    pub fn up(&self) {
        self.came_up.store(true, Ordering::SeqCst);
        self.set(true, None);
    }

    /// The carrier is connected but serves nothing yet, with why (it
    /// waits for the hub's build): the machine shows `unreachable`, why.
    pub fn note(&self, why: String) {
        self.set(false, Some(why));
    }

    fn set(&self, up: bool, why: Option<String>) {
        {
            let mut c = lock(&self.ctl.carrier);
            if up && !c.up {
                c.generation += 1;
            }
            c.up = up;
            if why.is_some() || up {
                c.why = why;
            }
        }
        let _ = self.tx.send(Cmd::Wake);
    }
}

enum Cmd {
    Call(Call),
    /// Look at the carrier and the watchers again now (a watch came or
    /// went, the carrier moved, a stop).
    Wake,
    /// Ask the machine's `roots.list` again ([`Handle::relist`]).
    Relist,
}

/// A forwarded call: the remote's response (its `id` is the remote's; the
/// daemon answers with its own) or a local error as a response.
struct Call {
    method: String,
    params: Value,
    /// The wait for its answer ([`Timing::call`] unless the caller said).
    wait: Duration,
    reply: Sender<Response>,
}

impl Machines {
    /// Start the service: one link per entry of `registry` and per neighbor
    /// of `inbound` (from now on as they come and go), each held at once.
    /// `dial` opens a machine's link (its `fleet-<name>.sock`, or a test
    /// daemon's socket), `route` a routed machine's stream, `hold` carries
    /// an ssh machine's link. A `hub`'s saved
    /// rows are restored before the links start when this host is the hub
    /// then.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        registry: PathBuf,
        inbound: Arc<Neighbors>,
        dial: Dial,
        route: RouteDial,
        hold: Hold,
        timing: Timing,
        hooks: Hooks,
        hub: Option<Hub>,
        log: fn(&str),
    ) -> Machines {
        let hub = hub.map(|h| HubSave { path: h.remotes, is_hub: h.is_hub, state: Mutex::default(), cv: Condvar::new() });
        let inner = Arc::new(Inner {
            registry,
            inbound,
            reloading: Mutex::new(()),
            dial,
            route,
            hold,
            timing,
            hooks,
            log,
            remotes: Arc::default(),
            links: Mutex::new(BTreeMap::new()),
            stopped: AtomicBool::new(false),
            hub,
        });
        let saver = inner.hub.as_ref().and_then(|h| {
            if (h.is_hub)() {
                match saved::load(&h.path) {
                    Ok(s) => inner.remotes.restore(saved::machines(s)),
                    Err(e) => log(&format!("machines: {e:#}")),
                }
            }
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("serve-remotes".into())
                .spawn(move || save_loop(&inner))
                .map_err(|e| log(&format!("cannot start saving remotes.json: {e}")))
                .ok()
        });
        // Weak: the neighbors outlive the service (the API's `Ctx` holds
        // them), and a stopped service has nothing to reload.
        let weak = Arc::downgrade(&inner);
        inner.inbound.on_change(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                Handle { inner }.reload();
            }
        }));
        let machines = Machines { inner, saver };
        machines.handle().reload();
        machines
    }

    pub fn handle(&self) -> Handle {
        Handle { inner: Arc::clone(&self.inner) }
    }
}

impl Drop for Machines {
    fn drop(&mut self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        let links = std::mem::take(&mut *self.inner.lock_links());
        stop_all(links.into_values().collect());
        if let Some(hub) = &self.inner.hub {
            lock(&hub.state).stop = true;
            hub.cv.notify_all();
        }
        if let Some(saver) = self.saver.take() {
            let _ = saver.join();
        }
    }
}

/// Unix seconds now.
fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A hub's `serve-remotes` thread: each change of the rows saves them,
/// at most every `saved::SAVE_EVERY` ([`saved::save_wait`]); the stop saves
/// what's left at once. A failed save is logged; the next change retries.
/// Changes while this host isn't the hub are dropped.
fn save_loop(inner: &Inner) {
    let Some(hub) = &inner.hub else { return };
    let mut last: Option<Instant> = None;
    loop {
        {
            let mut s = lock(&hub.state);
            while !s.dirty && !s.stop {
                s = hub.cv.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            if !s.dirty {
                return;
            }
            let wait = saved::save_wait(last, Instant::now());
            if !wait.is_zero() && !s.stop {
                drop(hub.cv.wait_timeout(s, wait));
                continue;
            }
            s.dirty = false;
        }
        if !(hub.is_hub)() {
            continue;
        }
        if let Err(e) = saved::save(&hub.path, &saved::of(&inner.remotes.all(), now_unix())) {
            (inner.log)(&format!("machines: cannot save remotes: {e:#}"));
        }
        last = Some(Instant::now());
    }
}

/// Cut every link first, then join: they wind down in parallel.
fn stop_all(links: Vec<LinkThread>) {
    for l in &links {
        l.ctl.cut();
        let _ = l.tx.send(Cmd::Wake);
    }
    for mut l in links {
        for t in [l.thread.take(), l.carrier.take()].into_iter().flatten() {
            let _ = t.join();
        }
    }
}

impl Inner {
    fn lock_links(&self) -> MutexGuard<'_, BTreeMap<String, LinkThread>> {
        lock(&self.links)
    }

    /// The rows changed: a hub saves them soon.
    fn rows_changed(&self) {
        if let Some(hub) = &self.hub {
            lock(&hub.state).dirty = true;
            hub.cv.notify_all();
        }
    }
}

/// The registry's `entries` and an inbound entry per neighbor up now
/// (`Entry::inbound`), sorted by name; a registry name wins (the clash
/// rules refuse one, this covers a race between them).
fn with_inbound(mut entries: Vec<Entry>, inbound: &Neighbors) -> Vec<Entry> {
    for n in inbound.list() {
        if !entries.iter().any(|e| e.name == n.name) {
            entries.push(Entry::inbound(n.name));
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

impl Handle {
    /// Re-read the registry and the inbound neighbors: start links for new
    /// machines, stop (and drop the rows of) removed ones, restart changed
    /// ones. A machine's roots and rows stay as its link (or a hub's
    /// restore) left them: its roots are its listing's, not the registry's.
    /// `machines.add` and `.rm` call it, as does the neighbors' hook; a
    /// registry that doesn't load is logged and changes nothing.
    pub fn reload(&self) {
        let inner = &self.inner;
        let _one = lock(&inner.reloading);
        let entries = match super::load(&inner.registry) {
            Ok(entries) => with_inbound(entries, &inner.inbound),
            Err(e) => {
                (inner.log)(&format!("machines: {e:#}"));
                return;
            }
        };
        let gone: Vec<LinkThread> = {
            let mut links = inner.lock_links();
            let stale: Vec<String> =
                links.iter().filter(|(name, l)| !entries.iter().any(|e| e.name == **name && *e == l.entry)).map(|(name, _)| name.clone()).collect();
            stale.iter().filter_map(|name| links.remove(name)).collect()
        };
        // Joined outside the lock (a forwarded call looks links up), and
        // before a changed entry's new thread starts writing its state.
        stop_all(gone);
        let mut links = inner.lock_links();
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        // Before the new threads run: they record their state there.
        inner.remotes.sync(&entries);
        inner.rows_changed();
        for e in &entries {
            if !links.contains_key(&e.name) {
                let link = spawn_link(e.clone(), Arc::clone(inner));
                links.insert(e.name.clone(), link);
            }
        }
        drop(links);
        (inner.hooks.changed)();
    }

    /// End every ssh machine's carrier and redial it at once (this host's
    /// hub changed, so its hello did; the far end's `fleet-link` exits with
    /// it). An inbound machine's carrier is its `fleet-link` here, which the
    /// daemon ends itself (`links.cycle`).
    pub fn cycle(&self) {
        for l in self.inner.lock_links().values() {
            if carried(&l.entry) {
                l.ctl.cycle();
            }
        }
    }

    /// Every link looks at its watchers now (a `machines: true` watch came).
    pub fn wake(&self) {
        for l in self.inner.lock_links().values() {
            let _ = l.tx.send(Cmd::Wake);
        }
    }

    /// The machines as of the last reload (the registry's and the inbound
    /// neighbors'), sorted by name, each with its live roots (`Remotes`':
    /// what its link's last `roots.list` answered, before that what a hub
    /// restored, else none): what a forwarded verb dials and a federated
    /// call fans out to, without a registry read per request.
    pub fn entries(&self) -> Vec<Entry> {
        let links = self.inner.lock_links();
        links.values().map(|l| Entry { roots: self.inner.remotes.roots(&l.entry.name), ..l.entry.clone() }).collect()
    }

    /// Machine `name`'s link asks its `roots.list` again (now if it's up,
    /// else at its next link-up, which always does): a config copy may have
    /// been installed there. Called once a `fleet.sync` went to it or came
    /// from it (`changes::push`, `changes::sync`): it installs a moved
    /// config before answering one, and sends the log on only after
    /// installing. A name with no link here does nothing.
    pub fn relist(&self, name: &str) {
        if let Some(l) = self.inner.lock_links().get(name) {
            let _ = l.tx.send(Cmd::Relist);
        }
    }

    /// The machines' states and rows.
    pub fn remotes(&self) -> &Arc<Remotes> {
        &self.inner.remotes
    }

    /// The error for `machine`, neither a machine here nor routed:
    /// `not-found`.
    pub fn unknown(&self, machine: &str) -> ApiError {
        ApiError::not_found(format!("no machine named `{machine}`"))
    }

    /// The routed machines now (`snapshots::routed`): the graph's
    /// ([`Hooks::routed`]) and the hub view's, none with a link here.
    pub fn routed(&self) -> Vec<snapshots::Routed> {
        self.inner.remotes.routed((self.inner.hooks.routed)())
    }

    /// Routed machine `name`, if it is one.
    pub fn routed_one(&self, name: &str) -> Option<snapshots::Routed> {
        if self.inner.lock_links().contains_key(name) {
            return None;
        }
        self.routed().into_iter().find(|r| r.name == name)
    }

    /// A fresh `API` stream to `machine`'s daemon: over its link for a
    /// machine here, over the [`RouteDial`] for a routed one; `None` for a
    /// name that's neither. What a forwarded verb runs on.
    pub fn dial(&self, machine: &str) -> Option<anyhow::Result<Duplex>> {
        let entry = self.inner.lock_links().get(machine).map(|l| l.entry.clone());
        match entry {
            Some(entry) => Some((self.inner.dial)(&entry)),
            None => self.routed_one(machine).map(|_| (self.inner.route)(machine)),
        }
    }

    /// Call `method` on machine `machine`'s daemon over its link (a routed
    /// machine's: a stream of its own, [`Self::call_routed`]): the remote's
    /// response as is (its error code kept), or `not-found` for a name
    /// that's no machine here nor routed.
    pub fn forward(&self, machine: &str, method: &str, params: Value) -> Result<Response, ApiError> {
        self.forward_within(machine, method, params, self.inner.timing.call)
    }

    /// [`Self::forward`], the answer waited for up to `wait` instead of
    /// [`Timing::call`] (a call the far end answers only after slow work:
    /// `fleet.sync`, which installs the configs it moved first). A call
    /// that times out ends the link, as any does.
    pub fn forward_within(&self, machine: &str, method: &str, params: Value, wait: Duration) -> Result<Response, ApiError> {
        let tx = self.inner.lock_links().get(machine).map(|l| l.tx.clone());
        let tx = match tx {
            Some(tx) => tx,
            None if self.routed_one(machine).is_some() => return self.call_routed(machine, method, params, wait),
            None => return Err(self.unknown(machine)),
        };
        let (reply, rx) = mpsc::channel();
        let stopping = || ApiError::internal(anyhow::anyhow!("{machine}: the link is stopping"));
        tx.send(Cmd::Call(Call { method: method.into(), params, wait, reply })).map_err(|_| stopping())?;
        // The thread always answers (in time or not); this bound only covers
        // a thread that's gone.
        let t = &self.inner.timing;
        let bound = t.handshake * 2 + wait + t.backoff_cap + Duration::from_secs(5);
        rx.recv_timeout(bound).map_err(|e| match e {
            RecvTimeoutError::Timeout => ApiError::internal(anyhow::anyhow!("{machine}: no answer")),
            RecvTimeoutError::Disconnected => stopping(),
        })
    }

    /// Whether `machine`'s own link here is connected: `Err` why not (its
    /// error, or no link to it at all).
    fn direct_up(&self, machine: &str) -> Result<(), String> {
        match self.inner.remotes.get(machine) {
            Some(m) if m.state == LinkState::Connected => Ok(()),
            Some(m) => Err(m.error.unwrap_or_else(|| "its link isn't up".into())),
            None => Err("no link to it".into()),
        }
    }

    /// Whether a route is worth trying to `machine`, whose own link isn't
    /// up (`direct`: why): one with a link here (down) may have a route
    /// socket bound (`fleet::routes::wanted` skips only links that are up);
    /// a routed one while the graph has a path starting with a link up
    /// here. Else why there's no way to it.
    fn route_worth(&self, machine: &str, direct: &str) -> Result<(), String> {
        if self.inner.lock_links().contains_key(machine) {
            return Ok(());
        }
        match self.routed_one(machine) {
            Some(r) if r.state == LinkState::Connected => Ok(()),
            Some(r) => Err(format!("{direct}, and {}", r.error.unwrap_or_else(|| "no route to it is up".into()))),
            None => Err(format!("{direct}, and no path to it in this host's link graph")),
        }
    }

    /// A fresh `API` stream to member `machine`'s daemon over any path:
    /// its own link while that's connected, else a route (the
    /// [`RouteDial`]). Why neither, naming the link's and the route's
    /// failures. What the fleet's calls to a member dial (the hub's
    /// `fleet.place`, a placement's `capacity.get` and `roots.list`).
    pub fn open_member(&self, machine: &str) -> Result<Duplex, String> {
        let direct = match self.direct_up(machine) {
            Ok(()) => {
                let entry = self.inner.lock_links().get(machine).map(|l| l.entry.clone());
                if let Some(entry) = entry {
                    return (self.inner.dial)(&entry).map_err(|e| format!("{e:#}"));
                }
                "no link to it".to_string()
            }
            Err(why) => why,
        };
        self.route_worth(machine, &direct)?;
        (self.inner.route)(machine).map_err(|e| format!("{direct}; over a route: {e:#}"))
    }

    /// `method` on member `machine` over any path ([`Self::open_member`]'s
    /// rule): over its link while connected ([`Self::forward`]), else a
    /// routed stream of its own ([`Self::call_routed`]). The remote's
    /// response as is, or why it didn't get there. The fleet's membership
    /// calls go this way (`leave::call`).
    pub fn call_member(&self, machine: &str, method: &str, params: Value) -> Result<Response, String> {
        match self.direct_up(machine) {
            Ok(()) => self.forward(machine, method, params).map_err(|e| e.message),
            Err(direct) => {
                self.route_worth(machine, &direct)?;
                self.call_routed(machine, method, params, self.inner.timing.call).map_err(|e| format!("{direct}; over a route: {}", e.message))
            }
        }
    }

    /// `method` on routed machine `machine`: an `API` stream opened over
    /// the [`RouteDial`], the hello within [`Timing::handshake`], the call
    /// within `wait`, then closed. No path or a failed hello is `failed`,
    /// why.
    fn call_routed(&self, machine: &str, method: &str, params: Value, wait: Duration) -> Result<Response, ApiError> {
        let t = &self.inner.timing;
        let duplex = (self.inner.route)(machine).map_err(|e| ApiError::failed(format!("{machine}: unreachable: {e:#}")))?;
        let mut link = link::handshake(machine, duplex, t.handshake, link::Over::Link).map_err(|e| ApiError::failed(e.to_string()))?;
        match link.conn.call_within(method, params, wait) {
            Ok(Some(r)) => Ok(r),
            Ok(None) => Err(ApiError::internal(anyhow::anyhow!("{machine}: timed out"))),
            Err(e) => Err(ApiError::failed(format!("{machine}: {e:#}"))),
        }
    }
}

fn spawn_link(entry: Entry, inner: Arc<Inner>) -> LinkThread {
    let (tx, rx) = mpsc::channel();
    let ctl = Arc::new(Ctl::default());
    let log = inner.log;
    let thread = {
        let (entry, ctl, inner) = (entry.clone(), Arc::clone(&ctl), Arc::clone(&inner));
        std::thread::Builder::new()
            .name(format!("serve-machine-{}", entry.name))
            .spawn(move || LinkLoop::new(entry, &inner, &ctl).run(&rx))
    };
    let thread = thread.map_err(|e| log(&format!("cannot start machine {}'s link: {e}", entry.name))).ok();
    // An inbound machine's carrier is the `fleet-link --peer` that
    // announced it.
    let carrier = carried(&entry).then(|| {
        let (entry, ctl, tx) = (entry.clone(), Arc::clone(&ctl), tx.clone());
        std::thread::Builder::new()
            .name(format!("serve-machine-{}-link", entry.name))
            .spawn(move || carry(&entry, &inner, &ctl, &tx))
    });
    let carrier = carrier.and_then(|c| c.map_err(|e| log(&format!("cannot start machine {}'s carrier: {e}", entry.name))).ok());
    LinkThread { entry, tx, ctl, thread, carrier }
}

/// The delay before the next attempt after `failures` failures in a row: 1,
/// 2, 4 … units, capped; counts this one.
pub(crate) fn backoff(t: &Timing, failures: &mut u32) -> Duration {
    let delay = t.backoff.saturating_mul(1u32 << (*failures).min(16)).min(t.backoff_cap);
    *failures = failures.saturating_add(1);
    delay
}

/// An ssh machine's carrier thread: the [`Hold`] again and again until the
/// stop, the backoff between (reset once a carrier came up). Each end is
/// the carrier's `why` (the machine's `error` until the next one is up),
/// logged as [`log_failure`] says.
fn carry(entry: &Entry, inner: &Inner, ctl: &Ctl, tx: &Sender<Cmd>) {
    let mut failures = 0;
    let mut logged = None;
    while !ctl.stopped() {
        // A cycle before this attempt (in the backoff) is spent by it.
        ctl.cycled.store(false, Ordering::SeqCst);
        let held = Held { ctl, tx, came_up: AtomicBool::new(false) };
        let mut why = (inner.hold)(entry, &held);
        let cycled = ctl.cycled.swap(false, Ordering::SeqCst);
        if cycled {
            why = "this host's hub changed: redialing".into();
        }
        {
            let mut c = lock(&ctl.carrier);
            c.up = false;
            c.why = Some(why.clone());
            c.stream = None;
        }
        let _ = tx.send(Cmd::Wake);
        if ctl.stopped() {
            break;
        }
        if held.came_up.load(Ordering::SeqCst) {
            failures = 0;
            logged = None;
        }
        if cycled {
            continue;
        }
        if log_failure(&mut logged, &why) {
            (inner.log)(&format!("machine {}: {why}", entry.name));
        }
        ctl.sleep(backoff(&inner.timing, &mut failures));
    }
}

/// Why a connected link ended.
enum End {
    /// The service stops it.
    Stopped,
    /// Lost: the reason, for `error`.
    Lost(String),
}

/// One machine's thread state.
struct LinkLoop<'a> {
    entry: Entry,
    inner: &'a Inner,
    ctl: &'a Ctl,
    /// Forwarded calls not answered yet, oldest first.
    pending: VecDeque<Call>,
    failures: u32,
    /// The last failure logged since the link was last up.
    logged: Option<String>,
    /// The carrier generation this link last dialed.
    generation: u64,
    /// The machine's last `roots.list` answer over this link (its `fleet`
    /// row is the hub's fleet root for this host).
    listing: Vec<crate::serve::api::RootCandidate>,
    /// A [`Cmd::Relist`] came: list again on the connected loop.
    relist: bool,
}

impl<'a> LinkLoop<'a> {
    /// `entry`'s roots stay empty until the link lists: they are what this
    /// link watches and stores pushes of.
    fn new(entry: Entry, inner: &'a Inner, ctl: &'a Ctl) -> Self {
        Self { entry, inner, ctl, pending: VecDeque::new(), failures: 0, logged: None, generation: 0, listing: Vec::new(), relist: false }
    }

    /// A command for the loop (a call is queued).
    fn take(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Call(call) => self.pending.push_back(call),
            Cmd::Relist => self.relist = true,
            Cmd::Wake => {}
        }
    }

    fn run(&mut self, rx: &Receiver<Cmd>) {
        let tick = self.inner.timing.tick;
        while !self.ctl.stopped() {
            if let Some(why) = self.carrier_down() {
                match why {
                    Some(why) => {
                        self.set(LinkState::Unreachable, Some(why.clone()));
                        self.fail_pending(&why);
                    }
                    // Its first attempt is under way: calls wait for it.
                    None => self.set(LinkState::Connecting, None),
                }
                if !self.recv(rx, tick) {
                    break;
                }
                continue;
            }
            self.set(LinkState::Connecting, None);
            let failed = match self.connect() {
                Ok(mut link) => {
                    self.failures = 0;
                    self.logged = None;
                    let remote = link.remote().version.clone();
                    self.inner.remotes.update(&self.entry.name, |m| m.version = Some(remote));
                    self.inner.rows_changed();
                    self.set(LinkState::Connected, None);
                    let end = self.serve(&mut link, rx);
                    self.ctl.hold(None);
                    drop(link);
                    match end {
                        End::Stopped => break,
                        End::Lost(why) => why,
                    }
                }
                Err(why) => why,
            };
            if self.ctl.stopped() {
                break;
            }
            // Its carrier went: that's the reason, at once.
            if self.carrier_moved() {
                continue;
            }
            if log_failure(&mut self.logged, &failed) {
                (self.inner.log)(&format!("machine {}: {failed}", self.entry.name));
            }
            self.set(LinkState::Unreachable, Some(failed.clone()));
            // A queued call doesn't wait out the backoff for an answer.
            self.fail_pending(&failed);
            let delay = self.backoff();
            let until = Instant::now() + delay;
            // A call arriving meanwhile retries at once, as does a carrier
            // that went or came back.
            while self.pending.is_empty() && !self.ctl.stopped() && !self.carrier_moved() {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() || !self.recv(rx, left.min(tick)) {
                    break;
                }
            }
        }
        for call in self.pending.drain(..) {
            let _ = call.reply.send(Response::err(None, "internal", format!("{}: the link is stopping", self.entry.name)));
        }
        self.ctl.hold(None);
    }

    /// `Some` while an ssh machine's carrier is down: why (`None` before
    /// its first attempt ended). A new carrier resets the backoff.
    fn carrier_down(&mut self) -> Option<Option<String>> {
        if !carried(&self.entry) {
            return None;
        }
        let c = lock(&self.ctl.carrier);
        if !c.up {
            return Some(c.why.clone());
        }
        if c.generation != self.generation {
            self.generation = c.generation;
            self.failures = 0;
            self.logged = None;
        }
        None
    }

    /// An ssh machine's carrier went down, or came up again, since this
    /// link dialed it.
    fn carrier_moved(&self) -> bool {
        if !carried(&self.entry) {
            return false;
        }
        let c = lock(&self.ctl.carrier);
        !c.up || c.generation != self.generation
    }

    /// Answer every queued call: the machine is unreachable, `why`.
    fn fail_pending(&mut self, why: &str) {
        for call in self.pending.drain(..) {
            let _ = call.reply.send(Response::err(None, "failed", format!("{}: unreachable: {why}", self.entry.name)));
        }
    }

    fn backoff(&mut self) -> Duration {
        backoff(&self.inner.timing, &mut self.failures)
    }

    /// Wait up to `wait` for a command; `false` when the channel is gone.
    fn recv(&mut self, rx: &Receiver<Cmd>, wait: Duration) -> bool {
        match rx.recv_timeout(wait) {
            Ok(cmd) => self.take(cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
        true
    }

    fn set(&self, state: LinkState, error: Option<String>) {
        let mut moved = false;
        self.inner.remotes.update(&self.entry.name, |m| moved = m.set_link(state, error));
        if moved {
            (self.inner.hooks.changed)();
        }
    }

    /// Dial and say hello. The socket is held for [`Ctl::cut`] before the
    /// hello.
    fn connect(&self) -> Result<Link, String> {
        let duplex = (self.inner.dial)(&self.entry).map_err(|e| format!("{}: {e:#}", self.entry.name))?;
        self.ctl.hold(duplex.stream.try_clone().ok());
        if self.ctl.stopped() {
            return Err("stopping".into());
        }
        link::handshake(&self.entry.name, duplex, self.inner.timing.handshake, link::Over::Link).map_err(|e| e.to_string())
    }

    /// The connected loop: queued calls first, else a short wait for a
    /// notification, so neither starves the other.
    fn serve(&mut self, link: &mut Link, rx: &Receiver<Cmd>) -> End {
        let name = self.entry.name.clone();
        let t = self.inner.timing;
        if let Err(why) = self.quick(link, "subscribe", json!({ "topics": ["instances"] })) {
            return End::Lost(why);
        }
        // Every link-up lists: the machine may have new roots since.
        self.relist = true;
        let mut watched = Watched::default();
        loop {
            if self.ctl.stopped() {
                return End::Stopped;
            }
            // The connection rides the carrier; its end is the carrier's.
            if self.carrier_moved() {
                return End::Lost(format!("{name}: its link went down"));
            }
            loop {
                match rx.try_recv() {
                    Ok(cmd) => self.take(cmd),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return End::Stopped,
                }
            }
            if std::mem::take(&mut self.relist)
                && let Err(why) = self.list_roots(link)
            {
                return End::Lost(why);
            }
            // The remote polls its runtime for a watch: only while a local
            // watcher wants machines. This host's hub is watched federated
            // too, for the machines it sees.
            let roots = (self.inner.hooks.watching)();
            let hub = (self.inner.hooks.hub)().filter(|h| h.hub == name);
            if hub.is_none() {
                self.drop_fleet();
            }
            let want = Want { roots, fleet: roots && hub.is_some() };
            if want != watched.asked || (want.roots && watched.roots != self.entry.roots) {
                if let Err(why) = self.watch(link, &mut watched, want) {
                    return End::Lost(why);
                }
            }
            if let Some(call) = self.pending.pop_front() {
                let answer = link.conn.call_within(&call.method, call.params, call.wait);
                match answer {
                    Ok(Some(r)) => {
                        let _ = call.reply.send(r);
                    }
                    Ok(None) => {
                        let _ = call.reply.send(Response::err(None, "internal", format!("{name}: timed out")));
                        return End::Lost(format!("{name}: `{}` timed out", call.method));
                    }
                    Err(e) => {
                        let _ = call.reply.send(Response::err(None, "failed", format!("{name}: {e:#}")));
                        return End::Lost(format!("{name}: {e:#}"));
                    }
                }
                continue;
            }
            match link.conn.next_notification(t.tick) {
                Ok(Some(n)) if n.method == "instances.snapshot" => self.store(n.params, watched.fleet.as_deref()),
                Ok(Some(n)) if n.method == "closing" => {
                    let reason = n.params.get("reason").and_then(Value::as_str).unwrap_or("closing");
                    return End::Lost(format!("{name}: its devsandbox serve is exiting ({reason})"));
                }
                Ok(_) => {}
                Err(e) => return End::Lost(format!("{name}: {e:#}")),
            }
        }
    }

    /// Ask the machine's `roots.list`: its rows kept for the hub's fleet
    /// root ([`Self::watch`]), its roots the machine's from now on, in the
    /// answer's order ([`Self::apply_roots`]). A row with an `error` (a recorded root
    /// whose directory is gone) is dropped unless it's the `fleet` one:
    /// its watch would only be refused. An error answer keeps the last
    /// listing (logged); a dead connection is lost.
    fn list_roots(&mut self, link: &mut Link) -> Result<(), String> {
        let name = self.entry.name.clone();
        let r = link
            .conn
            .call_within("roots.list", json!({}), self.inner.timing.call)
            .map_err(|e| format!("{name}: {e:#}"))?
            .ok_or_else(|| format!("{name}: `roots.list` timed out"))?;
        let mut rows = match (r.error, r.result.map(serde_json::from_value::<Vec<crate::serve::api::RootCandidate>>)) {
            (None, Some(Ok(rows))) => rows,
            (Some(e), _) => {
                (self.inner.log)(&format!("machine {name}: roots.list: {}", e.message));
                return Ok(());
            }
            (None, _) => {
                (self.inner.log)(&format!("machine {name}: bad roots.list answer"));
                return Ok(());
            }
        };
        rows.retain(|c| c.error.is_none() || c.fleet);
        let listed: Vec<PathBuf> = rows.iter().map(|c| c.root.clone()).collect();
        self.listing = rows;
        self.apply_roots(listed);
        Ok(())
    }

    /// Make `listed` (a `roots.list` answer) the machine's roots, here and
    /// in `Remotes`: a root no longer listed loses its rows (a hub's
    /// restored ones too). Under the links lock, as `reload` syncs
    /// `Remotes` under it, so a link no longer in the table (stopping)
    /// leaves `Remotes` to the reload. No deadlock: the lock order is
    /// `reload`'s (links, then `Remotes`), nothing holding the links lock
    /// waits on this thread (`stop_all` joins outside it), and the hooks
    /// run after it's released.
    fn apply_roots(&mut self, listed: Vec<PathBuf>) {
        let name = self.entry.name.clone();
        let inner = self.inner;
        let mut moved = false;
        self.entry.roots = listed;
        {
            let links = inner.lock_links();
            if links.contains_key(&name) {
                let roots = &self.entry.roots;
                inner.remotes.update(&name, |m| {
                    moved = m.roots != *roots;
                    m.roots.clone_from(roots);
                    m.rows.retain(|root, _| roots.contains(root));
                });
            }
        }
        if moved {
            self.inner.rows_changed();
            (self.inner.hooks.changed)();
        }
    }

    /// Go from what `watched` has to `want`: everything watched is
    /// unwatched, then each root `instances.watch`ed plain and, for the
    /// hub, its fleet root for this host (the last `roots.list`'s `fleet`
    /// row: there even on a hub with no config root of its own) with
    /// `machines: true`. A root the remote refuses (gone since it
    /// listed) is the machine's `error`, the others still watched; a dead
    /// connection is lost.
    fn watch(&self, link: &mut Link, watched: &mut Watched, want: Want) -> Result<(), String> {
        let name = &self.entry.name;
        let t = self.inner.timing;
        let call = |link: &mut Link, method: &str, params: Value| {
            link.conn
                .call_within(method, params, t.call)
                .map_err(|e| format!("{name}: {e:#}"))?
                .ok_or_else(|| format!("{name}: `{method}` timed out"))
        };
        for root in std::mem::take(&mut watched.roots) {
            call(link, "instances.unwatch", json!({ "dir": root }))?;
        }
        if let Some(dir) = watched.fleet.take() {
            call(link, "instances.unwatch", json!({ "dir": dir }))?;
        }
        watched.asked = want;
        let mut refused = Vec::new();
        if want.fleet {
            match crate::serve::fleet::placed::fleet_root(&self.listing) {
                Some(dir) => {
                    let r = call(link, "instances.watch", json!({ "dir": dir, "machines": true }))?;
                    match r.error {
                        Some(e) => refused.push(format!("the fleet through it: {}", e.message)),
                        None => watched.fleet = Some(dir),
                    }
                }
                None => refused.push("the fleet through it: it lists no fleet root for this host".to_string()),
            }
        }
        if want.roots {
            watched.roots.clone_from(&self.entry.roots);
            for root in &self.entry.roots {
                // Watched federated already: its own rows come in that push.
                if watched.fleet.as_ref() == Some(root) {
                    continue;
                }
                let r = call(link, "instances.watch", json!({ "dir": root }))?;
                if let Some(e) = r.error {
                    refused.push(format!("{}: {}", root.display(), e.message));
                }
            }
        }
        let error = (!refused.is_empty()).then(|| refused.join("; "));
        self.set(LinkState::Connected, error);
        Ok(())
    }

    /// This machine is no longer this host's hub: its fleet view goes.
    fn drop_fleet(&self) {
        let mut had = false;
        self.inner.remotes.update(&self.entry.name, |m| had = m.fleet.take().is_some());
        if had {
            (self.inner.hooks.changed)();
        }
    }

    /// A call of the link's own; an error answer or a lost connection is the
    /// reason it's lost.
    fn quick(&self, link: &mut Link, method: &str, params: Value) -> Result<(), String> {
        let name = &self.entry.name;
        match link.conn.call_within(method, params, self.inner.timing.call) {
            Ok(Some(r)) => match r.error {
                Some(e) => Err(format!("{name}: {method}: {} ({})", e.message, e.code)),
                None => Ok(()),
            },
            Ok(None) => Err(format!("{name}: `{method}` timed out")),
            Err(e) => Err(format!("{name}: {e:#}")),
        }
    }

    /// A pushed snapshot of one of the machine's roots: its rows now. The
    /// push of `fleet` (the hub's fleet root, watched federated) is split:
    /// the machines it sees are the hub's fleet view
    /// (`snapshots::fleet_view`), the rest that root's own rows.
    fn store(&self, mut params: Value, fleet: Option<&std::path::Path>) {
        let Some(dir) = params.get("dir").and_then(Value::as_str).map(PathBuf::from) else { return };
        if fleet == Some(dir.as_path()) {
            let me = (self.inner.hooks.hub)().map(|h| h.me);
            let view = Arc::new(snapshots::fleet_view(&mut params, me.as_deref()));
            self.inner.remotes.update(&self.entry.name, |m| m.fleet = Some(view));
            if !self.entry.roots.contains(&dir) {
                (self.inner.hooks.changed)();
                return;
            }
        }
        if !self.entry.roots.contains(&dir) {
            return;
        }
        // A reconnect's first push is disk-depth: keep the last statuses.
        let mut stored = false;
        self.inner.remotes.update(&self.entry.name, |m| {
            // The stored roots too, not only this thread's: a machine no
            // longer in the table is the reload's to drop.
            if !m.roots.contains(&dir) {
                return;
            }
            if let Some(prev) = m.rows.get(&dir) {
                snapshots::carry_remote_status(prev, &mut params);
            }
            m.rows.insert(dir, Arc::new(params));
            m.seen = Some(now_unix());
            stored = true;
        });
        if !stored {
            return;
        }
        self.inner.rows_changed();
        (self.inner.hooks.changed)();
    }
}

/// What a link wants watched on its machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Want {
    /// Each of the machine's roots (a local watcher wants machines).
    roots: bool,
    /// The fleet through it (it's this host's hub, and `roots`).
    fleet: bool,
}

/// What a link has watched on its machine.
#[derive(Debug, Default)]
struct Watched {
    /// What it last asked for (a refused fleet watch isn't retried every
    /// tick: only once the want changes, or on the next connect).
    asked: Want,
    /// The hub's fleet root for this host, watched with `machines: true`.
    fleet: Option<PathBuf>,
    /// The machine's roots as last watched: the ones to unwatch, and a
    /// new listing of other roots watches again.
    roots: Vec<PathBuf>,
}

/// Whether a link's `failed` goes to the log (`logged`: the last one that
/// did since the link was up): only when it differs from that one, since
/// a link is retried for as long as its machine is away.
fn log_failure(logged: &mut Option<String>, failed: &str) -> bool {
    if logged.as_deref() == Some(failed) {
        return false;
    }
    *logged = Some(failed.to_string());
    true
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::serve::daemon::tests::opts;
    use crate::serve::machines::inbound;
    use crate::serve::machines::ssh::Duplex;
    use crate::serve::snapshots::Machine;

    /// A fake machine daemon: answers the hello, `subscribe` and the
    /// link-up's `roots.list` (no roots), never anything else.
    fn mute_peer() -> Duplex {
        let (ours, theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut r = BufReader::new(theirs.try_clone().unwrap());
            let mut line = String::new();
            while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                let req: Value = serde_json::from_str(&line).unwrap();
                let answer = match req["method"].as_str() {
                    Some("hello") => json!({"id": req["id"], "result": {"version": "0.6.0", "build": 1, "protocol": 1}}),
                    Some("subscribe") => json!({"id": req["id"], "result": {"ok": true}}),
                    Some("roots.list") => json!({"id": req["id"], "result": []}),
                    _ => Value::Null,
                };
                if !answer.is_null() && writeln!(&theirs, "{answer}").is_err() {
                    break;
                }
                line.clear();
            }
        });
        Duplex::socket(ours)
    }

    /// A short scratch dir.
    fn short_scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-ms-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wait_for(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + within;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A fake carrier: up (until the stop, or `up` goes false: then
    /// `closed`) while `up` says so, else failing with `ssh: no route`;
    /// counts its runs by machine name.
    fn fake_hold(up: &Arc<AtomicBool>, runs: &Arc<Mutex<Vec<String>>>) -> Hold {
        let (up, runs) = (Arc::clone(up), Arc::clone(runs));
        Arc::new(move |e: &Entry, held: &Held| {
            runs.lock().unwrap().push(e.name.clone());
            if !up.load(Ordering::SeqCst) {
                return "ssh: no route".into();
            }
            held.up();
            while !held.stopped() && up.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            "link closed".into()
        })
    }

    fn ran(runs: &Mutex<Vec<String>>, name: &str) -> usize {
        runs.lock().unwrap().iter().filter(|n| *n == name).count()
    }

    fn no_hooks() -> Hooks {
        Hooks { watching: Box::new(|| false), changed: Box::new(|| {}), hub: Box::new(|| None), routed: Box::new(Vec::new) }
    }

    /// No routed machine is ever dialed in these tests.
    fn no_route() -> RouteDial {
        Arc::new(|name: &str| anyhow::bail!("no route to `{name}`"))
    }

    fn counting_dial(dials: &Arc<AtomicUsize>) -> Dial {
        let dials = Arc::clone(dials);
        Arc::new(move |_| {
            dials.fetch_add(1, Ordering::SeqCst);
            Ok(mute_peer())
        })
    }

    /// A call the machine doesn't answer in time answers `internal`
    /// and drops the link (a late answer would desync the ids), not its
    /// carrier; the next call dials again. An unregistered machine is
    /// `not-found`.
    #[test]
    fn a_timed_out_call_drops_the_link() {
        let base = short_scratch("link-timeout");
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("m", "m"))).unwrap();
        let dials = Arc::new(AtomicUsize::new(0));
        let (up, runs) = (Arc::new(AtomicBool::new(true)), Arc::default());
        let timing = Timing { call: Duration::from_millis(200), ..opts(1, 0).machine_timing };
        let machines = Machines::spawn(registry, Arc::default(), counting_dial(&dials), no_route(), fake_hold(&up, &runs), timing, no_hooks(), None, |_| {});
        let h = machines.handle();
        for round in 1..=2 {
            let r = h.forward("m", "instances.list", json!({"dir": "/x"})).unwrap();
            let e = r.error.unwrap();
            assert_eq!((e.code.as_str(), e.message.as_str()), ("internal", "m: timed out"));
            assert_eq!(dials.load(Ordering::SeqCst), round);
        }
        assert_eq!(ran(&runs, "m"), 1, "the carrier stayed up");
        assert_eq!(h.forward("n", "instances.list", json!({})).unwrap_err().code, "not-found");
        let started = Instant::now();
        drop(machines);
        assert!(started.elapsed() < Duration::from_secs(2), "the stop waited");
        assert!(h.forward("m", "instances.list", json!({})).is_err(), "stopped");
        let _ = std::fs::remove_dir_all(&base);
    }

    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// An ssh machine is held with nobody wanting it: while its carrier
    /// fails it's `unreachable` with why (logged once however often it's
    /// retried), a call fails at once with it; the carrier coming up is
    /// dialed at once and connects; it going down drops the link.
    #[test]
    fn a_machine_follows_its_carrier() {
        let base = short_scratch("carrier");
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("c", "me@c"))).unwrap();
        let dials = Arc::new(AtomicUsize::new(0));
        let (up, runs) = (Arc::new(AtomicBool::new(false)), Arc::default());
        let timing = opts(1, 0).machine_timing;
        let log = |line: &str| LOGGED.lock().unwrap().push(line.into());
        let machines = Machines::spawn(registry, Arc::default(), counting_dial(&dials), no_route(), fake_hold(&up, &runs), timing, no_hooks(), None, log);
        let h = machines.handle();
        let state = || h.remotes().get("c").unwrap();
        wait_for("the carrier retried", Duration::from_secs(5), || ran(&runs, "c") >= 3);
        let s = state();
        assert_eq!((s.state, s.error.as_deref()), (LinkState::Unreachable, Some("ssh: no route")));
        let logged: Vec<_> = LOGGED.lock().unwrap().iter().filter(|l| l.starts_with("machine c: ")).cloned().collect();
        assert_eq!(logged, ["machine c: ssh: no route"], "one line for one way of failing");
        let started = Instant::now();
        let e = h.forward("c", "instances.list", json!({})).unwrap().error.unwrap();
        assert_eq!((e.code.as_str(), e.message.as_str()), ("failed", "c: unreachable: ssh: no route"));
        assert!(started.elapsed() < Duration::from_secs(1), "no wait for a down carrier");
        assert_eq!(dials.load(Ordering::SeqCst), 0, "nothing dialed over a down carrier");

        up.store(true, Ordering::SeqCst);
        wait_for("connected", Duration::from_secs(5), || state().state == LinkState::Connected);
        assert_eq!((dials.load(Ordering::SeqCst), state().version.as_deref()), (1, Some("0.6.0")));
        up.store(false, Ordering::SeqCst);
        wait_for("unreachable again", Duration::from_secs(5), || state().state == LinkState::Unreachable);
        up.store(true, Ordering::SeqCst);
        wait_for("connected again", Duration::from_secs(5), || state().state == LinkState::Connected);
        assert_eq!(dials.load(Ordering::SeqCst), 2, "one dial per carrier");
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- inbound machines and a hub -----------------------------------------

    static HUB_LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Inbound neighbor `w` (announced: the returned guard) and ssh machine
    /// `s`, nobody watching; `w`'s fleet-link socket is "up" while the flag
    /// says so (a [`mute_peer`]), else its dial fails as
    /// `fleet::dialer::dial`'s does; `s`'s carrier always fails. Dials and
    /// carrier runs by name.
    #[allow(clippy::type_complexity)]
    fn inbound_and_ssh(
        base: &Path,
        hub: Option<Hub>,
        log: fn(&str),
    ) -> (Machines, inbound::Guard, Arc<AtomicBool>, Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<String>>>) {
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("s", "me@s"))).unwrap();
        let neighbors: Arc<inbound::Neighbors> = Arc::default();
        let guard = neighbors.announce("w", "0.6.0 abc dev x/y member", false);
        let up = Arc::new(AtomicBool::new(false));
        let dials = Arc::new(Mutex::new(Vec::new()));
        let dial: Dial = {
            let (up, dials) = (Arc::clone(&up), Arc::clone(&dials));
            Arc::new(move |e: &Entry| {
                dials.lock().unwrap().push(e.name.clone());
                if e.kind == Kind::Inbound && up.load(Ordering::SeqCst) {
                    return Ok(mute_peer());
                }
                anyhow::bail!("its link isn't up (fleet-{}.sock: No such file or directory)", e.name)
            })
        };
        let runs = Arc::new(Mutex::new(Vec::new()));
        let hold = fake_hold(&Arc::new(AtomicBool::new(false)), &runs);
        let machines = Machines::spawn(registry, neighbors, dial, no_route(), hold, opts(1, 0).machine_timing, no_hooks(), hub, log);
        (machines, guard, up, dials, runs)
    }

    /// Every link is held with no watcher: an inbound one retries an absent
    /// socket (logging that once) and connects when it comes, with no
    /// carrier of its own; an ssh one runs its carrier, never dialing its
    /// socket while that's down.
    #[test]
    fn every_link_is_held() {
        let base = short_scratch("held");
        let (machines, _w, up, dials, runs) = inbound_and_ssh(&base, None, |l| HUB_LOGGED.lock().unwrap().push(l.into()));
        let h = machines.handle();
        wait_for("the inbound link retrying", Duration::from_secs(5), || ran(&dials, "w") >= 3);
        assert_eq!(h.remotes().get("w").unwrap().state, LinkState::Unreachable);
        let logged: Vec<_> = HUB_LOGGED.lock().unwrap().iter().filter(|l| l.starts_with("machine w: ")).cloned().collect();
        assert_eq!(logged.len(), 1, "one line for one way of failing: {logged:?}");
        assert!(logged[0].contains("its link isn't up"), "{}", logged[0]);

        up.store(true, Ordering::SeqCst);
        wait_for("the inbound link up", Duration::from_secs(5), || h.remotes().get("w").unwrap().state == LinkState::Connected);
        assert_eq!(ran(&runs, "w"), 0, "no carrier for an inbound machine");
        assert!(ran(&runs, "s") >= 1, "the ssh machine's carrier runs");
        assert_eq!(ran(&dials, "s"), 0, "nothing dialed over a down carrier");
        assert_eq!(h.remotes().get("s").unwrap().state, LinkState::Unreachable);
        assert!(!base.join("remotes.json").exists(), "only a hub saves");
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A hub restores `remotes.json` (unreachable until the link connects,
    /// machines no longer registered dropped; roots and rows kept until
    /// the machine's link lists) and saves it back at the stop.
    #[test]
    fn a_hub_restores_and_saves_remotes() {
        let base = short_scratch("hub-save");
        let path = base.join("remotes.json");
        let rows = json!({"dir": "/r", "instances": [{"name": "web-1"}]});
        let row = |name: &str| Machine {
            name: name.into(),
            roots: vec![PathBuf::from("/r")],
            rows: [(PathBuf::from("/r"), Arc::new(rows.clone()))].into(),
            seen: Some(1_761_566_400),
            ..Machine::default()
        };
        saved::save(&path, &saved::of(&[row("s"), row("w"), row("gone")], 0)).unwrap();
        let hub = Hub { remotes: path.clone(), is_hub: Arc::new(|| true) };
        let (machines, _w, up, _, runs) = inbound_and_ssh(&base, Some(hub), |_| {});
        let h = machines.handle();
        assert!(h.remotes().get("gone").is_none(), "not registered: dropped");
        // Nothing listed yet: the restored roots and rows stay.
        let s = h.remotes().get("s").unwrap();
        assert!(s.restored && s.rows.contains_key(Path::new("/r")));
        assert_eq!(h.entries().iter().map(|e| e.roots.clone()).collect::<Vec<_>>(), vec![vec![PathBuf::from("/r")], vec![PathBuf::from("/r")]]);
        wait_for("s's carrier failed", Duration::from_secs(5), || ran(&runs, "s") >= 1);
        wait_for("s unreachable", Duration::from_secs(5), || h.remotes().get("s").unwrap().error.is_some());
        let s = h.remotes().get("s").unwrap();
        assert_eq!(s.state, LinkState::Unreachable, "restored: unreachable");
        assert!(s.error.as_deref().unwrap().ends_with("(rows from Oct 27 12:00 UTC)"), "{:?}", s.error);

        // `w` lists no roots once its link is up: its restored `/r` goes.
        up.store(true, Ordering::SeqCst);
        wait_for("w listed", Duration::from_secs(5), || h.remotes().get("w").unwrap().roots.is_empty());
        let w = h.remotes().get("w").unwrap();
        assert!(w.rows.is_empty() && h.remotes().get("s").unwrap().rows.contains_key(Path::new("/r")));
        drop(machines);
        let saved = saved::load(&path).unwrap();
        assert_eq!(saved.machines.keys().collect::<Vec<_>>(), ["s", "w"], "gone dropped");
        assert_eq!(saved.machines["s"].roots, [PathBuf::from("/r")], "s never listed: as restored");
        assert!(saved.machines["w"].rows.is_empty() && saved.machines["w"].roots.is_empty());
        assert_eq!(saved.machines["w"].version.as_deref(), Some("0.6.0"), "w's hello");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Not the hub at the start: nothing restored, nothing saved; the flag
    /// flipping on (member.set) saves from the next change.
    #[test]
    fn only_while_the_hub_are_remotes_saved() {
        let base = short_scratch("hub-flip");
        let path = base.join("remotes.json");
        let rows = json!({"dir": "/r", "instances": []});
        let saved_s = Machine { name: "s".into(), rows: [(PathBuf::from("/r"), Arc::new(rows))].into(), seen: Some(1), ..Machine::default() };
        saved::save(&path, &saved::of(&[saved_s], 0)).unwrap();
        let before = std::fs::read(&path).unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let is_hub = {
            let flag = Arc::clone(&flag);
            Arc::new(move || flag.load(Ordering::SeqCst))
        };
        let (machines, _w, up, _, _) = inbound_and_ssh(&base, Some(Hub { remotes: path.clone(), is_hub }), |_| {});
        let h = machines.handle();
        assert!(!h.remotes().get("s").unwrap().restored, "not the hub: nothing restored");
        up.store(true, Ordering::SeqCst);
        wait_for("w connected", Duration::from_secs(5), || h.remotes().get("w").unwrap().state == LinkState::Connected);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(std::fs::read(&path).unwrap(), before, "not the hub: nothing saved");

        flag.store(true, Ordering::SeqCst);
        h.reload();
        drop(machines);
        assert_eq!(saved::load(&path).unwrap().machines.keys().collect::<Vec<_>>(), ["w"], "the hub now: saved");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The hub at the start, then not (the role moved): restored, then
    /// nothing saved from the flip on, the stop included.
    #[test]
    fn a_hub_no_longer_the_hub_stops_saving_remotes() {
        let base = short_scratch("hub-unflip");
        let path = base.join("remotes.json");
        let rows = json!({"dir": "/r", "instances": []});
        let saved_s = Machine { name: "s".into(), rows: [(PathBuf::from("/r"), Arc::new(rows))].into(), seen: Some(1), ..Machine::default() };
        saved::save(&path, &saved::of(&[saved_s], 0)).unwrap();
        let flag = Arc::new(AtomicBool::new(true));
        let is_hub = {
            let flag = Arc::clone(&flag);
            Arc::new(move || flag.load(Ordering::SeqCst))
        };
        let (machines, _w, up, _, _) = inbound_and_ssh(&base, Some(Hub { remotes: path.clone(), is_hub }), |_| {});
        let h = machines.handle();
        assert!(h.remotes().get("s").unwrap().restored, "the hub: restored");
        flag.store(false, Ordering::SeqCst);
        // A save already past its check lands now; none after it.
        std::thread::sleep(Duration::from_millis(100));
        let before = std::fs::read(&path).unwrap();
        up.store(true, Ordering::SeqCst);
        wait_for("w connected", Duration::from_secs(5), || h.remotes().get("w").unwrap().state == LinkState::Connected);
        h.reload();
        drop(machines);
        assert_eq!(std::fs::read(&path).unwrap(), before, "no longer the hub: nothing saved");
        assert!(!saved::load(&path).unwrap().machines.contains_key("w"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An inbound neighbor's roots are its link's listing, as any
    /// machine's: listed and watched over that one link, no redial.
    #[test]
    fn an_inbound_neighbors_roots_are_its_links_listing() {
        let base = short_scratch("roots");
        let neighbors: Arc<inbound::Neighbors> = Arc::default();
        let guard = neighbors.announce("w", "0.6.0 abc dev x/y member", false);
        let (dials, calls) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(Vec::new())));
        let listed = Arc::new(Mutex::new(vec!["/r".to_string()]));
        let dial: Dial = {
            let (dials, calls) = (Arc::clone(&dials), Arc::clone(&calls));
            Arc::new(move |_| {
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(listing_peer(&calls, &listed))
            })
        };
        let hooks = Hooks { watching: Box::new(|| true), ..no_hooks() };
        let hold = fake_hold(&Arc::new(AtomicBool::new(false)), &Arc::default());
        let timing = opts(1, 0).machine_timing;
        let machines = Machines::spawn(base.join("machines.toml"), neighbors, dial, no_route(), hold, timing, hooks, None, |_| {});
        let h = machines.handle();
        wait_for("/r watched", Duration::from_secs(5), || calls.lock().unwrap().iter().any(|c| c == "instances.watch /r"));
        assert_eq!(dials.load(Ordering::SeqCst), 1, "no redial");
        assert_eq!(h.remotes().get("w").unwrap().state, LinkState::Connected);
        assert_eq!(h.entries()[0].roots, [PathBuf::from("/r")]);
        drop(machines);
        drop(guard);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A fake machine daemon whose `roots.list` answers `listed` as it is
    /// then (a `!/x` entry: root `/x` with an `error`, as a recorded root
    /// whose directory is gone lists, its watch refused `not-found`) and
    /// whose `sandboxes.list {root}` answers one `s` there; every other
    /// call `ok`, a watch followed by its root's (empty) snapshot, each
    /// call's `method dir|root` kept in `calls`.
    fn listing_peer(calls: &Arc<Mutex<Vec<String>>>, listed: &Arc<Mutex<Vec<String>>>) -> Duplex {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (calls, listed) = (Arc::clone(calls), Arc::clone(listed));
        std::thread::spawn(move || {
            let mut r = BufReader::new(theirs.try_clone().unwrap());
            let mut line = String::new();
            while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                let req: Value = serde_json::from_str(&line).unwrap();
                let method = req["method"].as_str().unwrap_or_default();
                let at = req["params"]["dir"].as_str().or(req["params"]["root"].as_str()).unwrap_or_default().to_string();
                calls.lock().unwrap().push(format!("{method} {at}"));
                let gone = |r: &str| listed.lock().unwrap().iter().any(|l| l.strip_prefix('!') == Some(r));
                let answer = match method {
                    "hello" => json!({"id": req["id"], "result": {"version": "0.6.0", "build": 1, "protocol": 1}}),
                    "roots.list" => {
                        let rows: Vec<Value> = listed
                            .lock()
                            .unwrap()
                            .iter()
                            .map(|r| match r.strip_prefix('!') {
                                Some(r) => json!({"root": r, "sandboxes": [], "error": "no such directory"}),
                                None => json!({"root": r, "sandboxes": [], "error": null}),
                            })
                            .collect();
                        json!({"id": req["id"], "result": rows})
                    }
                    "sandboxes.list" => json!({"id": req["id"], "result": [{"name": "s", "root": at}]}),
                    "instances.watch" if gone(&at) => {
                        json!({"id": req["id"], "error": {"code": "not-found", "message": format!("no config root at {at}")}})
                    }
                    _ => json!({"id": req["id"], "result": {"ok": true}}),
                };
                if writeln!(&theirs, "{answer}").is_err() {
                    break;
                }
                if method == "instances.watch" && answer["error"].is_null() {
                    let push = json!({"method": "instances.snapshot", "params": {"dir": at, "instances": []}});
                    if writeln!(&theirs, "{push}").is_err() {
                        break;
                    }
                }
                line.clear();
            }
        });
        Duplex::socket(ours)
    }

    /// A machine's roots are what its link lists: a root it lists later (a
    /// signed config copy it installed since) is, once its link lists
    /// again (`relist`, as a `fleet.sync` does), among its entry's roots:
    /// watched, and asked by a federated `sandboxes.list`, in the
    /// listing's order.
    #[test]
    fn a_root_listed_later_is_watched_and_listed() {
        let base = short_scratch("live-roots");
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("b", "me@b"))).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listed = Arc::new(Mutex::new(vec!["/a".to_string()]));
        let dial: Dial = {
            let (calls, listed) = (Arc::clone(&calls), Arc::clone(&listed));
            Arc::new(move |_| Ok(listing_peer(&calls, &listed)))
        };
        let hooks = Hooks { watching: Box::new(|| true), ..no_hooks() };
        let hold = fake_hold(&Arc::new(AtomicBool::new(true)), &Arc::default());
        let machines = Machines::spawn(registry.clone(), Arc::default(), dial, no_route(), hold, opts(1, 0).machine_timing, hooks, None, |_| {});
        let h = machines.handle();
        let called = |c: &str| calls.lock().unwrap().iter().any(|x| x == c);
        wait_for("/a watched", Duration::from_secs(5), || called("instances.watch /a"));
        assert_eq!(h.entries()[0].roots, [PathBuf::from("/a")]);

        listed.lock().unwrap().push("/data/mesh/tree/bolt".into());
        h.relist("b");
        wait_for("the new root watched", Duration::from_secs(5), || called("instances.watch /data/mesh/tree/bolt"));
        let live = [PathBuf::from("/a"), PathBuf::from("/data/mesh/tree/bolt")];
        assert_eq!(h.entries()[0].roots, live);
        assert_eq!(h.remotes().get("b").unwrap().roots, live, "its pushes are stored");
        let all = super::super::forward::sandboxes_everywhere(Ok(json!([])), Some(&h)).unwrap();
        assert_eq!(
            all,
            json!([
                {"name": "s", "machine": "b", "root": "/a"},
                {"name": "s", "machine": "b", "root": "/data/mesh/tree/bolt"},
            ])
        );
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Machine `b` listing `listed` (`/a` among them): its link up, `/a`
    /// watched.
    fn listing_machine(tag: &str, listed: Vec<String>) -> (PathBuf, Machines, Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<String>>>) {
        let base = short_scratch(tag);
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("b", "me@b"))).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listed = Arc::new(Mutex::new(listed));
        let dial: Dial = {
            let (calls, listed) = (Arc::clone(&calls), Arc::clone(&listed));
            Arc::new(move |_| Ok(listing_peer(&calls, &listed)))
        };
        let hooks = Hooks { watching: Box::new(|| true), ..no_hooks() };
        let hold = fake_hold(&Arc::new(AtomicBool::new(true)), &Arc::default());
        let machines = Machines::spawn(registry, Arc::default(), dial, no_route(), hold, opts(1, 0).machine_timing, hooks, None, |_| {});
        wait_for("/a watched", Duration::from_secs(5), || calls.lock().unwrap().iter().any(|c| c == "instances.watch /a"));
        (base, machines, calls, listed)
    }

    /// A listed root with an `error` (recorded there, its directory gone)
    /// is no live root: not watched, so no refusal on the machine's row.
    #[test]
    fn a_listed_root_with_an_error_is_not_watched() {
        let (base, machines, calls, _) = listing_machine("dead-root", vec!["/a".into(), "!/gone".into()]);
        let h = machines.handle();
        // Queued behind the watch that's under way: once it's answered,
        // that watch is done and the row's error set.
        assert!(h.forward("b", "ping", json!({})).unwrap().error.is_none());
        assert!(!calls.lock().unwrap().iter().any(|c| c == "instances.watch /gone"), "{:?}", calls.lock().unwrap());
        let b = h.remotes().get("b").unwrap();
        assert_eq!((b.state, b.error), (LinkState::Connected, None));
        assert_eq!(h.entries()[0].roots, [PathBuf::from("/a")]);
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A reload (the registry rewritten, the machine unchanged) keeps the
    /// link and what it listed: roots, rows, no relisting.
    #[test]
    fn a_reload_keeps_the_listed_roots() {
        let (base, machines, calls, _) = listing_machine("reload-keeps", vec!["/a".into()]);
        let h = machines.handle();
        let a = PathBuf::from("/a");
        wait_for("/a's push stored", Duration::from_secs(5), || h.remotes().get("b").unwrap().rows.contains_key(&a));
        h.reload();
        let b = h.remotes().get("b").unwrap();
        assert_eq!((b.roots, b.rows.contains_key(&a)), (vec![a.clone()], true));
        assert_eq!(h.entries()[0].roots, [a]);
        assert!(h.forward("b", "ping", json!({})).unwrap().error.is_none());
        assert_eq!(calls.lock().unwrap().iter().filter(|c| c.starts_with("roots.list")).count(), 1);
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `cycle` ends a live carrier and redials it at once, not after the
    /// backoff; a carrier down in its backoff is redialed at once too.
    #[test]
    fn a_cycle_redials_the_carrier_at_once() {
        let base = short_scratch("cycle");
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("c", "me@c"))).unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        // Up until its stream is shut (by `cycle` or the stop), or failing.
        let hold: Hold = {
            let (runs, fail) = (Arc::clone(&runs), Arc::clone(&fail));
            Arc::new(move |_: &Entry, held: &Held| {
                runs.fetch_add(1, Ordering::SeqCst);
                if fail.load(Ordering::SeqCst) {
                    return "ssh: no route".into();
                }
                let (ours, mut theirs) = UnixStream::pair().unwrap();
                if !held.hold(Some(ours)) {
                    return "stopping".into();
                }
                held.up();
                let _ = std::io::Read::read(&mut theirs, &mut [0u8; 1]);
                "link closed".into()
            })
        };
        let timing = Timing { backoff: Duration::from_secs(60), backoff_cap: Duration::from_secs(60), ..opts(1, 0).machine_timing };
        let no_dial: Dial = Arc::new(|_| anyhow::bail!("no link here"));
        let machines = Machines::spawn(registry, Arc::default(), no_dial, no_route(), hold, timing, no_hooks(), None, |_| {});
        let h = machines.handle();
        wait_for("up", Duration::from_secs(5), || runs.load(Ordering::SeqCst) == 1 && lock(&h.inner.lock_links()["c"].ctl.carrier).up);
        h.cycle();
        wait_for("redialed at once", Duration::from_secs(5), || runs.load(Ordering::SeqCst) == 2);
        // Down, in a 60 s backoff: a cycle redials it now.
        fail.store(true, Ordering::SeqCst);
        h.cycle();
        wait_for("failed", Duration::from_secs(5), || runs.load(Ordering::SeqCst) == 3);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(runs.load(Ordering::SeqCst), 3, "in its backoff");
        fail.store(false, Ordering::SeqCst);
        h.cycle();
        wait_for("redialed out of the backoff", Duration::from_secs(5), || runs.load(Ordering::SeqCst) == 4);
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }
}
