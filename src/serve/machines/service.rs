//! The held link per machine (docs/architecture/daemon/machines.md, *Held
//! links: `Machines`*): one `serve-machine-<name>` thread per registered
//! machine and per inbound neighbor up now (`inbound::Neighbors`, whose
//! hook reloads the service as one comes, goes or gets its roots), owned
//! by the [`Machines`] service the daemon runs for its whole life.
//!
//! Every link is held for as long as the service runs (fleet plan decision
//! 7), an inbound neighbor's while it's up. What carries it depends on the
//! machine's kind: an ssh machine's carrier is its peer link, held by a
//! `serve-machine-<name>-link` thread running the [`Hold`]
//! (`fleet::dialer`: `fleet-link --peer <this host>` there through the
//! transport, this end binding `fleet-<name>.*.sock` here), redialed after
//! the backoff below whenever it ends; an offered machine's is its worker's
//! `fleet-link --worker` here, an inbound neighbor's the `fleet-link --peer
//! <it>` that announced it. Once the carrier
//! is up the link thread dials `fleet-<name>.sock` (the [`Dial`]) and says
//! hello, subscribes to `instances`, watches each of the machine's roots
//! while a local connection watches with `machines: true`
//! ([`Hooks::watching`]), and stores every pushed `instances.snapshot` in
//! the shared [`Remotes`] ([`Hooks::changed`] then re-publishes). A carrier
//! that's down, a failed connect or a lost link is `unreachable` with the
//! reason, rows kept; a failed connect or lost link over a live carrier is
//! retried with a doubling backoff, a carrier coming back is dialed at
//! once. A failure is logged when it differs from the last one logged since
//! the link (or carrier) was last up, not per retry.
//!
//! A hub also saves `Remotes` to `remotes.json` (`machines::saved`) from a
//! `serve-remotes` thread, at most every `saved::SAVE_EVERY` while rows
//! change and once more at the stop, and restores it at the start: those
//! rows show `unreachable` until their machine's link connects.
//!
//! Forwarded calls reach the thread over its channel, one in flight
//! per machine, each with [`Timing::call`] to answer. A timed-out call drops
//! the link (its answer would come out of turn for the next call), not its
//! carrier. A call made while the carrier is down fails at once with why
//! (or waits for the carrier's first attempt to end).
//!
//! `agent` isn't the link's business: this host answers the machine's
//! `AGENT` streams per stream from the registry (`daemon::agent_for`), so a
//! `reload` that changes only `agent` keeps the live link.
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
use super::ssh::Dial;
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

/// What the service needs from the daemon. Both are called on link threads
/// and must not block.
pub struct Hooks {
    /// Whether any local connection watches a root with `machines: true`.
    pub watching: Box<dyn Fn() -> bool + Send + Sync>,
    /// A machine's rows or state changed: re-publish the federated
    /// snapshots.
    pub changed: Box<dyn Fn() + Send + Sync>,
}

/// How an ssh machine's link is carried: hold `entry`'s peer link until it
/// ends, telling `held` how it stands; returns why it ended. The daemon's
/// is `fleet::dialer::Dialer::hold`; tests fake it.
pub type Hold = Arc<dyn Fn(&Entry, &Held) -> String + Send + Sync>;

/// This daemon is a fleet hub (`[serve] hub = true`): `Remotes` lives on in
/// `remotes`.
pub struct Hub {
    /// `remotes.json` (`saved::path`; a scratch file in tests).
    pub remotes: PathBuf,
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
    /// Carries an ssh machine's link.
    hold: Hold,
    timing: Timing,
    hooks: Hooks,
    log: fn(&str),
    remotes: Arc<Remotes>,
    links: Mutex<BTreeMap<String, LinkThread>>,
    stopped: AtomicBool,
    /// `Some` on a fleet hub.
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
/// offered worker's and an inbound neighbor's is a `fleet-link` here.
fn carried(entry: &Entry) -> bool {
    matches!(entry.kind, Kind::Ssh { .. })
}

/// What a machine's link and carrier threads share with the service: how
/// it stops them wherever they are blocked, and how the carrier stands.
#[derive(Default)]
struct Ctl {
    stop: AtomicBool,
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

