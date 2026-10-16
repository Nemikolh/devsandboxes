//! The held link per machine (docs/architecture/daemon/machines.md, *Held
//! links: `Machines`*): one `serve-machine-<name>` thread per registered
//! machine, owned by the [`Machines`] service the daemon runs for its whole
//! life.
//!
//! A link connects only while wanted: while some local connection
//! watches with `machines: true` ([`Hooks::watching`]), or a forwarded call
//! is queued, and for [`Timing::linger`] after. Connected, it subscribes to
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
//! Nothing here blocks a connection or the daemon's locks on the network:
//! the link thread does all the I/O, a caller only waits for its reply. A
//! stop (`machines.rm`, daemon exit) shuts the link's socket down, so even a
//! handshake in progress ends at once, and joins the thread.

use std::collections::{BTreeMap, VecDeque};
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Entry;
use super::link::{self, Link};
use super::ssh::Dial;
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
    timing: Timing,
    hooks: Hooks,
    log: fn(&str),
    remotes: Arc<Remotes>,
    links: Mutex<BTreeMap<String, LinkThread>>,
    stopped: AtomicBool,
}

/// One machine's thread, as the service holds it.
struct LinkThread {
    entry: Entry,
    tx: Sender<Cmd>,
    ctl: Arc<Ctl>,
    thread: Option<JoinHandle<()>>,
}

/// How the service stops a link thread wherever it is blocked.
#[derive(Default)]
struct Ctl {
    stop: AtomicBool,
    /// The connection's socket while one is open, to shut it down.
    stream: Mutex<Option<Stream>>,
}

impl Ctl {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn cut(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(s) = self.stream.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn hold(&self, stream: Option<Stream>) {
        *self.stream.lock().unwrap_or_else(|e| e.into_inner()) = stream;
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
    /// daemon's socket).
    pub fn spawn(registry: PathBuf, dial: Dial, timing: Timing, hooks: Hooks, log: fn(&str)) -> Machines {
        let inner = Arc::new(Inner {
            registry,
            dial,
            timing,
            hooks,
            log,
            remotes: Arc::default(),
            links: Mutex::new(BTreeMap::new()),
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
        if let Some(t) = l.thread.take() {
            let _ = t.join();
        }
    }
}

impl Inner {
    fn lock_links(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, LinkThread>> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Handle {
    /// Re-read the registry: start links for new machines, stop (and drop
    /// the rows of) removed ones, restart changed ones. `machines.add` and
    /// `machines.rm` call it; a registry that doesn't load is logged and
    /// changes nothing.
    pub fn reload(&self) {
        let inner = &self.inner;
        let entries = match super::load(&inner.registry) {
            Ok(entries) => entries,
            Err(e) => {
                (inner.log)(&format!("machines: {e:#}"));
                return;
            }
        };
        let gone: Vec<LinkThread> = {
            let mut links = inner.lock_links();
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

fn spawn_link(entry: Entry, inner: Arc<Inner>) -> LinkThread {
    let (tx, rx) = mpsc::channel();
    let ctl = Arc::new(Ctl::default());
    let log = inner.log;
    let thread = {
        let (entry, ctl) = (entry.clone(), Arc::clone(&ctl));
        std::thread::Builder::new()
            .name(format!("serve-machine-{}", entry.name))
            .spawn(move || LinkLoop::new(entry, &inner, &ctl).run(&rx))
    };
    let thread = thread.map_err(|e| log(&format!("cannot start machine {}'s link: {e}", entry.name))).ok();
    LinkThread { entry, tx, ctl, thread }
}

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
    /// The last moment the link was wanted.
    last_want: Option<Instant>,
    failures: u32,
}

impl<'a> LinkLoop<'a> {
    fn new(entry: Entry, inner: &'a Inner, ctl: &'a Ctl) -> Self {
        Self { entry, inner, ctl, pending: VecDeque::new(), last_want: None, failures: 0 }
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

    /// The delay before the next attempt: 1, 2, 4 … units, capped.
    fn backoff(&mut self) -> Duration {
        let t = &self.inner.timing;
        let delay = t.backoff.saturating_mul(1u32 << self.failures.min(16)).min(t.backoff_cap);
        self.failures = self.failures.saturating_add(1);
        delay
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

    /// Wanted now (a machines watcher, a queued call), or within the linger.
    fn wanted(&mut self) -> bool {
        let now = Instant::now();
        if !self.pending.is_empty() || (self.inner.hooks.watching)() {
            self.last_want = Some(now);
        }
        self.last_want.is_some_and(|t| now < t + self.inner.timing.linger)
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

    /// Dial and say hello. The daemon's preferred host agent goes along for
    /// `ssh -A`. The socket is held for [`Ctl::cut`] before the hello.
    fn connect(&self) -> Result<Link, String> {
        let agent = crate::devsbd::bridge::host_agent();
        let duplex = (self.inner.dial)(&self.entry, agent.as_deref()).map_err(|e| format!("{}: {e:#}", self.entry.name))?;
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
                self.last_want = Some(Instant::now());
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
    use crate::serve::machines::ssh::Duplex;
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
        let entry = Entry { name: "box".into(), target: "me@box".into(), roots: vec![remote_root.clone()], command: None };
        crate::serve::machines::update(&registry, |e| e.push(entry)).unwrap();

        let remote = spawn_daemon(&remote_dir, remote_opts(&base, "web-1", 60_000));
        drop(connect(&remote_dir));
        let dials = Arc::new(AtomicUsize::new(0));
        let mut local_opts = opts(1, 300);
        local_opts.machines = Some(registry);
        local_opts.dial = Some({
            let (remote_dir, dials) = (remote_dir.clone(), Arc::clone(&dials));
            Arc::new(move |_: &Entry, _: Option<&Path>| {
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(Duplex::socket(endpoint::connect(&remote_dir)?))
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
            json!([{"name": "box", "target": "me@box", "state": "connected", "error": null, "version": "0.6.0", "runtime": "Disk", "command": null}])
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
        let entry = Entry { name: "m".into(), target: "m".into(), roots: vec![], command: None };
        crate::serve::machines::update(&registry, |e| e.push(entry)).unwrap();
        let dials = Arc::new(AtomicUsize::new(0));
        let dial: Dial = {
            let dials = Arc::clone(&dials);
            Arc::new(move |_, _| {
                dials.fetch_add(1, Ordering::SeqCst);
                Ok(mute_peer())
            })
        };
        let timing = Timing { call: Duration::from_millis(200), ..opts(1, 0).machine_timing };
        let hooks = Hooks { watching: Box::new(|| false), changed: Box::new(|| {}) };
        let machines = Machines::spawn(registry, dial, timing, hooks, |_| {});
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
}
