//! The held link per machine (docs/architecture/daemon/machines.md, *Held
//! links: `Machines`*): one `serve-machine-<name>` thread per registered
//! machine, owned by the [`Machines`] service the daemon runs for its whole
//! life.
//!
//! A link connects only while wanted: while some local connection
//! watches with `machines: true` ([`Hooks::watching`]), a forwarded call
//! is queued, or a [`Want`] holds the machine (a forwarded verb), and for
//! [`Timing::linger`] after. Connected, it subscribes to
//! `instances`, watches each of the machine's roots while a local watcher
//! wants machines, and stores every pushed `instances.snapshot` in the
//! shared [`Remotes`] ([`Hooks::changed`] then re-publishes). A failed
//! connect or a lost link is `unreachable` with the reason, rows kept, and
//! retried with a doubling backoff while wanted.
//!
//! Forwarded calls reach the thread over its channel, one in flight
//! per machine, each with [`Timing::call`] to answer. A timed-out call drops
//! the link: its answer would come out of turn for the next call.
//!
//! A machine with `agent` on also gets a `serve-machine-<name>-agent`
//! thread: while the machine is wanted (the same want, linger included) it
//! runs the agent relay (`ssh::agent_argv` through the agent [`Dial`],
//! served by `agent::serve`), retried with the link's backoff. `reload`
//! flips `agent` on the live link (no reconnect); off kills the relay at
//! once, as does the want ending (within a tick) or a stop.
//!
//! Nothing here blocks a connection or the daemon's locks on the network:
//! the link thread does all the I/O, a caller only waits for its reply. A
//! stop (`machines.rm`, daemon exit) shuts the link's socket and the
//! relay's down, so even a handshake in progress ends at once, and joins
//! both threads.

use std::collections::{BTreeMap, VecDeque};
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Entry;
use super::agent;
use super::link::{self, Link};
use super::ssh::{Dial, Duplex, Peer};
use crate::serve::api::ApiError;
use crate::serve::endpoint::Stream;
use crate::serve::proto::Response;
use crate::serve::snapshots::{self, LinkState, Remotes};

/// The link's clocks; [`Timing::default`] in production, short in tests.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long a link stays up after the last want.
    pub linger: Duration,
    /// The first retry's delay; each failure doubles it, up to `backoff_cap`.
    pub backoff: Duration,
    pub backoff_cap: Duration,
    /// A forwarded call's wait for its answer.
    pub call: Duration,
    /// The hello's wait: ssh's connect and auth, the remote login shell and a
    /// lazy start of the remote daemon.
    pub handshake: Duration,
    /// How often a link thread looks at its queue and its want.
    pub tick: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            linger: Duration::from_secs(60),
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

/// The service, owned by the daemon's `run`. Dropping it stops and joins
/// every link (closing its connection, killing its ssh).
pub struct Machines {
    inner: Arc<Inner>,
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
    dial: Dial,
    /// Spawns a machine's agent relay (`ssh::ssh_agent_dial` in production).
    agent_dial: Dial,
    timing: Timing,
    hooks: Hooks,
    log: fn(&str),
    remotes: Arc<Remotes>,
    links: Mutex<BTreeMap<String, LinkThread>>,
    /// [`Want`]s per machine name. By name, not per link: a hold outlives a
    /// link restarted by a registry change.
    holds: Mutex<BTreeMap<String, usize>>,
    stopped: AtomicBool,
}

/// One machine's threads, as the service holds them.
struct LinkThread {
    entry: Entry,
    tx: Sender<Cmd>,
    ctl: Arc<Ctl>,
    thread: Option<JoinHandle<()>>,
    agent: Option<JoinHandle<()>>,
}

/// What a machine's link and agent threads share with the service: how it
/// stops them wherever they are blocked, the live `agent` flag, the want.
#[derive(Default)]
struct Ctl {
    stop: AtomicBool,
    /// The connection's socket while one is open, to shut it down.
    stream: Mutex<Option<Stream>>,
    /// The entry's `agent`, as of the last reload.
    agent: AtomicBool,
    /// The last moment the machine was wanted; both threads stamp it
    /// ([`Inner::wanted`]).
    last_want: Mutex<Option<Instant>>,
    relay: Mutex<Relay>,
    /// Wakes the agent thread and [`Want::agent_ready`] waiters.
    relay_cv: Condvar,
}