    /// Sleep up to `wait` unless stopped first.
    fn sleep(&self, wait: Duration) {
        let c = lock(&self.carrier);
        let _ = self.carrier_cv.wait_timeout_while(c, wait, |_| !self.stopped());
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
}

/// A forwarded call: the remote's response (its `id` is the remote's; the
/// daemon answers with its own) or a local error as a response.
struct Call {
    method: String,
    params: Value,
    reply: Sender<Response>,
}

impl Machines {
    /// Start the service: one link per entry of `registry` and per neighbor
    /// of `inbound` (from now on as they come and go), each held at once.
    /// `dial` opens a machine's link (its `fleet-<name>.sock`, or a test
    /// daemon's socket), `hold` carries an ssh machine's. A `hub`'s saved
    /// rows are restored before the links start.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        registry: PathBuf,
        inbound: Arc<Neighbors>,
        dial: Dial,
        hold: Hold,
        timing: Timing,
        hooks: Hooks,
        hub: Option<Hub>,
        log: fn(&str),
    ) -> Machines {
        let hub = hub.map(|h| HubSave { path: h.remotes, state: Mutex::default(), cv: Condvar::new() });
        let inner = Arc::new(Inner {
            registry,
            inbound,
            reloading: Mutex::new(()),
            dial,
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
            match saved::load(&h.path) {
                Ok(s) => inner.remotes.restore(saved::machines(s)),
                Err(e) => log(&format!("machines: {e:#}")),
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
            entries.push(Entry::inbound(n.name, n.roots));
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// The same link: everything but `agent`, which the live one follows.
fn same_link(a: &Entry, b: &Entry) -> bool {
    Entry { agent: b.agent, ..a.clone() } == *b
}

impl Handle {
    /// Re-read the registry and the inbound neighbors: start links for new
    /// machines, stop (and drop the rows of) removed ones, restart changed
    /// ones, except for a change of `agent` alone, which the live link
    /// keeps. `machines.add`, `.set` and `.rm` call it, as does the
    /// neighbors' hook; a registry that doesn't load is logged and changes
    /// nothing.
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
            for (name, l) in links.iter_mut() {
                let Some(e) = entries.iter().find(|e| e.name == *name) else { continue };
                if e.agent != l.entry.agent && same_link(e, &l.entry) {
                    l.entry.agent = e.agent;
                }
            }
            let stale: Vec<String> = links
                .iter()
                .filter(|(name, l)| !entries.iter().any(|e| e.name == **name && *e == l.entry))
                .map(|(name, _)| name.clone())
                .collect();
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

    /// Every link looks at its watchers now (a `machines: true` watch came).
    pub fn wake(&self) {
        for l in self.inner.lock_links().values() {
            let _ = l.tx.send(Cmd::Wake);
        }
    }

    /// The machines as of the last reload (the registry's and the inbound
    /// neighbors'), sorted by name: what a forwarded verb dials and a
    /// federated call fans out to, without a registry read per request.
    pub fn entries(&self) -> Vec<Entry> {
        self.inner.lock_links().values().map(|l| l.entry.clone()).collect()
    }

    /// The machines' states and rows.
    pub fn remotes(&self) -> &Arc<Remotes> {
        &self.inner.remotes
    }

    /// Call `method` on machine `machine`'s daemon over its link: the
    /// remote's response as is (its error code kept), or `not-found` for a
    /// machine that isn't registered.
    pub fn forward(&self, machine: &str, method: &str, params: Value) -> Result<Response, ApiError> {
        let tx = match self.inner.lock_links().get(machine) {
            Some(l) => l.tx.clone(),
            None => return Err(ApiError::not_found(format!("no machine named `{machine}`"))),
        };
        let (reply, rx) = mpsc::channel();
        let stopping = || ApiError::internal(anyhow::anyhow!("{machine}: the link is stopping"));
        tx.send(Cmd::Call(Call { method: method.into(), params, reply })).map_err(|_| stopping())?;
        // The thread always answers (in time or not); this bound only covers
        // a thread that's gone.
        let t = &self.inner.timing;
        let bound = t.handshake * 2 + t.call + t.backoff_cap + Duration::from_secs(5);
        rx.recv_timeout(bound).map_err(|e| match e {
            RecvTimeoutError::Timeout => ApiError::internal(anyhow::anyhow!("{machine}: no answer")),
            RecvTimeoutError::Disconnected => stopping(),
        })
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
    // An offered machine's carrier is its worker's `fleet-link` here, an
    // inbound one's the `fleet-link --peer` that announced it.
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
        let held = Held { ctl, tx, came_up: AtomicBool::new(false) };
        let why = (inner.hold)(entry, &held);
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
}

impl<'a> LinkLoop<'a> {
    fn new(entry: Entry, inner: &'a Inner, ctl: &'a Ctl) -> Self {
        Self { entry, inner, ctl, pending: VecDeque::new(), failures: 0, logged: None, generation: 0 }
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
            Ok(Cmd::Call(call)) => self.pending.push_back(call),
            Ok(Cmd::Wake) | Err(RecvTimeoutError::Timeout) => {}
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
        let mut watching = false;
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
                    Ok(Cmd::Call(call)) => self.pending.push_back(call),
                    Ok(Cmd::Wake) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return End::Stopped,
                }
            }
            // The remote polls its runtime for a watch: only while a local
            // watcher wants machines.
            let want_watch = (self.inner.hooks.watching)();
            if want_watch != watching {
                if let Err(why) = self.watch(link, want_watch) {
                    return End::Lost(why);
                }
                watching = want_watch;
            }
            if let Some(call) = self.pending.pop_front() {
                let answer = link.conn.call_within(&call.method, call.params, t.call);
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
                Ok(Some(n)) if n.method == "instances.snapshot" => self.store(n.params),
                Ok(Some(n)) if n.method == "closing" => {
                    let reason = n.params.get("reason").and_then(Value::as_str).unwrap_or("closing");
                    return End::Lost(format!("{name}: its devsandbox serve is exiting ({reason})"));
                }
                Ok(_) => {}
                Err(e) => return End::Lost(format!("{name}: {e:#}")),
            }
        }
    }

    /// `instances.watch` (`on`) or `.unwatch` each root. A root the remote
    /// refuses (gone since `machine add`) is the machine's `error`, the
    /// others still watched; a dead connection is lost.
    fn watch(&self, link: &mut Link, on: bool) -> Result<(), String> {
        let method = if on { "instances.watch" } else { "instances.unwatch" };
        let mut refused = Vec::new();
        for root in &self.entry.roots {
            let r = link
                .conn
                .call_within(method, json!({ "dir": root }), self.inner.timing.call)
                .map_err(|e| format!("{}: {e:#}", self.entry.name))?
                .ok_or_else(|| format!("{}: `{method}` timed out", self.entry.name))?;
            if let Some(e) = r.error {
                refused.push(format!("{}: {}", root.display(), e.message));
            }
        }
        let error = (!refused.is_empty()).then(|| refused.join("; "));
        self.set(LinkState::Connected, error);
        Ok(())
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

    /// A pushed snapshot of one of the machine's roots: its rows now.
    fn store(&self, mut params: Value) {
        let Some(dir) = params.get("dir").and_then(Value::as_str).map(PathBuf::from) else { return };
        if !self.entry.roots.contains(&dir) {
            return;
        }
        // A reconnect's first push is disk-depth: keep the last statuses.
        self.inner.remotes.update(&self.entry.name, |m| {
            if let Some(prev) = m.rows.get(&dir) {
                snapshots::carry_remote_status(prev, &mut params);
            }
            m.rows.insert(dir, Arc::new(params));
            m.seen = Some(now_unix());
        });
        self.inner.rows_changed();
        (self.inner.hooks.changed)();
    }
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
    use crate::serve::machines::ssh::Duplex;
    use crate::serve::snapshots::Machine;

    /// A fake machine daemon: answers the hello and `subscribe`, never
    /// anything else.
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
        Hooks { watching: Box::new(|| false), changed: Box::new(|| {}) }
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
        let machines = Machines::spawn(registry, Arc::default(), counting_dial(&dials), fake_hold(&up, &runs), timing, no_hooks(), None, |_| {});
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
        let machines = Machines::spawn(registry, Arc::default(), counting_dial(&dials), fake_hold(&up, &runs), timing, no_hooks(), None, log);
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

    /// `machines.set` flipping `agent` alone: the reload puts it in
    /// `Remotes` (what the next federated snapshot carries) and fires
    /// `changed` (which re-publishes), without restarting the link.
    #[test]
    fn an_agent_flip_reaches_remotes_and_fires_changed() {
        let base = short_scratch("agent-flip");
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("m", "m"))).unwrap();
        let changed = Arc::new(AtomicUsize::new(0));
        let hooks = {
            let c = Arc::clone(&changed);
            Hooks { watching: Box::new(|| false), changed: Box::new(move || _ = c.fetch_add(1, Ordering::SeqCst)) }
        };
        let no_dial: Dial = Arc::new(|_| anyhow::bail!("no link here"));
        let (up, runs) = (Arc::new(AtomicBool::new(false)), Arc::default());
        let machines = Machines::spawn(registry.clone(), Arc::default(), no_dial, fake_hold(&up, &runs), opts(1, 0).machine_timing, hooks, None, |_| {});
        let h = machines.handle();
        assert!(!h.remotes().get("m").unwrap().agent);
        let ctl = Arc::clone(&h.inner.lock_links()["m"].ctl);
        let before = changed.load(Ordering::SeqCst);

        crate::serve::machines::update(&registry, |e| e[0].agent = true).unwrap();
        h.reload();
        assert!(h.remotes().get("m").unwrap().agent, "Remotes has the new agent");
        assert!(changed.load(Ordering::SeqCst) > before, "changed fired");
        assert!(Arc::ptr_eq(&ctl, &h.inner.lock_links()["m"].ctl), "the same link");
        assert!(h.entries()[0].agent, "the entry follows");

        crate::serve::machines::update(&registry, |e| e[0].agent = false).unwrap();
        h.reload();
        assert!(!h.remotes().get("m").unwrap().agent);
        assert!(Arc::ptr_eq(&ctl, &h.inner.lock_links()["m"].ctl));
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- offered machines and a hub -----------------------------------------

    static HUB_LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Offered worker `w` and ssh machine `s`, nobody watching; `w`'s
    /// fleet-link socket is "up" while the flag says so (a [`mute_peer`]),
    /// else its dial fails as `fleet::dialer::dial`'s does; `s`'s carrier
    /// always fails. Dials and carrier runs by name.
    #[allow(clippy::type_complexity)]
    fn offered_and_ssh(
        base: &Path,
        hub: Option<Hub>,
        log: fn(&str),
    ) -> (Machines, Arc<AtomicBool>, Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<String>>>) {
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| {
            e.push(Entry { agent: true, ..Entry::offered("w") });
            e.push(Entry::ssh("s", "me@s"));
        })
        .unwrap();
        let up = Arc::new(AtomicBool::new(false));
        let dials = Arc::new(Mutex::new(Vec::new()));
        let dial: Dial = {
            let (up, dials) = (Arc::clone(&up), Arc::clone(&dials));
            Arc::new(move |e: &Entry| {
                dials.lock().unwrap().push(e.name.clone());
                if e.is_offered() && up.load(Ordering::SeqCst) {
                    return Ok(mute_peer());
                }
                anyhow::bail!("its link isn't up (fleet-{}.sock: No such file or directory)", e.name)
            })
        };
        let runs = Arc::new(Mutex::new(Vec::new()));
        let hold = fake_hold(&Arc::new(AtomicBool::new(false)), &runs);
        let machines = Machines::spawn(registry, Arc::default(), dial, hold, opts(1, 0).machine_timing, no_hooks(), hub, log);
        (machines, up, dials, runs)
    }

    /// Every link is held with no watcher: an offered one retries an absent
    /// worker (logging that once) and connects when it comes, with no
    /// carrier of its own; an ssh one runs its carrier, never dialing its
    /// socket while that's down.
    #[test]
    fn every_link_is_held() {
        let base = short_scratch("held");
        let (machines, up, dials, runs) = offered_and_ssh(&base, None, |l| HUB_LOGGED.lock().unwrap().push(l.into()));
        let h = machines.handle();
        wait_for("the offered link retrying", Duration::from_secs(5), || ran(&dials, "w") >= 3);
        assert_eq!(h.remotes().get("w").unwrap().state, LinkState::Unreachable);
        let logged: Vec<_> = HUB_LOGGED.lock().unwrap().iter().filter(|l| l.starts_with("machine w: ")).cloned().collect();
        assert_eq!(logged.len(), 1, "one line for one way of failing: {logged:?}");
        assert!(logged[0].contains("its link isn't up"), "{}", logged[0]);

        up.store(true, Ordering::SeqCst);
        wait_for("the offered link up", Duration::from_secs(5), || h.remotes().get("w").unwrap().state == LinkState::Connected);
        assert_eq!(ran(&runs, "w"), 0, "no carrier for an offered machine");
        assert!(ran(&runs, "s") >= 1, "the ssh machine's carrier runs");
        assert_eq!(ran(&dials, "s"), 0, "nothing dialed over a down carrier");
        assert_eq!(h.remotes().get("s").unwrap().state, LinkState::Unreachable);
        assert!(!base.join("remotes.json").exists(), "only a hub saves");
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A hub restores `remotes.json` (unreachable until the link connects,
    /// machines no longer registered dropped) and saves it back at the stop.
    #[test]
    fn a_hub_restores_and_saves_remotes() {
        let base = short_scratch("hub-save");
        let path = base.join("remotes.json");
        let rows = json!({"dir": "/r", "instances": [{"name": "web-1"}]});
        let row = |name: &str| Machine {
            name: name.into(),
            rows: [(PathBuf::from("/r"), Arc::new(rows.clone()))].into(),
            seen: Some(1_761_566_400),
            ..Machine::default()
        };
        saved::save(&path, &saved::of(&[row("s"), row("gone")], 0)).unwrap();
        let hub = Hub { remotes: path.clone() };
        let (machines, up, _, runs) = offered_and_ssh(&base, Some(hub), |_| {});
        let h = machines.handle();
        assert!(h.remotes().get("gone").is_none(), "not registered: dropped");
        let s = h.remotes().get("s").unwrap();
        // `s` has no `/r` registered: its rows go with the sync, the machine stays.
        assert!(s.rows.is_empty() && s.restored);
        wait_for("s's carrier failed", Duration::from_secs(5), || ran(&runs, "s") >= 1);
        wait_for("s unreachable", Duration::from_secs(5), || h.remotes().get("s").unwrap().error.is_some());
        let s = h.remotes().get("s").unwrap();
        assert_eq!(s.state, LinkState::Unreachable, "restored: unreachable");
        assert!(s.error.as_deref().unwrap().ends_with("(rows from Oct 27 12:00 UTC)"), "{:?}", s.error);

        up.store(true, Ordering::SeqCst);
        wait_for("w connected", Duration::from_secs(5), || h.remotes().get("w").unwrap().state == LinkState::Connected);
        drop(machines);
        let saved = saved::load(&path).unwrap();
        assert_eq!(saved.machines.keys().collect::<Vec<_>>(), ["w"], "gone dropped; s has nothing left to save");
        assert_eq!(saved.machines["w"].version.as_deref(), Some("0.6.0"), "w's hello");
        let _ = std::fs::remove_dir_all(&base);
    }
}