/// The agent relay while one runs.
#[derive(Default)]
struct Relay {
    /// Its handshake is through: the far end's socket is bound.
    ready: bool,
    /// Its socket (to shut down) and ssh (to kill): [`Ctl::kill_relay`].
    kill: Option<(Stream, Peer)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Ctl {
    fn new(entry: &Entry) -> Self {
        let ctl = Ctl::default();
        ctl.agent.store(entry.agent, Ordering::SeqCst);
        ctl
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn cut(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(s) = lock(&self.stream).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
        self.kill_relay();
    }

    fn hold(&self, stream: Option<Stream>) {
        *lock(&self.stream) = stream;
    }

    fn agent_on(&self) -> bool {
        self.agent.load(Ordering::SeqCst)
    }

    /// `reload` flipped `agent`: off kills the relay now.
    fn set_agent(&self, on: bool) {
        self.agent.store(on, Ordering::SeqCst);
        if on { self.wake_agent() } else { self.kill_relay() }
    }

    fn touch(&self) {
        *lock(&self.last_want) = Some(Instant::now());
    }

    /// End the running relay, if any: its socket shut (so `agent::serve`
    /// returns), its ssh killed.
    fn kill_relay(&self) {
        let taken = {
            let mut r = lock(&self.relay);
            r.ready = false;
            r.kill.take()
        };
        if let Some((stream, mut peer)) = taken {
            let _ = stream.shutdown(Shutdown::Both);
            peer.close();
        }
        self.relay_cv.notify_all();
    }

    /// The relay's handshake is through (unless it was killed meanwhile).
    fn set_ready(&self) {
        let mut r = lock(&self.relay);
        r.ready = r.kill.is_some();
        self.relay_cv.notify_all();
    }

    fn wake_agent(&self) {
        let _r = lock(&self.relay);
        self.relay_cv.notify_all();
    }

    /// Sleep up to `wait`, or until woken.
    fn wait_agent(&self, wait: Duration) {
        let r = lock(&self.relay);
        let _ = self.relay_cv.wait_timeout(r, wait);
    }

    /// Wait up to `wait` for the relay's handshake; `false` at once while
    /// `agent` is off.
    fn wait_ready(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut r = lock(&self.relay);
        loop {
            if r.ready {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if !self.agent_on() || self.stopped() || left.is_zero() {
                return false;
            }
            r = self.relay_cv.wait_timeout(r, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

enum Cmd {
    Call(Call),
    /// Look at the want again now (a watch came or went, a stop).
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
    /// Start the service: one link per entry of `registry` (none connects
    /// until wanted). `dial` opens a machine's transport (ssh, or a test
    /// daemon's socket), `agent_dial` its agent relay's.
    pub fn spawn(registry: PathBuf, dial: Dial, agent_dial: Dial, timing: Timing, hooks: Hooks, log: fn(&str)) -> Machines {
        let inner = Arc::new(Inner {
            registry,
            dial,
            agent_dial,
            timing,
            hooks,
            log,
            remotes: Arc::default(),
            links: Mutex::new(BTreeMap::new()),
            holds: Mutex::new(BTreeMap::new()),
            stopped: AtomicBool::new(false),
        });
        let machines = Machines { inner };
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
    }
}

/// Cut every link first, then join: they wind down in parallel.
fn stop_all(links: Vec<LinkThread>) {
    for l in &links {
        l.ctl.cut();
        let _ = l.tx.send(Cmd::Wake);
    }
    for mut l in links {
        for t in [l.thread.take(), l.agent.take()].into_iter().flatten() {
            let _ = t.join();
        }
    }
}

impl Inner {
    fn lock_links(&self) -> MutexGuard<'_, BTreeMap<String, LinkThread>> {
        lock(&self.links)
    }

    /// Whether machine `name` is wanted now (`busy`: the link has calls
    /// queued; a machines watcher; a [`Want`]) or was within the linger.
    /// Stamps `ctl`'s last want, which the link and agent threads share.
    fn wanted(&self, name: &str, ctl: &Ctl, busy: bool) -> bool {
        let now = Instant::now();
        let held = lock(&self.holds).get(name).is_some_and(|n| *n > 0);
        let fresh = busy || held || (self.hooks.watching)();
        let mut last = lock(&ctl.last_want);
        if fresh {
            *last = Some(now);
        }
        last.is_some_and(|t| now < t + self.timing.linger)
    }
}

/// The same link: everything but `agent`, which flips on the live one.
fn same_link(a: &Entry, b: &Entry) -> bool {
    Entry { agent: b.agent, ..a.clone() } == *b
}

impl Handle {
    /// Re-read the registry: start links for new machines, stop (and drop
    /// the rows of) removed ones, restart changed ones, except for a change
    /// of `agent` alone, which flips on the live link (off kills its relay
    /// now). `machines.add`, `.set` and `.rm` call it; a registry that
    /// doesn't load is logged and changes nothing.
    pub fn reload(&self) {
        let inner = &self.inner;
        let entries = match super::load(&inner.registry) {
            Ok(entries) => entries,
            Err(e) => {
                (inner.log)(&format!("machines: {e:#}"));
                return;
            }
        };
        let mut flipped = Vec::new();
        let gone: Vec<LinkThread> = {
            let mut links = inner.lock_links();
            for (name, l) in links.iter_mut() {
                let Some(e) = entries.iter().find(|e| e.name == *name) else { continue };
                if e.agent != l.entry.agent && same_link(e, &l.entry) {
                    l.entry.agent = e.agent;
                    flipped.push((Arc::clone(&l.ctl), e.agent));
                }
            }
            let stale: Vec<String> = links
                .iter()
                .filter(|(name, l)| !entries.iter().any(|e| e.name == **name && *e == l.entry))
                .map(|(name, _)| name.clone())
                .collect();
            stale.iter().filter_map(|name| links.remove(name)).collect()
        };
        // Outside the lock: killing a relay reaps its ssh.
        for (ctl, on) in flipped {
            ctl.set_agent(on);
        }
        // Joined outside the lock (a forwarded call looks links up), and
        // before a changed entry's new thread starts writing its state.
        stop_all(gone);
        let mut links = inner.lock_links();
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        // Before the new threads run: they record their state there.
        inner.remotes.sync(&entries);
        for e in &entries {
            if !links.contains_key(&e.name) {
                let link = spawn_link(e.clone(), Arc::clone(inner));
                links.insert(e.name.clone(), link);
            }
        }
        drop(links);
        (inner.hooks.changed)();
    }

    /// Every link looks at its want now (a `machines: true` watch came).
    pub fn wake(&self) {
        for l in self.inner.lock_links().values() {
            let _ = l.tx.send(Cmd::Wake);
        }
    }

    /// Hold machine `machine` wanted until the guard drops (a forwarded verb
    /// for its run, a connection for its life); `None` when it isn't
    /// registered. The link and, with `agent` on, its relay start now.
    pub fn want(&self, machine: &str) -> Option<Want> {
        let (tx, ctl) = {
            let links = self.inner.lock_links();
            let l = links.get(machine)?;
            *lock(&self.inner.holds).entry(machine.to_string()).or_default() += 1;
            (l.tx.clone(), Arc::clone(&l.ctl))
        };
        let _ = tx.send(Cmd::Wake);
        ctl.wake_agent();
        Some(Want { inner: Arc::clone(&self.inner), machine: machine.to_string() })
    }

    /// The registered machines as of the last reload, sorted by name: what
    /// a forwarded verb dials and a federated call fans out to, without a
    /// registry read per request.
    pub fn entries(&self) -> Vec<Entry> {
        self.inner.lock_links().values().map(|l| l.entry.clone()).collect()
    }

    /// The machines' states and rows.
    pub fn remotes(&self) -> &Arc<Remotes> {
        &self.inner.remotes
    }

    /// Call `method` on machine `machine`'s daemon over its link, connecting
    /// it if needed: the remote's response as is (its error code kept), or
    /// `not-found` for a machine that isn't registered.
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
        let bound = t.handshake + t.call + t.backoff_cap + Duration::from_secs(5);
        rx.recv_timeout(bound).map_err(|e| match e {
            RecvTimeoutError::Timeout => ApiError::internal(anyhow::anyhow!("{machine}: no answer")),
            RecvTimeoutError::Disconnected => stopping(),
        })
    }
}

/// A hold on a machine ([`Handle::want`]): wanted while it lives, the
/// linger counted from its drop.
pub struct Want {
    inner: Arc<Inner>,
    machine: String,
}

impl Want {
    /// Wait up to `wait` for the machine's agent relay to be up (its
    /// handshake through, so its socket there exists); `false` at once while
    /// the machine's `agent` is off.
    pub fn agent_ready(&self, wait: Duration) -> bool {
        let ctl = self.inner.lock_links().get(&self.machine).map(|l| Arc::clone(&l.ctl));
        ctl.is_some_and(|ctl| ctl.wait_ready(wait))
    }
}

impl Drop for Want {
    fn drop(&mut self) {
        {
            let mut holds = lock(&self.inner.holds);
            if let Some(n) = holds.get_mut(&self.machine) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    holds.remove(&self.machine);
                }
            }
        }
        let link = self.inner.lock_links().get(&self.machine).map(|l| (l.tx.clone(), Arc::clone(&l.ctl)));
        if let Some((tx, ctl)) = link {
            ctl.touch();
            let _ = tx.send(Cmd::Wake);
            ctl.wake_agent();
        }
    }
}

fn spawn_link(entry: Entry, inner: Arc<Inner>) -> LinkThread {
    let (tx, rx) = mpsc::channel();
    let ctl = Arc::new(Ctl::new(&entry));
    let log = inner.log;
    let thread = {
        let (entry, ctl, inner) = (entry.clone(), Arc::clone(&ctl), Arc::clone(&inner));
        std::thread::Builder::new()
            .name(format!("serve-machine-{}", entry.name))
            .spawn(move || LinkLoop::new(entry, &inner, &ctl).run(&rx))
    };
    let thread = thread.map_err(|e| log(&format!("cannot start machine {}'s link: {e}", entry.name))).ok();
    let agent = {
        let (entry, ctl) = (entry.clone(), Arc::clone(&ctl));
        std::thread::Builder::new()
            .name(format!("serve-machine-{}-agent", entry.name))
            .spawn(move || agent_loop(&entry, &inner, &ctl))
    };
    let agent = agent.map_err(|e| log(&format!("cannot start machine {}'s agent thread: {e}", entry.name))).ok();
    LinkThread { entry, tx, ctl, thread, agent }
}

/// The delay before the next attempt after `failures` failures in a row: 1,
/// 2, 4 … units, capped; counts this one.
fn backoff(t: &Timing, failures: &mut u32) -> Duration {
    let delay = t.backoff.saturating_mul(1u32 << (*failures).min(16)).min(t.backoff_cap);
    *failures = failures.saturating_add(1);
    delay
}

/// A machine's agent thread: runs the relay while `agent` is on and the
/// machine is wanted, waits (woken by a flip, a want, a stop) otherwise.
/// A failed relay is logged and retried after the link's backoff; one
/// ended on purpose (off, unwanted, stopped) isn't.
fn agent_loop(entry: &Entry, inner: &Inner, ctl: &Arc<Ctl>) {
    let tick = inner.timing.tick;
    let mut failures = 0;
    let mut retry_at: Option<Instant> = None;
    while !ctl.stopped() {
        let up = ctl.agent_on() && inner.wanted(&entry.name, ctl, false);
        if !up || retry_at.is_some_and(|t| Instant::now() < t) {
            ctl.wait_agent(tick);
            continue;
        }
        retry_at = None;
        let (handshaken, failed) = run_relay(entry, inner, ctl);
        if handshaken {
            failures = 0;
        }
        if let Some(why) = failed {
            (inner.log)(&format!("machine {}: agent relay: {why}", entry.name));
            retry_at = Some(Instant::now() + backoff(&inner.timing, &mut failures));
        }
    }
}

/// One run of the relay, until it ends or is ended: whether its handshake
/// went through, and why it failed (`None`: ended on purpose).
fn run_relay(entry: &Entry, inner: &Inner, ctl: &Arc<Ctl>) -> (bool, Option<String>) {
    let Duplex { stream, peer } = match (inner.agent_dial)(entry) {
        Ok(duplex) => duplex,
        Err(e) => return (false, Some(format!("{e:#}"))),
    };
    let (reader, writer) = match stream.try_clone().and_then(|r| Ok((r, stream.try_clone()?))) {
        Ok(halves) => halves,
        Err(e) => return (false, Some(format!("cannot clone its socket: {e}"))),
    };
    {
        let mut r = lock(&ctl.relay);
        r.ready = false;
        r.kill = Some((stream, peer));
    }
    let unwanted = || ctl.stopped() || !ctl.agent_on() || !inner.wanted(&entry.name, ctl, false);
    let handshaken = AtomicBool::new(false);
    let ended = std::thread::scope(|s| {
        let served = std::thread::Builder::new().name(format!("serve-machine-{}-relay", entry.name)).spawn_scoped(s, || {
            let (allowed, ready, dead) = (Arc::clone(ctl), Arc::clone(ctl), Arc::clone(ctl));
            let end = agent::serve(
                reader,
                writer,
                move || allowed.agent_on(),
                crate::devsbd::bridge::host_agent,
                || {
                    handshaken.store(true, Ordering::SeqCst);
                    ready.set_ready();
                },
                move || dead.kill_relay(),
            );
            ctl.wake_agent();
            end
        });
        let served = match served {
            Ok(served) => served,
            Err(e) => return Err(format!("cannot start its thread: {e}")),
        };
        // The want ending kills it within a tick; a flip off or a stop
        // already did.
        while !served.is_finished() {
            if unwanted() {
                ctl.kill_relay();
                break;
            }
            ctl.wait_agent(inner.timing.tick);
        }
        served.join().map_err(|_| "its thread panicked".to_string())
    });
    let left = {
        let mut r = lock(&ctl.relay);
        r.ready = false;
        r.kill.take()
    };
    ctl.relay_cv.notify_all();
    let handshaken = handshaken.load(Ordering::SeqCst);
    if unwanted() {
        return (handshaken, None);
    }
    let why = match ended {
        Ok(agent::End::Handshake(why)) => format!("handshake failed: {why}"),
        Ok(agent::End::Dead) => format!("silent for {}s", crate::devsbd::mux::KEEPALIVE_TIMEOUT.as_secs()),
        Ok(agent::End::Closed) => "closed".to_string(),
        Err(why) => why,
    };
    // Still ours: it ended by itself, and ssh's last words may say why.
    let tail = left.map(|(_, peer)| peer.stderr_tail(STDERR_WAIT)).unwrap_or_default();
    (handshaken, Some(if tail.is_empty() { why } else { format!("{why}: {tail}") }))
}

/// How long a failed relay's log line waits for ssh's stderr to end.
const STDERR_WAIT: Duration = Duration::from_secs(2);

/// Why a connected link ended.
enum End {
    /// Not wanted for the linger: closed on purpose.
    Idle,
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
}

impl<'a> LinkLoop<'a> {
    fn new(entry: Entry, inner: &'a Inner, ctl: &'a Ctl) -> Self {
        Self { entry, inner, ctl, pending: VecDeque::new(), failures: 0 }
    }

    fn run(&mut self, rx: &Receiver<Cmd>) {
        let tick = self.inner.timing.tick;
        while !self.ctl.stopped() {
            if !self.wanted() {
                self.set(LinkState::Idle, None);
                if !self.recv(rx, tick) {
                    break;
                }
                continue;
            }
            self.set(LinkState::Connecting, None);
            let failed = match self.connect() {
                Ok(mut link) => {
                    self.failures = 0;
                    if let Some(noise) = &link.noise {
                        (self.inner.log)(&format!("machine {}: login shell printed: {noise}", self.entry.name));
                    }
                    let remote = link.remote().version.clone();
                    self.inner.remotes.update(&self.entry.name, |m| m.version = Some(remote));
                    self.set(LinkState::Connected, None);
                    let end = self.serve(&mut link, rx);
                    self.ctl.hold(None);
                    drop(link);
                    match end {
                        End::Idle => continue,
                        End::Stopped => break,
                        End::Lost(why) => why,
                    }
                }
                Err(why) => why,
            };
            if self.ctl.stopped() {
                break;
            }
            (self.inner.log)(&format!("machine {}: {failed}", self.entry.name));
            self.set(LinkState::Unreachable, Some(failed.clone()));
            // A queued call doesn't wait out the backoff for an answer.
            for call in self.pending.drain(..) {
                let _ = call.reply.send(Response::err(None, "failed", format!("{}: unreachable: {failed}", self.entry.name)));
            }
            let delay = self.backoff();
            let until = Instant::now() + delay;
            // A call arriving meanwhile retries at once.
            while self.pending.is_empty() && !self.ctl.stopped() {
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

    /// Wanted now (a machines watcher, a queued call, a [`Want`]), or within
    /// the linger.
    fn wanted(&self) -> bool {
        self.inner.wanted(&self.entry.name, self.ctl, !self.pending.is_empty())
    }

    fn set(&self, state: LinkState, error: Option<String>) {
        let mut moved = false;
        self.inner.remotes.update(&self.entry.name, |m| {
            moved = m.state != state || m.error != error;
            m.state = state;
            m.error = error;
        });
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
        link::handshake(&self.entry.name, duplex, self.inner.timing.handshake).map_err(|e| e.to_string())
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
            loop {
                match rx.try_recv() {
                    Ok(Cmd::Call(call)) => self.pending.push_back(call),
                    Ok(Cmd::Wake) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return End::Stopped,
                }
            }
            if !self.wanted() {
                return End::Idle;
            }
            // The remote polls its runtime for a watch: only while a local
            // watcher wants machines, not for a lingering call link.
            let want_watch = (self.inner.hooks.watching)();
            if want_watch != watching {
                if let Err(why) = self.watch(link, want_watch) {
                    return End::Lost(why);
                }
                watching = want_watch;
            }
            if let Some(call) = self.pending.pop_front() {
                let answer = link.conn.call_within(&call.method, call.params, t.call);
                self.ctl.touch();
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
        });
        (self.inner.hooks.changed)();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::serve::client::{self, Conn};
    use crate::serve::daemon::tests::{fake_join, opts, scratch, spawn_daemon};
    use crate::serve::daemon::{Exit, Options};
    use crate::serve::endpoint;
    use crate::serve::machines::ssh::{self, Duplex};
    use crate::serve::proto::{Notification, Version};
    use crate::serve::snapshots::Join;

    fn connect(dir: &Path) -> Conn {
        let v = Version { semver: "0.6.0".into(), build: 1 };
        client::connect_with(dir, "test", &v, client::START_TIMEOUT, &|_, _| Ok(())).unwrap()
    }

    /// The machine's join: one instance `instance` made in the watched root,
    /// and a sandbox `web` read from it.
    fn remote_join(instance: &'static str) -> Join {
        Arc::new(move |dir, view| {
            let mut list = fake_join()(dir, view);
            let mut row = crate::serve::snapshots::tests::row(instance, true, None);
            row.root = Some(dir.display().to_string());
            list.snapshot.instances.push(row);
            let cfg = crate::config::Config::parse("[sandbox.web]\nimage = \"alpine\"\n").unwrap();
            list.snapshot.sandboxes = crate::snapshot::sandbox_rows(dir, &cfg).0;
            list
        })
    }

    fn remote_opts(base: &Path, instance: &'static str, idle_ms: u64) -> Options {
        let mut o = opts(1, idle_ms);
        o.state = Some(base.join("remote-state.toml"));
        o.home = Some(base.join("remote-home"));
        o.join = Some(remote_join(instance));
        o
    }

    /// The next `instances.snapshot` that `want` accepts, within 10 s.
    fn snapshot_where(conn: &mut Conn, want: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let n: Notification = conn.next_notification(left).unwrap().expect("no such snapshot in time");
            if n.method == "instances.snapshot" && want(&n.params) {
                return n.params;
            }
        }
    }

    fn state(s: &Value) -> &str {
        s["machines"][0]["state"].as_str().unwrap_or("")
    }

    fn instance_names(s: &Value) -> Vec<&str> {
        s["instances"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect()
    }

    /// A local watcher with `machines: true` gets the machine's rows tagged;
    /// the machine going away leaves them, unreachable; coming back refreshes
    /// them; `instances.list` goes over the link; unwanted, it closes.
    #[test]
    fn a_machines_rows_follow_its_link() {
        let base = scratch("link-fed");
        let (remote_dir, local_dir) = (base.join("remote"), base.join("local"));
        let (remote_root, local_root) = (base.join("remote-root"), base.join("local-root"));
        std::fs::create_dir_all(&remote_root).unwrap();
        std::fs::create_dir_all(&local_root).unwrap();
        let registry = base.join("machines.toml");
        let entry = Entry { name: "box".into(), target: "me@box".into(), roots: vec![remote_root.clone()], command: None, agent: false };
        crate::serve::machines::update(&registry, |e| e.push(entry)).unwrap();

        let remote = spawn_daemon(&remote_dir, remote_opts(&base, "web-1", 60_000));
        drop(connect(&remote_dir));
        let dials = Arc::new(AtomicUsize::new(0));
        let mut local_opts = opts(1, 300);
        local_opts.machines = Some(registry);
        local_opts.dial = Some({
            let (remote_dir, dials) = (remote_dir.clone(), Arc::clone(&dials));
            Arc::new(move |_: &Entry| {
                dials.fetch_add(1, Ordering::SeqCst);
                ssh::test_support::relayed(endpoint::connect(&remote_dir)?)
            })
        });
        let local = spawn_daemon(&local_dir, local_opts);
        let mut c = connect(&local_dir);
        // Registered, not wanted: no dial.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(dials.load(Ordering::SeqCst), 0);
        let rows = c.call("machines.list", Value::Null).unwrap().result.unwrap();
        assert_eq!(rows[0]["state"], "idle");

        c.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        let r = c.call("instances.watch", json!({"dir": local_root, "machines": true})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        // The first push at once, with the machine (not connected yet).
        let first = snapshot_where(&mut c, |_| true);
        assert_eq!(first["machines"][0]["name"], "box");
        assert_ne!(state(&first), "connected");
        assert!(instance_names(&first).is_empty());

        let s = snapshot_where(&mut c, |s| state(s) == "connected" && !instance_names(s).is_empty());
        let remote_text = remote_root.to_str().unwrap();
        assert_eq!(instance_names(&s), ["web-1"]);
        assert_eq!((&s["instances"][0]["machine"], &s["instances"][0]["root"]), (&json!("box"), &json!(remote_text)));
        assert_eq!((&s["sandboxes"][0]["name"], &s["sandboxes"][0]["machine"]), (&json!("web"), &json!("box")));
        assert_eq!(s["sandboxes"][0]["root"], remote_text);
        assert_eq!(s["dir"], local_root.to_str().unwrap());
        assert_eq!(
            s["machines"],
            json!([{"name": "box", "target": "me@box", "state": "connected", "error": null, "version": "0.6.0", "runtime": "Disk", "command": null, "agent": false}])
        );
        crate::serve::dts::assert_matches("InstancesSnapshot", &s);
        crate::serve::dts::assert_matches("SnapshotMachine", &s["machines"][0]);
        let rows = c.call("machines.list", Value::Null).unwrap().result.unwrap();
        assert_eq!((&rows[0]["state"], &rows[0]["version"]), (&json!("connected"), &json!("0.6.0")));

        // A watch without `machines` gets today's params exactly.
        let mut plain = connect(&local_dir);
        plain.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        plain.call("instances.watch", json!({"dir": local_root})).unwrap();
        let p = snapshot_where(&mut plain, |_| true);
        let disk = fake_join()(&local_root, &crate::snapshot::RuntimeView::disk());
        assert_eq!(p, *crate::serve::snapshots::params(&local_root, &disk).unwrap());

        // `instances.list` with `machine`: the machine's answer, its code kept.
        let gone = base.join("only-a-path");
        let r = c.call("instances.list", json!({"dir": gone, "machine": "box"})).unwrap();
        let e = r.error.unwrap();
        assert_eq!((e.code.as_str(), e.message.starts_with("no directory")), ("not-found", true), "{e:?}");
        let e = c.call("instances.list", json!({"dir": gone, "machine": "nope"})).unwrap().error.unwrap();
        assert_eq!((e.code.as_str(), e.message.as_str()), ("not-found", "no machine named `nope`"));

        // The machine stops: unreachable, its rows kept.
        let r = connect(&remote_dir).call("shutdown", json!({})).unwrap();
        assert!(r.error.is_none());
        assert_eq!(remote.join().unwrap().unwrap(), Exit::Shutdown);
        let s = snapshot_where(&mut c, |s| state(s) == "unreachable");
        assert_eq!(instance_names(&s), ["web-1"]);
        assert!(s["machines"][0]["error"].as_str().is_some_and(|e| e.starts_with("box: ")), "{s}");

        // It comes back: connected, the rows refreshed.
        let remote = spawn_daemon(&remote_dir, remote_opts(&base, "web-2", 1500));
        let s = snapshot_where(&mut c, |s| state(s) == "connected" && instance_names(s) == ["web-2"]);
        assert_eq!(s["machines"][0]["error"], Value::Null);

        // Nobody wants it: after the linger the link closes, so the machine
        // idles out with this host's daemon still up.
        c.call("instances.unwatch", json!({"dir": local_root})).unwrap();
        assert_eq!(remote.join().unwrap().unwrap(), Exit::Idle);
        let rows = c.call("machines.list", Value::Null).unwrap().result.unwrap();
        assert_eq!(rows[0]["state"], "idle");
        assert!(!local.is_finished());
        drop((c, plain));
        assert_eq!(local.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A fake machine daemon: answers the hello and `subscribe`, never
    /// anything else.
    fn mute_peer() -> Duplex {
        let (ours, theirs) = UnixStream::pair().unwrap();
        crate::serve::relay::write_sentinel(&mut &theirs, true).unwrap();
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

    /// A call the machine doesn't answer in time answers `internal`
    /// and drops the link (a late answer would desync the ids); the next
    /// call dials again. An unregistered machine is `not-found`.
    #[test]
    fn a_timed_out_call_drops_the_link() {
        let base = scratch("link-timeout");
        let registry = base.join("machines.toml");
        let entry = Entry { name: "m".into(), target: "m".into(), roots: vec![], command: None, agent: false };
        crate::serve::machines::update(&registry, |e| e.push(entry)).unwrap();
        let dials = Arc::new(AtomicUsize::new(0));
        let dial: Dial = {
            let dials = Arc::clone(&dials);
            Arc::new(move |_| {
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(mute_peer())
            })
        };
        let timing = Timing { call: Duration::from_millis(200), ..opts(1, 0).machine_timing };
        let hooks = Hooks { watching: Box::new(|| false), changed: Box::new(|| {}) };
        let no_agent: Dial = Arc::new(|_| anyhow::bail!("no agent relay here"));
        let machines = Machines::spawn(registry, dial, no_agent, timing, hooks, |_| {});
        let h = machines.handle();
        for round in 1..=2 {
            let r = h.forward("m", "instances.list", json!({"dir": "/x"})).unwrap();
            let e = r.error.unwrap();
            assert_eq!((e.code.as_str(), e.message.as_str()), ("internal", "m: timed out"));
            assert_eq!(dials.load(Ordering::SeqCst), round);
        }
        assert_eq!(h.forward("n", "instances.list", json!({})).unwrap_err().code, "not-found");
        let started = Instant::now();
        drop(machines);
        assert!(started.elapsed() < Duration::from_secs(2), "the stop waited");
        assert!(h.forward("m", "instances.list", json!({})).is_err(), "stopped");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `machines.set` flipping `agent` alone: the reload puts it in
    /// `Remotes` (what the next federated snapshot carries) and fires
    /// `changed` (which re-publishes), without restarting the link.
    #[test]
    fn an_agent_flip_reaches_remotes_and_fires_changed() {
        let base = scratch("agent-flip");
        let registry = base.join("machines.toml");
        let entry = Entry { name: "m".into(), target: "m".into(), roots: vec![], command: None, agent: false };
        crate::serve::machines::update(&registry, |e| e.push(entry)).unwrap();
        let changed = Arc::new(AtomicUsize::new(0));
        let hooks = {
            let c = Arc::clone(&changed);
            Hooks { watching: Box::new(|| false), changed: Box::new(move || _ = c.fetch_add(1, Ordering::SeqCst)) }
        };
        let no_dial: Dial = Arc::new(|_| anyhow::bail!("no link here"));
        let no_agent: Dial = Arc::new(|_| anyhow::bail!("no agent relay here"));
        let machines = Machines::spawn(registry.clone(), no_dial, no_agent, opts(1, 0).machine_timing, hooks, |_| {});
        let h = machines.handle();
        let before = changed.load(Ordering::SeqCst);
        assert!(!h.remotes().get("m").unwrap().agent);
        let ctl = Arc::clone(&h.inner.lock_links()["m"].ctl);

        crate::serve::machines::update(&registry, |e| e[0].agent = true).unwrap();
        h.reload();
        assert!(h.remotes().get("m").unwrap().agent, "Remotes has the new agent");
        assert_eq!(changed.load(Ordering::SeqCst), before + 1, "changed fired");
        assert!(Arc::ptr_eq(&ctl, &h.inner.lock_links()["m"].ctl), "the same link");
        assert!(ctl.agent_on(), "flipped on the live link");

        crate::serve::machines::update(&registry, |e| e[0].agent = false).unwrap();
        h.reload();
        assert!(!h.remotes().get("m").unwrap().agent);
        assert_eq!(changed.load(Ordering::SeqCst), before + 2);
        drop(machines);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- the agent relay -----------------------------------------------------

    const CLIENT: &str = "machine:test";

    /// A short scratch dir (socket paths stay well under 104 bytes).
    fn short_scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-ms-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An agent dial running the relay's far end (`agent_relay::run`) in a
    /// thread, its socket in `dir`; counts the spawns. Shutting the
    /// `Duplex` down ends it, which unlinks the socket.
    fn in_process_relay(dir: &Path) -> (Dial, Arc<AtomicUsize>) {
        let spawns = Arc::new(AtomicUsize::new(0));
        let dial: Dial = {
            let (dir, spawns) = (dir.to_path_buf(), Arc::clone(&spawns));
            Arc::new(move |_| {
                spawns.fetch_add(1, Ordering::SeqCst);
                let (ours, theirs) = UnixStream::pair()?;
                let (r, w) = (theirs.try_clone()?, theirs);
                let dir = dir.clone();
                std::thread::spawn(move || crate::commands::agent_relay::run(r, w, &dir, CLIENT));
                Ok(Duplex::socket(ours))
            })
        };
        (dial, spawns)
    }

    fn wait_for(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + within;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    struct Agented {
        base: PathBuf,
        registry: PathBuf,
        machines: Machines,
        watching: Arc<AtomicBool>,
        dials: Arc<AtomicUsize>,
        spawns: Arc<AtomicUsize>,
        socket: PathBuf,
        timing: Timing,
    }

    /// Machine `m` (`agent` as given) whose link is a [`mute_peer`] and
    /// whose agent relay runs in-process; a watcher flag for the want.
    fn agented(tag: &str, agent: bool) -> Agented {
        let base = short_scratch(tag);
        let registry = base.join("machines.toml");
        let entry = Entry { name: "m".into(), target: "m".into(), roots: vec![], command: None, agent };
        crate::serve::machines::update(&registry, |e| e.push(entry)).unwrap();
        let dials = Arc::new(AtomicUsize::new(0));
        let dial: Dial = {
            let dials = Arc::clone(&dials);
            Arc::new(move |_| {
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(mute_peer())
            })
        };
        let (agent_dial, spawns) = in_process_relay(&base);
        let watching = Arc::new(AtomicBool::new(false));
        let hooks = {
            let w = Arc::clone(&watching);
            Hooks { watching: Box::new(move || w.load(Ordering::SeqCst)), changed: Box::new(|| {}) }
        };
        let timing = opts(1, 0).machine_timing;
        let machines = Machines::spawn(registry.clone(), dial, agent_dial, timing, hooks, |_| {});
        let socket = endpoint::agent_socket(&base, CLIENT);
        Agented { base, registry, machines, watching, dials, spawns, socket, timing }
    }

    impl Agented {
        fn set_agent(&self, on: bool) {
            crate::serve::machines::update(&self.registry, |e| e[0].agent = on).unwrap();
            self.machines.handle().reload();
        }

        fn state(&self) -> LinkState {
            self.machines.handle().remotes().get("m").unwrap().state
        }

        fn finish(self) {
            let started = Instant::now();
            drop(self.machines);
            assert!(started.elapsed() < Duration::from_secs(2), "the stop waited");
            wait_for("the relay's end after the stop", Duration::from_secs(2), || !self.socket.exists());
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// With `agent` on the relay runs while the machine is wanted; `set`
    /// off kills it at once without touching the link, on brings it back;
    /// the want ending kills it after the linger.
    #[test]
    fn the_agent_relay_follows_the_want_and_the_flag() {
        let a = agented("follow", true);
        let h = a.machines.handle();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(a.spawns.load(Ordering::SeqCst), 0, "not wanted: no relay");

        a.watching.store(true, Ordering::SeqCst);
        h.wake();
        wait_for("the relay up", Duration::from_secs(5), || a.socket.exists());
        wait_for("the link up", Duration::from_secs(5), || a.state() == LinkState::Connected);
        assert_eq!((a.spawns.load(Ordering::SeqCst), a.dials.load(Ordering::SeqCst)), (1, 1));

        // Off: killed promptly, the link left alone.
        a.set_agent(false);
        // The far end unlinks on its next accept poll (200 ms).
        wait_for("the relay killed", Duration::from_secs(1), || !a.socket.exists());
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(a.spawns.load(Ordering::SeqCst), 1, "no respawn while off");
        assert_eq!((a.dials.load(Ordering::SeqCst), a.state()), (1, LinkState::Connected), "the link wasn't restarted");
        let want = h.want("m").unwrap();
        let started = Instant::now();
        assert!(!want.agent_ready(Duration::from_secs(5)), "off: not ready");
        assert!(started.elapsed() < Duration::from_millis(100), "off answers at once");
        drop(want);

        // On again: a new relay, same link.
        a.set_agent(true);
        wait_for("the relay back", Duration::from_secs(5), || a.socket.exists());
        assert_eq!((a.spawns.load(Ordering::SeqCst), a.dials.load(Ordering::SeqCst)), (2, 1));

        // Nobody wants it: gone after the linger, the link idle.
        a.watching.store(false, Ordering::SeqCst);
        std::thread::sleep(a.timing.linger / 2);
        assert!(a.socket.exists(), "killed before the linger");
        wait_for("the relay gone after the linger", Duration::from_secs(2), || !a.socket.exists());
        wait_for("the link idle", Duration::from_secs(2), || a.state() == LinkState::Idle);
        assert_eq!(a.spawns.load(Ordering::SeqCst), 2);
        a.finish();
    }

    /// A [`Want`] alone keeps the machine wanted (link and relay), and says
    /// when the relay is up; its drop starts the linger.
    #[test]
    fn a_want_holds_the_machine_and_waits_for_the_relay() {
        let a = agented("want", true);
        let h = a.machines.handle();
        assert!(h.want("nope").is_none(), "not registered");
        let want = h.want("m").unwrap();
        assert!(want.agent_ready(Duration::from_secs(5)), "the relay came up");
        assert!(a.socket.exists(), "ready means its socket is there");
        wait_for("the link up", Duration::from_secs(5), || a.state() == LinkState::Connected);
        std::thread::sleep(a.timing.linger * 2);
        assert!(a.socket.exists() && a.state() == LinkState::Connected, "held past the linger");
        let second = h.want("m").unwrap();
        drop(want);
        std::thread::sleep(a.timing.linger * 2);
        assert!(a.socket.exists(), "still held by the second");
        drop(second);
        wait_for("the relay gone after the linger", Duration::from_secs(2), || !a.socket.exists());
        wait_for("the link idle", Duration::from_secs(2), || a.state() == LinkState::Idle);
        assert_eq!(a.spawns.load(Ordering::SeqCst), 1);

        // A stop with the relay up kills it too.
        let want = h.want("m").unwrap();
        assert!(want.agent_ready(Duration::from_secs(5)));
        a.finish();
        assert!(!want.agent_ready(Duration::ZERO), "stopped");
    }

    #[test]
    fn agent_off_never_spawns_a_relay() {
        let a = agented("off", false);
        a.watching.store(true, Ordering::SeqCst);
        a.machines.handle().wake();
        wait_for("the link up", Duration::from_secs(5), || a.state() == LinkState::Connected);
        let want = a.machines.handle().want("m").unwrap();
        assert!(!want.agent_ready(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(a.spawns.load(Ordering::SeqCst), 0);
        drop(want);
        a.finish();
    }

    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// A relay that can't spawn is logged and retried with the backoff,
    /// not in a tight loop.
    #[test]
    fn a_failing_relay_backs_off() {
        let base = short_scratch("fail");
        let registry = base.join("machines.toml");
        let entry = Entry { name: "fails".into(), target: "x".into(), roots: vec![], command: None, agent: true };
        crate::serve::machines::update(&registry, |e| e.push(entry)).unwrap();
        let spawns = Arc::new(AtomicUsize::new(0));
        let agent_dial: Dial = {
            let spawns = Arc::clone(&spawns);
            Arc::new(move |_| {
                spawns.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("ssh: Could not resolve hostname x")
            })
        };
        let dial: Dial = Arc::new(|_| Ok(mute_peer()));
        let hooks = Hooks { watching: Box::new(|| true), changed: Box::new(|| {}) };
        let timing = opts(1, 0).machine_timing;
        let machines = Machines::spawn(registry, dial, agent_dial, timing, hooks, |line| LOGGED.lock().unwrap().push(line.into()));
        std::thread::sleep(Duration::from_millis(600));
        drop(machines);
        // 50, 100, 200, 200 … ms apart: a handful in 600 ms, not hundreds.
        let n = spawns.load(Ordering::SeqCst);
        assert!((2..=8).contains(&n), "{n} spawns");
        let logged = LOGGED.lock().unwrap();
        let ours: Vec<_> = logged.iter().filter(|l| l.starts_with("machine fails: agent relay: ")).collect();
        assert_eq!(ours.len(), n, "{logged:?}");
        assert!(ours[0].ends_with("Could not resolve hostname x"), "{}", ours[0]);
        let _ = std::fs::remove_dir_all(&base);
    }
}
