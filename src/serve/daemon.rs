//! The daemon side of `devsandbox serve`: start lock, accept loop, version
//! handoff and idle exit (docs/architecture/daemon/role.md).
//!
//! Threading: the calling thread runs the accept loop, waking every [`TICK`]
//! to re-check the idle countdown and the stop request (handoff, `shutdown`);
//! each connection gets its own thread reading JSON lines and answering them
//! (the API methods are in `api.rs`). Exiting (idle, handoff or shutdown) closes the listener and removes
//! the socket first, sends subscribers a `closing` notification, then shuts
//! down the read side of every connection, so idle ones end at once while a
//! request in flight still writes its response, and waits up to
//! [`DRAIN_TIMEOUT`] for them. Then the live host side ([`Host`]: bridges,
//! autostart) stops, killing every bridge, and so does the forward
//! [`Registry`], closing every forward's listener, and the machine links
//! (`machines::service`), killing their ssh. The start lock is released
//! last, so a successor never binds (or bridges) while this one drains.
//!
//! Notifications (docs/reference/api.md): one daemon-wide `serve-watch` thread
//! notices changes (store writes by this process at once, by others within
//! [`WATCH`] via the file stamp; `state.toml` by its stamp; the host's
//! container poll) and only *flags* them on each subscribed connection's
//! [`Outbox`]; it never does I/O, so a slow client can't hold up the others.
//! A connection's first `subscribe` starts its `serve-notify-<n>` thread,
//! which writes the flagged notifications.
//! Flags coalesce (one pending `inbox.changed` however many writes; the
//! host's sink posts `inbox.shown` the same way, latest line only, and the
//! forward registry `forwards.changed` / `forwards.status`; `opens.done`
//! keeps the last [`OPENS_KEPT`]), so the queue is bounded by
//! construction; every write to a connection, response or notification,
//! goes through its one writer lock, and a client that
//! doesn't drain its socket for [`WRITE_TIMEOUT`] is disconnected.
//!
//! `instances.watch` adds a config root to the connection's `Pending`; the
//! host poll reads every connection's watched roots each pass and posts
//! each root's `instances.snapshot` there, latest wins per root
//! (`snapshots.rs`). A hang-up drops the connection's entry, and its
//! watches with it. A `state.toml` change wakes the poll while anything is
//! watched. A watch with `machines: true` gets each push merged with the
//! machines' rows, and is what has the machines watch their roots; a
//! machine's push or state change re-publishes those watchers' roots from
//! the last local params (`Shared::locals`).
//!
//! A verb request (`verbs.rs`) runs on its connection's thread until the
//! verb's answer, writing `output`/`progress` through the same writer lock
//! and watching the socket for a hang-up ([`ConnPeer`]). The drain's
//! read-side shutdown ends it too, but sets `draining` first, so the verb
//! is left running rather than cancelled, and doesn't hold the drain; the
//! successor adopts it by its spool files and holds until it exits.
//!
//! A request with `machine` goes to that machine (`machines::forward`): a
//! quick call over its held link, a verb over an ssh of its own, relayed on
//! this connection. Such a verb has no spool here, so a drain cuts it like a
//! hang-up (cancelled there unless `detach`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::api::{self, Topic};
use super::endpoint::{self, Listener, Stream};
use super::fleet::{self, lent};
use super::forwards::{self, Registry};
use super::host::{Host, Observers, Waker};
use super::idle::{self, Decision, Holders};
use super::machines::{self, forward as fwd, link, service, ssh::{self, Dial}};
use super::opens;
use super::proto::{self, HelloParams, HelloResult, Notification, Request, Response, Version};
use super::helper;
use super::snapshots::{self, Join, Snapshots};
use super::verbs;
use crate::inbox::ops;

/// How often the accept loop wakes without a connection.
const TICK: Duration = Duration::from_millis(100);
/// How long an exiting daemon waits for in-flight requests.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a starting daemon waits for a lock whose holder doesn't answer
/// (starting up, or draining after a handoff). Longer than [`DRAIN_TIMEOUT`].
const LOCK_WAIT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(50);
/// How often the watcher re-checks the store file for writes by other
/// processes (the dashboard, `devsandbox rm`); this process's own writes
/// wake it at once.
const WATCH: Duration = Duration::from_millis(500);
/// A client that doesn't drain its socket for this long is disconnected.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Options {
    pub version: Version,
    /// No idle exit: `--keep-alive` (the unit `serve install` writes passes
    /// it) or `[serve] keep-alive = true` in the per-user global config
    /// ([`crate::daemon_config`]), resolved once by [`serve`].
    pub keep_alive: bool,
    /// A hub's `remotes.json` (this host is the hub while `member.toml`'s
    /// `hub` is `self`, [`Shared::hub`]); `None` is the default
    /// ([`machines::saved::path`]). Tests point it at their own file.
    pub remotes: Option<PathBuf>,
    /// [`idle::IDLE_TIMEOUT`] outside tests.
    pub idle_timeout: Duration,
    /// Run the live host side ([`Host`]: bridges, popups, autostart). Off in
    /// tests, which must not touch the user's state or containers.
    pub host: bool,
    /// The Inbox store the API serves; `None` is the default
    /// (`inbox::store::path`). Tests point it at their own file.
    pub inbox: Option<PathBuf>,
    /// How verbs run; `None` is the default ([`verbs::Runner::new`]: this
    /// binary, the user's state). Tests run scripts on scratch files.
    pub verbs: Option<verbs::Runner>,
    /// The `state.toml` the watcher stamps for `instances.changed`; `None`
    /// is the default (`State::path`). Tests point it at their own file.
    pub state: Option<PathBuf>,
    /// How a watched root's snapshot is joined; `None` is the default
    /// (`snapshot::collect_from` + `thread_children` from `state`). Tests
    /// inject one that reads neither the user's state nor a runtime.
    pub join: Option<Join>,
    /// The home `roots.list` expands `~` with and finds `~/.devsandboxes`
    /// in; `None` is `$HOME`. Tests point it at a scratch dir.
    pub home: Option<PathBuf>,
    /// `machines.toml`; `None` is the default ([`machines::path`]). Tests
    /// point it at their own file.
    pub machines: Option<PathBuf>,
    /// How `machines.probe` and `.add` reach a target at add time, before it
    /// has a link; `None` is ssh ([`ssh::probe_dial`]). Tests dial another
    /// test daemon's socket.
    pub probe: Option<Dial>,
    /// How an ssh machine's link is dialed (`fleet::dialer`); `None` is ssh
    /// (`Transport::ssh`). Tests run the far end's `fleet-link --peer`
    /// in-process.
    pub link_transport: Option<fleet::transport::Transport>,
    /// The machine links' backoff and timeouts; short in tests.
    pub machine_timing: service::Timing,
    /// How `helper.call` reaches containers; `None` is the host side's
    /// bridges (none without one). Tests pass a client over fake links.
    pub helper: Option<helper::HelperClient>,
    /// `member.toml` (`fleet::member`), for `member.*` and `membership.*`
    /// (`member.key` and `membership.json` beside it), the trust table and
    /// the capacity's caps; its dir (the data dir) holds the peers' fleet
    /// roots. `None` is the default ([`fleet::member::path`]). Tests point
    /// it at their own file.
    pub member: Option<PathBuf>,
    /// How the ssh machines' links follow a hub's version; `None` is the
    /// release download and a forced handoff on this daemon's socket
    /// (`fleet::update::Release`). Tests fake it.
    pub update_steps: Option<Arc<dyn fleet::update::Steps>>,
    /// The env a fleet peer's verbs run with; `None` is this process's.
    pub peer_env: Option<BTreeMap<String, String>>,
    /// This host's capacity before its caps (which come from
    /// `member.toml` either way), as `capacity.get` and a placed `run`
    /// read it; `None` is `capacity::HostCapacity` (the runtime's cached
    /// `info`, `HostFacts`, the poll's running count). Tests inject one.
    pub local_capacity: Option<LocalCapacity>,
    /// The agent a machine's `AGENT` streams reach while this host grants
    /// it agent ([`agent_for`]); `None` is `bridge::host_agent`. Tests inject one.
    pub host_agent: Option<HostAgent>,
    /// How often an `agent-route-<member>.sock`'s loop asks its member
    /// whether it serves `AGENT` (`fleet::agent_routes`); `agent::PROBE`
    /// outside tests.
    pub agent_probe: Duration,
    /// The argv a peer's `PTY` stream runs for a piped or tty exec; `None`
    /// is the runtime's `exec` (`fleet::pty::runtime_exec`). Tests run `sh`.
    pub pty_exec: Option<fleet::pty::ExecCommand>,
    /// How this host opens a URL (a bridge's `open` record routed here, an
    /// `open.deliver` taken); `None` is `open::open_here` on a thread of its
    /// own through the forwards registry. Tests record the job.
    pub open_here: Option<opens::OpenHere>,
}

/// [`Options::host_agent`].
pub type HostAgent = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// [`Options::local_capacity`].
pub type LocalCapacity = Arc<dyn Fn() -> super::capacity::Capacity + Send + Sync>;

impl Options {
    pub fn new(keep_alive: bool) -> Self {
        Self {
            version: Version::current(),
            keep_alive,
            remotes: None,
            idle_timeout: idle::IDLE_TIMEOUT,
            host: true,
            inbox: None,
            verbs: None,
            state: None,
            join: None,
            home: None,
            machines: None,
            probe: None,
            link_transport: None,
            machine_timing: service::Timing::default(),
            helper: None,
            member: None,
            update_steps: None,
            peer_env: None,
            local_capacity: None,
            host_agent: None,
            agent_probe: fleet::agent::PROBE,
            pty_exec: None,
            open_here: None,
        }
    }
}

/// The production [`Join`]: the snapshot of a root from the poll's runtime
/// pass, `thread_children` from `state`.
fn default_join(state: PathBuf) -> Join {
    Arc::new(move |dir, view| api::InstancesList::of(crate::snapshot::collect_from(dir, view), &state))
}

/// Why [`run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Another daemon holds the lock and answers: nothing to do.
    AlreadyRunning,
    Idle,
    /// A newer client asked, or a forced hello; it starts the successor.
    Handoff,
    /// A client sent `shutdown` (`serve install`, before the service manager
    /// starts its own daemon).
    Shutdown,
}

/// `devsandbox serve`: run the daemon in the foreground until it idles out or
/// hands off. `socket_dir` overrides [`endpoint::socket_dir`] (the lazy start
/// passes the dir its client resolved, so both agree). The global config is
/// read once here; a bad one is logged and ignored so the daemon still starts.
pub fn serve(socket_dir: Option<PathBuf>, keep_alive: bool) -> Result<()> {
    let dir = match socket_dir {
        Some(dir) => dir,
        None => endpoint::socket_dir()?,
    };
    let config = crate::daemon_config::load().unwrap_or_else(|e| {
        log(&format!("warning: {e:#}; using defaults"));
        Default::default()
    });
    let opts = Options::new(keep_alive || config.serve.keep_alive);
    match run(&dir, &opts)? {
        Exit::AlreadyRunning => {}
        Exit::Idle => log("idle, exiting"),
        Exit::Handoff => log("handed off to a newer (or forced) binary, exiting"),
        Exit::Shutdown => log("shutdown requested, exiting"),
    }
    Ok(())
}

/// Run a daemon on `dir` (created 0700 if needed). Returns at once with
/// [`Exit::AlreadyRunning`] when another daemon owns the dir.
pub fn run(dir: &Path, opts: &Options) -> Result<Exit> {
    endpoint::ensure_private_dir(dir)?;
    let Some(_lock) = acquire_lock(dir)? else {
        return Ok(Exit::AlreadyRunning);
    };
    let inbox = match &opts.inbox {
        Some(path) => path.clone(),
        None => crate::inbox::store::path()?,
    };
    let state = match &opts.state {
        Some(path) => path.clone(),
        None => crate::state::State::path()?,
    };
    let listener = Listener::bind(dir)?;
    let member = member_path(opts);
    // The daemon is `member.toml`'s only writer (fleet plan decision 3): its
    // `hub` is read once here, then kept by each write of it (an adopted
    // membership log, a leave: [`Shared::set_hub`]).
    let hub = match &member {
        Ok(path) => fleet::member::load(path).unwrap_or_else(|e| {
            log(&format!("warning: {e:#}; in no fleet until it's fixed"));
            None
        }),
        Err(_) => None,
    };
    let hub = hub.map(|m| m.hub()).unwrap_or_default();
    log(&format!(
        "listening on {} (pid {}, version {} build {}{})",
        endpoint::socket_path(dir).display(),
        std::process::id(),
        opts.version.semver,
        opts.version.build,
        match (hub.is_this(), opts.keep_alive) {
            (true, _) => ", fleet hub (keep-alive)",
            (false, true) => ", keep-alive",
            (false, false) => "",
        },
    ));

    let join = opts.join.clone().unwrap_or_else(|| default_join(state.clone()));
    let mut shared = Shared::new(opts.version.clone(), inbox, state, join);
    shared.home = opts.home.clone().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    shared.socket_dir = dir.to_path_buf();
    shared.local_capacity = opts.local_capacity.clone();
    shared.peer_env = opts.peer_env.clone().unwrap_or_else(|| super::verbs_client::caller_env(std::env::vars_os()));
    match member {
        Ok(path) => shared.member = Some(path),
        Err(e) => log(&format!("warning: no member.toml (no peer is served, `member.*` fails): {e:#}")),
    }
    shared.hub = Arc::new(Mutex::new(hub));
    shared.hostname = link::hostname();
    shared.route_budget = opts.machine_timing.handshake;
    let registry_path = match &opts.machines {
        Some(path) => path.clone(),
        None => machines::path()?,
    };
    // Every machine, ssh or inbound, is dialed at its link's socket here;
    // only `machines.probe`/`.add` ssh to a target (tests inject that part).
    let dial = fleet::dialer::dial(dir.to_path_buf());
    let route = fleet::dialer::routed(dir.to_path_buf());
    let probe = opts.probe.clone().unwrap_or_else(|| ssh::probe_dial(link::client_name()));
    // Whenever the daemon runs, host side or not: every registered machine's
    // link is held (a test daemon's registry has none unless the test writes
    // one). The hooks reach `shared` once it exists; until then nothing
    // watches, there's nobody to tell and a machine's streams are refused.
    let hooked: Arc<OnceLock<std::sync::Weak<Shared>>> = Arc::default();
    let hold = machine_dialer(dir, opts, &hooked, &shared.hub);
    let hooks = {
        let (a, b, c, d) = (Arc::clone(&hooked), Arc::clone(&hooked), Arc::clone(&hooked), Arc::clone(&hooked));
        service::Hooks {
            watching: Box::new(move || a.get().and_then(std::sync::Weak::upgrade).is_some_and(|s| s.watching_machines())),
            changed: Box::new(move || {
                if let Some(s) = b.get().and_then(std::sync::Weak::upgrade) {
                    s.publish_machines();
                    s.graph.poke();
                }
            }),
            hub: Box::new(move || c.get().and_then(std::sync::Weak::upgrade).and_then(|s| s.hub_of())),
            routed: Box::new(move || d.get().and_then(std::sync::Weak::upgrade).map(|s| s.routed_reach()).unwrap_or_default()),
        }
    };
    let hub = {
        let remotes = match &opts.remotes {
            Some(path) => path.clone(),
            None => machines::saved::path()?,
        };
        let hub = Arc::clone(&shared.hub);
        service::Hub { remotes, is_hub: Arc::new(move || lock_hub(&hub).is_this()) }
    };
    // Inbound neighbors are machines too: the service follows them.
    let inbound: Arc<machines::inbound::Neighbors> = Arc::default();
    let links =
        service::Machines::spawn(registry_path.clone(), Arc::clone(&inbound), dial, route, hold, opts.machine_timing, hooks, Some(hub), log);
    let forwards_machines = registry_path.clone();
    if let Some(agent) = &opts.host_agent {
        shared.host_agent = Arc::clone(agent);
    }
    if let Some(exec) = opts.pty_exec {
        shared.pty_exec = exec;
    }
    shared.machines = Some(machines::api::Ctx {
        registry: registry_path,
        probe,
        version: opts.version.semver.clone(),
        links: Some(links.handle()),
        inbound: Arc::clone(&inbound),
        member: shared.member.clone(),
        hostname: shared.hostname.clone(),
        // `machines.add` or a leave wrote this host's `hub`: the same
        // follow-up as a `membership.adopt` call's.
        hub_changed: {
            let hooked = Arc::clone(&hooked);
            Box::new(move |hub| {
                if let Some(s) = hooked.get().and_then(std::sync::Weak::upgrade) {
                    s.set_hub(hub);
                }
            })
        },
        key_rotated: {
            let hooked = Arc::clone(&hooked);
            Box::new(move || {
                if let Some(s) = hooked.get().and_then(std::sync::Weak::upgrade) {
                    s.key_rotated();
                }
            })
        },
        end_inbound: {
            let hooked = Arc::clone(&hooked);
            Box::new(move |name: &str| {
                if let Some(s) = hooked.get().and_then(std::sync::Weak::upgrade) {
                    s.end_inbound(name);
                }
            })
        },
        notify: {
            let desktop = opts.host;
            Box::new(move |body: &str| {
                log(&format!("fleet: {body}"));
                if desktop {
                    let popup = crate::inbox::ShownPopup { key: None, level: crate::devsbd::notify::Level::Warn, body: body.to_string() };
                    crate::devsbd::desktop::notify_desktop("fleet", &popup);
                }
            })
        },
        exposed: {
            let hooked = Arc::clone(&hooked);
            Box::new(move || hooked.get().and_then(std::sync::Weak::upgrade).map(|s| s.exposed_workers(None)).unwrap_or_default())
        },
        // A grant changed: in this host's link report at once (fleet plan
        // decision 66), and in the federated snapshots' `agent`.
        agent_changed: {
            let hooked = Arc::clone(&hooked);
            Box::new(move || {
                if let Some(s) = hooked.get().and_then(std::sync::Weak::upgrade) {
                    s.graph.republish();
                    s.publish_machines();
                }
            })
        },
        log_from: {
            let (member, dir) = (shared.member.clone(), dir.to_path_buf());
            Box::new(move |peer: &str| follow_configs_now(member.as_deref(), &dir, peer))
        },
    });
    let shared = Arc::new(shared);
    let _ = hooked.set(Arc::downgrade(&shared));
    // This host's link report and the published graph (fleet plan step
    // 10b.2): only with a `member.toml`, beside which the log and key are.
    let publisher = shared.member.clone().map(|member| {
        let (hub, always) = (Arc::clone(&shared.hub), Arc::clone(&shared.hub));
        let keep_alive = opts.keep_alive;
        fleet::reports::Publisher::spawn(Arc::clone(&shared.graph), fleet::reports::Io {
            member,
            hostname: shared.hostname.clone(),
            links: links.handle(),
            hub: Arc::new(move || lock_hub(&hub).clone()),
            always_on: Arc::new(move || keep_alive || lock_hub(&always).is_this()),
            log,
        })
    });
    // A route socket per member this host has no link up to (fleet plan
    // step 10b.5), following the graph and the links: only with a
    // `member.toml`, as the graph.
    let routes = shared.member.is_some().then(|| {
        let (wanting, accepting) = (Arc::clone(&shared), Arc::clone(&shared));
        fleet::routes::Routes::spawn(fleet::routes::Io {
            dir: dir.to_path_buf(),
            wanted: Box::new(move || wanting.routed_members()),
            accept: Box::new(move |name, conn| accepting.accept_routed(name, conn)),
            log,
        })
    });
    // An `agent-route-<member>.sock` per member whose report grants this
    // host agent and that doesn't dial it (fleet plan decision 66),
    // following the graph and the links as the route sockets do.
    let agent_routes = shared.member.is_some().then(|| {
        let wanting = Arc::clone(&shared);
        fleet::agent_routes::AgentRoutes::spawn(fleet::agent_routes::Io {
            dir: dir.to_path_buf(),
            wanted: Box::new(move || wanting.agent_routed_members()),
            every: opts.agent_probe,
            log,
        })
    });
    if let (Some(routes), Some(agents)) = (&routes, &agent_routes) {
        let (routes, agents) = (routes.poke(), agents.poke());
        shared.graph.on_change(Box::new(move || {
            routes.poke();
            agents.poke();
        }));
    }
    let _ = shared.runner.set(match &opts.verbs {
        Some(runner) => runner.clone(),
        None => verbs::Runner::new()?,
    });
    // Under the start lock, so a predecessor's verbs are orphans by now:
    // they hold this daemon until they exit.
    if let Some(runner) = shared.runner.get() {
        shared.verbs.adopt(&runner.spool, log);
    }
    let registry = if opts.host {
        let shared = Arc::clone(&shared);
        let store = super::forward_store::path()?;
        let on_event = Box::new(move |event| shared.publish_forwards(event));
        let routed_of = links.handle();
        let routed: super::forwards::RoutedNames = Arc::new(move || routed_of.routed().into_iter().map(|r| r.name).collect());
        Some(Registry::spawn(log, store, forwards_machines, Arc::clone(&inbound), routed, dir.to_path_buf(), on_event))
    } else {
        None
    };
    if let Some(registry) = &registry {
        let _ = shared.forwards.set(registry.handle());
    }
    let on_open_done = open_done(&shared);
    let open_here = opts.open_here.clone().unwrap_or_else(|| {
        opens::open_here(opens::forwarder(registry.as_ref().map(Registry::handle)), log, Arc::clone(&on_open_done))
    });
    let _ = shared.open_here.set(Arc::clone(&open_here));
    let host = opts.host.then(|| {
        let shared = Arc::clone(&shared);
        // Configured `forwardPorts` follow the running containers.
        let sync = registry.as_ref().map(Registry::handle);
        let observers = {
            let (watching, publishing, owning) = (Arc::clone(&shared), Arc::clone(&shared), Arc::clone(&shared));
            Observers {
                watched: Box::new(move || watching.watched_roots()),
                cache: Arc::clone(&shared.snapshots),
                join: Arc::clone(&shared.join),
                on_snapshot: Box::new(move |dir, params| publishing.publish_snapshot(dir, params)),
                own: Box::new(move || owning.own()),
                capacity: Arc::clone(&shared.host_capacity),
            }
        };
        let attaches = shared.attaches.clone();
        let members = links.handle();
        let opener = opens::opener(opens::Sender {
            prepare: Arc::new(move |instance: &str, id: &str, url: &str| crate::open::prepare(instance, id, url, log)),
            latest: Arc::new(move |id: &str| attaches.latest(id, Instant::now())),
            call_member: Arc::new(move |m: &str, method: &str, params: Value| {
                let r = members.call_member(m, method, params)?;
                match r.error {
                    Some(e) => Err(format!("{} ({})", e.message, e.code)),
                    None => Ok(r.result.unwrap_or(Value::Null)),
                }
            }),
            open_here,
            done: Arc::clone(&on_open_done),
            log,
        });
        let sink = crate::devsbd::bridge::SinkHooks {
            on_shown: Some(Box::new(move |instance: &str, line: String| shared.publish_shown(instance, line))),
            opener,
            on_open_done: Some(on_open_done),
            log,
        };
        Host::start(
            log,
            sink,
            Box::new(move |running: &[String]| {
                if let Some(sync) = &sync {
                    sync.sync(running.to_vec());
                }
            }),
            observers,
        )
    });
    if let Some(host) = &host {
        let _ = shared.bridges.set(host.waker());
    }
    if let Some(client) = opts.helper.clone().or_else(|| host.as_ref().map(|h| helper::HelperClient::new(h.board(), log))) {
        helper::install(client.clone());
        let _ = shared.helper.set(client);
    }
    let watcher = Watcher::spawn(&shared, host.as_ref().map(Host::changes));
    let exit = loop {
        if let Some(exit) = *shared.stop.lock().unwrap() {
            break exit;
        }
        if let Some(runner) = shared.runner.get() {
            shared.verbs.check_adopted(&runner.spool, log);
        }
        let now = Instant::now();
        let holders = shared.holders(host.as_ref(), registry.as_ref());
        if holders.any() {
            *shared.idle_since.lock().unwrap() = now;
        }
        let idle_since = *shared.idle_since.lock().unwrap();
        // A hub holds its members' links: it never idles out.
        let keep_alive = opts.keep_alive || shared.is_hub();
        if idle::decide(&holders, idle_since, now, keep_alive, opts.idle_timeout) == Decision::Exit {
            break Exit::Idle;
        }
        match listener.accept_timeout(TICK) {
            Ok(Some(stream)) => shared.spawn_conn(shared.next_conn_id(), stream),
            Ok(None) => {}
            Err(e) => {
                // EMFILE and friends: don't spin, the next tick retries.
                log(&format!("accept failed: {e}"));
                std::thread::sleep(TICK);
            }
        }
    };

    // Stop accepting before anything else, then release the handoff and
    // shutdown replies: once a client reads one, nothing answers on the
    // socket, so the successor it (or the service manager) starts waits for
    // this lock instead of seeing a live daemon and giving up.
    listener.remove();
    shared.close();
    shared.publish_closing(match exit {
        Exit::Handoff => "handoff",
        Exit::Shutdown => "shutdown",
        _ => "idle",
    });
    shared.drain(DRAIN_TIMEOUT);
    drop(watcher);
    // Its streams go over the route sockets and the links: stopped first.
    drop(agent_routes);
    // Its streams go over the links: stopped before them.
    drop(routes);
    // Its calls go over the links: stopped before them.
    drop(publisher);
    // The machine links close (their ssh killed) after the connections whose
    // forwarded calls they may still answer.
    drop(links);
    // Before the lock goes: a successor's bridges must not overlap ours (two
    // sink bridges on one container would split its notify/control streams).
    drop(host);
    // Likewise the forwards: their listeners close here, so a successor can
    // bind the configured ones' saved host ports again. Ad-hoc ones are
    // closed too; the next daemon restores them from `forwards.toml`.
    drop(registry);
    Ok(exit)
}

fn lock_hub(hub: &Mutex<fleet::member::Hub>) -> std::sync::MutexGuard<'_, fleet::member::Hub> {
    hub.lock().unwrap_or_else(|e| e.into_inner())
}

/// `member.toml`: [`Options::member`], else the data dir's.
fn member_path(opts: &Options) -> Result<PathBuf> {
    match &opts.member {
        Some(path) => Ok(path.clone()),
        None => fleet::member::path(),
    }
}

/// How this host follows a hub's build (the ssh machines' links):
/// [`Options::update_steps`], dev builds as `member` (`member.toml`) says
/// at each `Hello`.
fn follow_hub(dir: &Path, opts: &Options, member: PathBuf) -> fleet::follow::Follow {
    let steps = opts
        .update_steps
        .clone()
        .unwrap_or_else(|| Arc::new(fleet::update::Release { socket_dir: dir.to_path_buf() }));
    fleet::follow::Follow {
        steps,
        dev_builds: Arc::new(move || fleet::member::load(&member).ok().flatten().is_some_and(|m| m.takes_dev_builds())),
    }
}

/// What carries an ssh machine's link (`fleet::dialer::Dialer::hold`):
/// `fleet-link --peer <this host's member name>` through
/// [`Options::link_transport`], this daemon answering the machine's streams
/// in-process ([`Shared::open_lent`]) as peer `<entry name>`, with its
/// fleet root `fleet/<entry name>/` under the data dir and the daemon's env
/// (decision 24 of the fleet plan). Without a data dir no stream is served.
/// Its hello says `hub` as it is at each dial.
fn machine_dialer(
    dir: &Path,
    opts: &Options,
    hooked: &Arc<OnceLock<std::sync::Weak<Shared>>>,
    hub: &Arc<Mutex<fleet::member::Hub>>,
) -> service::Hold {
    let member = member_path(opts);
    let data = member.as_ref().ok().and_then(|m| m.parent()).map(Path::to_path_buf);
    let follow = follow_hub(dir, opts, member.as_ref().cloned().unwrap_or_default());
    let hostname = link::hostname();
    let member_toml = member.as_ref().ok().cloned();
    let identity: Arc<dyn Fn() -> Result<fleet::proof::Identity> + Send + Sync> = {
        let (member_toml, hostname) = (member_toml.clone(), hostname.clone());
        Arc::new(move || fleet::proof::Identity::load(member_toml.as_deref().context("this daemon has no member.toml")?, &hostname))
    };
    let linked_member = member_toml.clone();
    // A machine's newer membership entries: what `membership.adopt` does,
    // `hub` followed the same way.
    let adopt: Arc<dyn Fn(&fleet::membership::Stored) -> Result<(), String> + Send + Sync> = {
        let (hooked, hostname) = (Arc::clone(hooked), hostname.clone());
        Arc::new(move |newer| {
            let path = member_toml.as_deref().ok_or("this daemon has no member.toml")?;
            let taken = fleet::membership_api::adopt(path, &hostname, newer).map_err(|e| e.message)?;
            if let Some(s) = hooked.get().and_then(std::sync::Weak::upgrade) {
                s.set_hub(taken.hub);
                if taken.rotated {
                    s.key_rotated();
                }
            }
            Ok(())
        })
    };
    let member_name: Arc<dyn Fn() -> Result<String> + Send + Sync> = Arc::new(move || match &member {
        Ok(path) => fleet::dialer::member_name(path, &hostname),
        Err(e) => Err(anyhow::anyhow!("{e:#}")),
    });
    let hello = {
        let (hub, name) = (Arc::clone(hub), Arc::clone(&member_name));
        Arc::new(move || {
            let hub = lock_hub(&hub).clone();
            fleet::update::hello_for(&hub, name().ok().as_deref())
        })
    };
    let open: fleet::dialer::Open = {
        let hooked = Arc::clone(hooked);
        Arc::new(move |proven: &fleet::proof::Proven, channel: lent::Channel| {
            let shared = hooked.get()?.upgrade()?;
            let name = proven.name.as_str();
            let peer = shared
                .fleet_peer(data.as_deref()?, name, proven.key, proven.introduced_by.clone(), None)
                .map_err(|e| log(&format!("machine {name}: {e:#}")))
                .ok()?;
            shared.open_lent(channel, &Arc::new(peer))
        })
    };
    let dialer = fleet::dialer::Dialer {
        transport: opts.link_transport.clone().unwrap_or_else(fleet::transport::Transport::ssh),
        dir: dir.to_path_buf(),
        member: member_name,
        open,
        hello,
        follow,
        handshake: opts.machine_timing.handshake,
        log,
        identity,
        adopt,
        linked: {
            let (member, dir) = (linked_member, dir.to_path_buf());
            Arc::new(move |name: &str| follow_configs(member.as_deref(), &dir, name))
        },
    };
    Arc::new(move |entry, held| dialer.hold(entry, held))
}

/// Take the start lock, or `None` when a live daemon (one answering on the
/// socket) holds it. A holder that doesn't answer is starting up or draining
/// after a handoff; wait for it, up to [`LOCK_WAIT`].
fn acquire_lock(dir: &Path) -> Result<Option<File>> {
    let path = endpoint::lock_path(dir);
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e).with_context(|| format!("cannot lock {}", path.display())),
        }
        if endpoint::connect(dir).is_ok() {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            bail!(
                "{} is locked but nothing answers on {}",
                path.display(),
                endpoint::socket_path(dir).display()
            );
        }
        std::thread::sleep(LOCK_POLL);
    }
}

struct Shared {
    version: Version,
    /// The Inbox store the API serves.
    inbox: PathBuf,
    /// The `state.toml` whose changes are `instances.changed`.
    state: PathBuf,
    /// Live connections. Its size is the `clients` holder count.
    conns: Mutex<HashMap<u64, ConnEntry>>,
    /// The last connection id handed out: the listener's and the machine
    /// links' connections share it.
    next_conn: AtomicU64,
    /// Inbox changes this daemon has seen: the `generation` of
    /// `inbox.changed`.
    inbox_generation: AtomicU64,
    /// The last moment a holder was seen.
    idle_since: Mutex<Instant>,
    /// A client asked the daemon to exit: [`Exit::Handoff`] or
    /// [`Exit::Shutdown`]. The first request wins.
    stop: Mutex<Option<Exit>>,
    /// The listener is gone; handoff and shutdown replies may go out.
    closed: (Mutex<bool>, Condvar),
    /// The host side's bridges, for `bridges.ensure`; unset without one.
    bridges: OnceLock<Waker>,
    /// The forward registry, for `forwards.*`; unset without a host side.
    forwards: OnceLock<forwards::Handle>,
    /// Host → container calls, for `helper.call`; unset without a host side.
    helper: OnceLock<helper::HelperClient>,
    /// How verbs run; unset only in tests that build a bare `Shared`.
    runner: OnceLock<verbs::Runner>,
    /// The verbs not reaped yet, a predecessor's adopted ones included: the
    /// `verbs` holder count.
    verbs: Arc<verbs::InFlight>,
    /// Verbs forwarded to a machine, in flight: counted with `verbs`, so
    /// the daemon doesn't idle out under a remote verb's ssh.
    remote_verbs: AtomicUsize,
    /// Set when the drain starts: its read-side shutdown is not a client
    /// hanging up, so a verb's connection must not cancel on it.
    draining: AtomicBool,
    /// The watched roots' last snapshots (the host poll fills it).
    snapshots: Arc<Snapshots>,
    /// How a root's snapshot is joined: the poll's, and a new watch's first
    /// (disk-only) one.
    join: Join,
    /// What `roots.list` reads `~` as.
    home: Option<PathBuf>,
    /// `member.toml`'s `hub`: read at the start, then set by each write of
    /// it (`membership.adopt`, `machines.add`, a leave: [`Shared::set_hub`]);
    /// the daemon is the file's only writer (fleet plan decision 3), so
    /// nothing re-reads it. The keep-alive, placement,
    /// `remotes.json` (`service::Hub`) and the links' hellos follow it.
    hub: Arc<Mutex<fleet::member::Hub>>,
    /// The `machines.*` methods' registry and probe, and the machine links;
    /// unset only in tests that build a bare `Shared` (the methods are then
    /// unknown, and no machine exists).
    machines: Option<machines::api::Ctx>,
    /// Each watched root's last local `instances.snapshot` params (the poll's,
    /// or a new watch's disk one): what a machine's push is merged with.
    locals: Mutex<BTreeMap<PathBuf, Arc<Value>>>,
    /// Held through a [`publish_machines`](Shared::publish_machines), so
    /// one called later posts later remotes (taken before `locals`).
    publishing: Mutex<()>,
    /// `member.toml` ([`Options::member`]), for `member.*`, the trust table
    /// and the capacity's caps; its dir is the data dir. `None` without a
    /// data dir and in tests that build a bare `Shared`.
    member: Option<PathBuf>,
    /// What names this host when `member.toml` doesn't.
    hostname: String,
    /// The socket dir: a machine's `fleet-<name>.sock` is here.
    socket_dir: PathBuf,
    /// [`Options::local_capacity`].
    local_capacity: Option<LocalCapacity>,
    /// [`Options::host_agent`], resolved.
    host_agent: HostAgent,
    /// [`Options::pty_exec`], resolved.
    pty_exec: fleet::pty::ExecCommand,
    /// This host's capacity on demand; the host poll keeps its running
    /// count.
    host_capacity: Arc<super::capacity::HostCapacity>,
    /// The env a fleet peer's verbs run with ([`Options::peer_env`], else
    /// this process's): `lent::Peer::env` of a machine's link streams and
    /// of a `link.attach {channel}`.
    peer_env: BTreeMap<String, String>,
    /// Channel streams being served ([`Shared::serve_channel`]'s `PTY`,
    /// `TUNNEL`, `TRANSFER`, `AGENT` and `ROUTE` threads): counted with the
    /// `clients` holders, like connections.
    channels: Arc<AtomicUsize>,
    /// The link reports this host holds (`fleet.report`, `fleet.graph`,
    /// `graph.get`), in memory only; `fleet::reports::Publisher` keeps
    /// this host's own.
    graph: Arc<fleet::reports::Published>,
    /// What all the paths of one routed stream this host opens share
    /// (`fleet::routes`): the machine links' handshake wait
    /// (`service::Timing::handshake`), a client's hello waiting as long.
    route_budget: Duration,
    /// Who attached to which instance last: a `PTY` stream's origin
    /// ([`Shared::serve_channel`]) and `instances.attach`'s holders.
    attaches: super::attaches::Attaches,
    /// How this host opens a URL ([`Options::open_here`], resolved): what
    /// a taken `open.deliver` runs; unset in tests that build a bare
    /// `Shared`.
    open_here: OnceLock<opens::OpenHere>,
    /// `open.deliver`'s limit, per `(peer, instance)`: as a bridge's sink
    /// limits a container's own.
    open_limit: Mutex<crate::devsbd::desktop::RateLimit>,
    /// The config roots whose verbs already said this host isn't the
    /// fleet's authority ([`config_notes`]).
    told_not_authority: Mutex<BTreeSet<String>>,
}

impl Shared {
    fn new(version: Version, inbox: PathBuf, state: PathBuf, join: Join) -> Self {
        Self {
            version,
            inbox,
            state,
            conns: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(0),
            inbox_generation: AtomicU64::new(0),
            idle_since: Mutex::new(Instant::now()),
            stop: Mutex::new(None),
            closed: (Mutex::new(false), Condvar::new()),
            bridges: OnceLock::new(),
            forwards: OnceLock::new(),
            helper: OnceLock::new(),
            runner: OnceLock::new(),
            verbs: Arc::default(),
            remote_verbs: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            snapshots: Arc::default(),
            join,
            home: None,
            hub: Arc::default(),
            machines: None,
            locals: Mutex::default(),
            publishing: Mutex::default(),
            member: None,
            hostname: String::new(),
            socket_dir: PathBuf::new(),
            local_capacity: None,
            host_agent: Arc::new(crate::devsbd::bridge::host_agent),
            pty_exec: fleet::pty::runtime_exec,
            host_capacity: Arc::new(super::capacity::HostCapacity::new(Arc::new(|| crate::runtime::backend().engine_info()))),
            peer_env: BTreeMap::new(),
            channels: Arc::default(),
            graph: Arc::default(),
            route_budget: service::Timing::default().handshake,
            attaches: Default::default(),
            open_here: OnceLock::new(),
            open_limit: Mutex::new(crate::devsbd::desktop::RateLimit::new(crate::open::OPEN_RATE_BURST, crate::open::OPEN_RATE_REFILL)),
            told_not_authority: Mutex::default(),
        }
    }

    /// This host's capacity now (`capacity.get`, a placed `run`'s own
    /// candidate): [`Options::local_capacity`] or the [`HostCapacity`]
    /// report, with `member.toml`'s caps.
    ///
    /// [`HostCapacity`]: super::capacity::HostCapacity
    fn capacity(&self) -> super::capacity::Capacity {
        let own = self.own();
        let base = match &self.local_capacity {
            Some(f) => f(),
            None => self.host_capacity.report(&own),
        };
        base.with_own(&own)
    }

    /// What the capacity report carries of this host's own files:
    /// `member.toml`'s `[caps]` (none while it's missing or doesn't parse).
    /// Read each time, so a hand edit shows on the next poll.
    fn own(&self) -> super::capacity::Own {
        let member = self.member.as_deref().and_then(|p| fleet::member::load(p).ok().flatten());
        super::capacity::Own { caps: member.map(|m| m.caps).unwrap_or_default() }
    }

    fn links(&self) -> Option<&service::Handle> {
        self.machines.as_ref()?.links.as_ref()
    }

    /// This host is its fleet's hub ([`Shared::hub`] is `self`).
    fn is_hub(&self) -> bool {
        lock_hub(&self.hub).is_this()
    }

    /// This host's hub when another member is, and this host's own member
    /// name: what the machine links watch the hub's fleet with
    /// (`service::Hooks::hub`).
    fn hub_of(&self) -> Option<service::HubOf> {
        let fleet::member::Hub::Other(hub) = lock_hub(&self.hub).clone() else { return None };
        // No name of its own: nothing to guard against (the hub leaves the
        // asking member out anyway).
        let me = self.member.as_deref().and_then(|p| fleet::dialer::member_name(p, &self.hostname).ok()).unwrap_or_default();
        Some(service::HubOf { hub, me })
    }

    /// `hub` was written (from an adopted membership log, or cleared on
    /// leaving): keep it, and when it changed end every
    /// link this host holds and every one it's dialed on, so both ends
    /// redial saying the new hub (`fleet::update::hello_of`). Held links:
    /// `service::Handle::cycle`. Dialed ones: each `fleet-link --peer`'s
    /// held `links.inbound` connection gets `links.cycle`
    /// (`commands::fleet_link::LINKS_CYCLE`), on which it hangs up.
    fn set_hub(&self, hub: fleet::member::Hub) {
        {
            let mut cur = lock_hub(&self.hub);
            if *cur == hub {
                return;
            }
            *cur = hub.clone();
        }
        log(&format!("this host's hub is now {}: its links end and redial", hub.field().as_deref().unwrap_or("none")));
        self.cycle_links();
    }

    /// This host's pending next key was promoted to `member.key` (fleet
    /// plan step 10a.6, `membership_api::Taken::rotated`): every link it's
    /// on ends, as on a hub change, so both ends redial and the link proves
    /// the new key (a peer keeps the key a link proved, `lent::Peer::key`).
    pub(crate) fn key_rotated(&self) {
        log("this host's member key rotated: its links end and redial proving the new key");
        self.cycle_links();
    }

    /// End every link this host holds (`service::Handle::cycle`) and every
    /// one it's dialed on (`links.cycle` on each `fleet-link --peer`'s held
    /// `links.inbound` connection).
    fn cycle_links(&self) {
        if let Some(links) = self.links() {
            links.cycle();
        }
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                if p.inbound.is_some() {
                    p.cycle = Some(None);
                }
                p.inbound.is_some()
            });
        }
    }

    /// Inbound neighbor `name` is out of this host's fleet (`machine rm`,
    /// `fleet.forget`): its `fleet-link`'s held `links.inbound` connection
    /// gets `links.cycle` saying so, and it hangs up the link.
    fn end_inbound(&self, name: &str) {
        log(&format!("{name} is out of this host's fleet: its inbound link ends"));
        let why = format!("`{name}` is out of this host's fleet: the link ends");
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                let hit = p.inbound.as_deref() == Some(name);
                if hit {
                    p.cycle = Some(Some(why.clone()));
                }
                hit
            });
        }
    }

    /// Every machine's state and rows, for a merge.
    fn remotes(&self) -> Vec<snapshots::Machine> {
        self.links().map(|l| l.remotes().all()).unwrap_or_default()
    }

    fn holders(&self, host: Option<&Host>, registry: Option<&Registry>) -> Holders {
        let clients = self.conns.lock().unwrap().values().filter(|c| c.holds()).count() + self.channels.load(Ordering::SeqCst);
        let forwards = registry.map_or(0, Registry::active);
        let verbs = self.verbs.len() + self.remote_verbs.load(Ordering::SeqCst);
        let links = self.links().map_or(0, |l| l.entries().iter().filter(|e| matches!(e.kind, machines::Kind::Ssh { .. })).count());
        let Some(host) = host else { return Holders { clients, forwards, verbs, links, ..Default::default() } };
        Holders {
            clients,
            forwards,
            verbs,
            links,
            live_instances: host.live_instances(),
            control: host.control(),
            autostart: host.autostarting(),
            ..Default::default()
        }
    }

    fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// A local client's connection: the whole API.
    fn spawn_conn(self: &Arc<Self>, id: u64, stream: Stream) {
        self.spawn_conn_as(id, stream, Table::Full);
    }

    /// [`lent::Files`] here: what a peer's [`lent::Reach`] is read from.
    fn reach_files(&self) -> lent::Files {
        lent::Files {
            state: self.state.clone(),
            member: self.member.clone(),
            home: self.home.clone(),
        }
    }

    /// The data dir a fleet peer's root goes under (`lent::peer_fleet_root`):
    /// `member.toml`'s; `None` without one.
    fn data_dir(&self) -> Option<&Path> {
        self.member.as_deref()?.parent()
    }

    /// Fleet member `name` as this host serves its streams, whatever
    /// carried them (a machine link's, a `link.attach`'s, a routed stream's
    /// origin: fleet plan decision 57): the key it proved, its introducer,
    /// its fleet root `fleet/<name>/` under `data` (created), the daemon's
    /// env, `agent` its relayed agent (only a direct link's dialed end has
    /// one).
    fn fleet_peer(&self, data: &Path, name: &str, key: fleet::identity::PublicKey, introduced_by: Option<String>, agent: Option<PathBuf>) -> Result<lent::Peer> {
        Ok(lent::Peer {
            name: name.to_string(),
            fleet_root: Some(lent::peer_fleet_root(data, name)?),
            env: Some(self.peer_env.clone()),
            agent,
            key: Some(key),
            introduced_by,
        })
    }

    /// One stream of an ssh machine's link (`fleet::dialer`), on
    /// `channel`: a socket pair whose one end goes to the machine, the other [`serve_channel`](Self::serve_channel)'s
    /// with `scope`. An `API` stream's first byte is the daemon's, as on a
    /// `link.attach` connection (fleet plan decision 27). `None` when
    /// refused.
    fn open_lent(self: &Arc<Self>, channel: lent::Channel, scope: &Arc<lent::Peer>) -> Option<crate::devsbd::mux::Conn> {
        let (hub, ours) = Stream::pair().ok()?;
        self.serve_channel(channel, scope, ours).then(|| hub.into())
    }

    /// Serve `stream` as one `channel` stream of `peer`'s, under this
    /// host's trust table for it: a `link.attach {channel}` connection's
    /// (handed off once answered) and a machine link's ([`open_lent`](Self::open_lent)).
    /// Quick (a machine link calls it on its mux's read thread): every
    /// channel but `api` runs on a thread of its own, counted as a holder.
    ///
    /// - `api`: a `Peer` connection ([`Table::Peer`]: every request
    ///   admitted, every answer and pushed snapshot filtered).
    /// - `pty`: `fleet::pty::serve` (a session attach or an exec in an
    ///   instance the peer reaches, its latest attach while it lasts);
    ///   refused without a helper client.
    /// - `tunnel`: `fleet::tunnel::serve` (a forward's bridge into one).
    /// - `transfer`: one transfer into the peer's fleet root
    ///   (`fleet::code::receivers`); refused without one.
    /// - `agent`: `fleet::agent::answer` with this host's agent while
    ///   `member.toml` grants it to the key the peer proved
    ///   ([`agent_for`]), else with none (its `NOT_SERVING` byte, then the
    ///   close). For a routed stream the peer is its origin, with the key
    ///   its opening proved: never the relay that carried it.
    /// - `route`: [`serve_route`](Self::serve_route), on a thread of its
    ///   own: relayed, or served as its origin's stream when it's for this
    ///   host.
    ///
    /// `false`, `stream` dropped: refused, or the daemon is stopping.
    fn serve_channel(self: &Arc<Self>, channel: lent::Channel, peer: &Arc<lent::Peer>, stream: Stream) -> bool {
        if *self.closed.0.lock().unwrap() {
            return false;
        }
        let peer = Arc::clone(peer);
        match channel {
            lent::Channel::Api => {
                self.spawn_conn_as(self.next_conn_id(), stream, Table::Peer(peer));
                true
            }
            lent::Channel::Pty => {
                let Some(helper) = self.helper.get().cloned() else { return false };
                let (files, exec, attaches) = (self.reach_files(), self.pty_exec, self.attaches.clone());
                self.spawn_channel("serve-pty", move || {
                    fleet::pty::serve(stream, &helper, &files, &peer, &attaches, crate::commands::exec::has_host_agent, exec, log)
                })
            }
            lent::Channel::Tunnel => {
                let files = self.reach_files();
                self.spawn_channel("serve-tunnel", move || fleet::tunnel::serve(stream, &files, &peer, log))
            }
            lent::Channel::Transfer => {
                let Some(root) = peer.fleet_root.clone() else { return false };
                // Each read and write bounded: a body flows, a pull's
                // answer comes once the puller checked the body.
                let bounded = [stream.set_read_timeout(Some(fleet::transfer::STREAM_WAIT)), stream.set_write_timeout(Some(fleet::transfer::STREAM_WAIT))];
                if bounded.iter().any(Result::is_err) {
                    return false;
                }
                let state = self.state.clone();
                // A state.toml that doesn't read: in use, so nothing is recut.
                let in_use: fleet::code::InUse =
                    Arc::new(move |at| crate::state::State::load_at(&state).map(|s| fleet::code::mounted(&s, at)).unwrap_or(true));
                let data = self.data_dir().map(Path::to_path_buf);
                self.spawn_channel("serve-transfer", move || {
                    let mut receivers = fleet::code::receivers(&root, fleet::transfer::Receivers::new(log), in_use);
                    if let Some(data) = &data {
                        receivers = fleet::mesh::serves(data, receivers);
                    }
                    if let Err(e) = receivers.receive(stream) {
                        log(&format!("fleet: a transfer from {} failed: {e:#}", peer.name));
                    }
                })
            }
            lent::Channel::Agent => {
                let member = self.member.clone();
                let host_agent = Arc::clone(&self.host_agent);
                self.spawn_channel("serve-agent", move || {
                    let agent = agent_for(member.as_deref(), &peer.name, peer.key.as_ref(), &host_agent);
                    fleet::agent::answer(stream, agent);
                })
            }
            lent::Channel::Route => {
                let shared = Arc::clone(self);
                self.spawn_channel("serve-route", move || shared.serve_route(&peer, stream))
            }
        }
    }

    /// A `ROUTE` stream link peer `from` opened (`fleet::route::serve`, as
    /// this host's identity, read now): a relay's, spliced to its next hop
    /// through [`route_next`](Self::route_next) on this thread, so it's
    /// counted while it lasts; relaying needs no trust (fleet plan decision
    /// 56). One for this host is served as one of its origin's streams on
    /// its inner channel ([`serve_channel`](Self::serve_channel)), under
    /// this host's trust table for the origin ([`fleet_peer`](Self::fleet_peer),
    /// no agent: decision 57): the handler gets one end of a socket pair,
    /// the stream's session (`fleet::session::pump`) runs between the other
    /// and the sealed stream on this thread, so it's counted while it lasts
    /// (decision 64).
    fn serve_route(self: &Arc<Self>, from: &lent::Peer, stream: Stream) {
        let identity = self
            .member
            .as_deref()
            .context("this daemon has no member.toml")
            .and_then(|m| fleet::proof::Identity::load(m, &self.hostname));
        let identity = match identity {
            Ok(identity) => identity,
            Err(e) => {
                fleet::route::refuse(&mut &stream, &self.hostname, &format!("{e:#}"));
                return log(&format!("fleet: refused a routed stream from {}: {e:#}", from.name));
            }
        };
        let here = fleet::route::Here { me: &identity.name, key: identity.signer_for(&identity.stored), stored: &identity.stored };
        let (routed, keys, stream) = match fleet::route::serve(stream, &from.name, &here, |next| self.route_next(next)) {
            Ok(Some(arrived)) => arrived,
            Ok(None) => return,
            Err(e) => return log(&format!("fleet: refused a routed stream from {}: {e:#}", from.name)),
        };
        let origin = routed.origin.as_str();
        let peer = self
            .data_dir()
            .context("this daemon has no data dir for a fleet root")
            .and_then(|data| self.fleet_peer(data, origin, routed.key, routed.introduced_by.clone(), None));
        let channel = routed.channel.as_str();
        let (plain, theirs) = match Stream::pair() {
            Ok(pair) => pair,
            Err(e) => return log(&format!("fleet: a routed stream from {origin}: cannot make its socket pair: {e}")),
        };
        match peer.map(|p| self.serve_channel(routed.channel, &Arc::new(p), theirs)) {
            Ok(true) => {
                // `AGENT` streams are mostly probes, one every `agent::PROBE`.
                if routed.channel != lent::Channel::Agent {
                    log(&format!("fleet: a routed {channel} stream from {origin} (via {})", from.name));
                }
                if let Err(e) = fleet::session::pump(plain, stream, keys) {
                    log(&format!("fleet: a routed {channel} stream from {origin}: {e:#}"));
                }
            }
            Ok(false) => log(&format!("fleet: refused a routed {channel} stream from {origin}")),
            Err(e) => log(&format!("fleet: a routed stream from {origin}: {e:#}")),
        }
    }

    /// A relay's next hop `next`: a `ROUTE` stream through its link's
    /// `fleet-<next>.sock`, when `next` is a machine here (registered or
    /// inbound) whose link is connected; else why not.
    fn route_next(&self, next: &str) -> Result<Stream, String> {
        let machine = self.links().and_then(|l| l.remotes().get(next));
        match machine {
            None => return Err(format!("`{next}` has no link to this host")),
            Some(m) if m.state != snapshots::LinkState::Connected => {
                let why = m.error.map(|e| format!(": {e}")).unwrap_or_default();
                return Err(format!("its link to `{next}` is {}{why}", m.state.as_str()));
            }
            Some(_) => {}
        }
        let socket = endpoint::fleet_socket(&self.socket_dir, next);
        fleet::link::connect(&socket, crate::devsbd::proto::channel::ROUTE)
            .map_err(|e| format!("its link to `{next}` isn't up ({}: {e})", socket.display()))
    }

    /// `name`'s link here (a machine registered or inbound) is connected.
    fn link_up(&self, name: &str) -> bool {
        self.links().and_then(|l| l.remotes().get(name)).is_some_and(|m| m.state == snapshots::LinkState::Connected)
    }

    /// The members this host binds a `route-<name>.sock` for
    /// (`fleet::routes::wanted`): its graph's, but itself and those it has
    /// a link up to. None outside a fleet's log.
    fn routed_members(&self) -> std::collections::BTreeSet<String> {
        match self.fleet_log() {
            Some((stored, me)) => fleet::routes::wanted(&self.graph.graph(), &stored, &me, |n| self.link_up(n)),
            None => Default::default(),
        }
    }

    /// The members this host binds an `agent-route-<name>.sock` for
    /// (`fleet::agent_routes::wanted`): those whose report grants it agent,
    /// but its inbound neighbors (their `fleet-link --peer` here binds
    /// `agent-link-<name>.sock`). None outside a fleet's log.
    fn agent_routed_members(&self) -> BTreeSet<String> {
        let Some((stored, me)) = self.fleet_log() else { return BTreeSet::new() };
        let inbound = self.machines.as_ref().map(|m| &m.inbound);
        fleet::agent_routes::wanted(&self.graph.graph(), &stored, &me, |n| inbound.is_some_and(|i| i.contains(n)))
    }

    /// This host's membership log (one with a genesis) and its member name.
    fn fleet_log(&self) -> Option<(fleet::membership::Stored, String)> {
        let member = self.member.as_deref()?;
        let stored = fleet::membership_api::load(member).ok().flatten()?;
        let me = fleet::dialer::member_name(member, &self.hostname).ok()?;
        stored.log.membership().is_some().then_some((stored, me))
    }

    /// The members this host's graph says `worker` buys little for
    /// (`fleet::graph::exposed_workers`), `me` overriding this host's own
    /// trust; none outside a fleet's log.
    fn exposed_workers(&self, me: Option<(&str, fleet::member::Trust)>) -> BTreeMap<String, Vec<String>> {
        match self.fleet_log() {
            Some((stored, _)) => fleet::graph::exposed_workers(&self.graph.graph(), &stored, me),
            None => BTreeMap::new(),
        }
    }

    /// The members this host grants its agent to now
    /// (`fleet::grants::load_granted`): the federated snapshots' `agent`.
    fn agent_granted(&self) -> BTreeSet<String> {
        self.member.as_deref().map(fleet::grants::load_granted).unwrap_or_default()
    }

    /// The routed machines' reach (`service::Hooks::routed`): each of
    /// [`routed_members`](Self::routed_members) the graph has a path to,
    /// with the first hop its streams take now, or why none
    /// (`fleet::routes::reach`). A member with no path at all (one that
    /// left, its last report stale) is no routed machine.
    fn routed_reach(&self) -> Vec<(String, Result<String, String>)> {
        let Some((stored, me)) = self.fleet_log() else { return Vec::new() };
        let graph = self.graph.graph();
        fleet::routes::wanted(&graph, &stored, &me, |n| self.link_up(n))
            .into_iter()
            .filter_map(|name| {
                let paths = graph.paths(&stored, &me, &name, fleet::graph::MAX_HOPS);
                if paths.is_empty() {
                    return None;
                }
                let reach = fleet::routes::reach(&name, &paths, |hop| self.link_up(hop));
                Some((name, reach))
            })
            .collect()
    }

    /// A connection to `dest`'s route socket (`fleet::routes`): served on
    /// a thread of its own, counted while it lasts ([`serve_routed`](Self::serve_routed)).
    fn accept_routed(self: &Arc<Self>, dest: &str, conn: Stream) {
        if *self.closed.0.lock().unwrap() {
            return;
        }
        let (shared, dest) = (Arc::clone(self), dest.to_string());
        self.spawn_channel("serve-routed", move || shared.serve_routed(&dest, conn));
    }

    /// `client`'s channel byte read, a routed stream to `dest` opened on it
    /// ([`open_routed`](Self::open_routed)) and its session pumped between
    /// `client` and it (`fleet::session::pump`, decision 64) until both
    /// ways end. None opening: logged, and an `API` client answered why
    /// (`fleet::routes::refuse_api`); any other closed.
    fn serve_routed(&self, dest: &str, client: Stream) {
        let Some(byte) = fleet::link::read_channel(&client) else { return };
        match self.open_routed(dest, byte) {
            Ok((stream, keys)) => {
                if let Err(e) = fleet::session::pump(client, stream, keys) {
                    log(&format!("fleet: a routed stream to {dest}: {e:#}"));
                }
            }
            Err(why) => {
                // `AGENT` streams are mostly probes (`fleet::agent_routes`).
                if byte != crate::devsbd::proto::channel::AGENT {
                    log(&format!("fleet: a routed stream to {dest}: {why}"));
                }
                if byte == crate::devsbd::proto::channel::API {
                    fleet::routes::refuse_api(&client, &why);
                }
            }
        }
    }

    /// A routed stream to `dest` on channel `byte`, from this host's
    /// identity (read now; a pending rotation's key as the link proof
    /// picks it, `Identity::signer_for`): its graph's paths in order
    /// (`fleet::routes::first_open`), skipping those whose first hop has
    /// no link up here, each attempt within `fleet::routes::ATTEMPT` and
    /// all within [`route_budget`](Self::route_budget): the stream and its
    /// session's keys. Else why not.
    fn open_routed(&self, dest: &str, byte: u8) -> Result<(Stream, fleet::session::Keys), String> {
        let channel = fleet::route::channel_of(byte).ok_or_else(|| format!("no channel {byte}"))?;
        let identity = self
            .member
            .as_deref()
            .context("this daemon has no member.toml")
            .and_then(|m| fleet::proof::Identity::load(m, &self.hostname))
            .map_err(|e| format!("{e:#}"))?;
        let me = identity.name.as_str();
        // The channel and the names, checked once rather than per path.
        fleet::route::Header::new(me, channel, vec![dest.to_string()]).check().map_err(|e| format!("{e:#}"))?;
        let stored = &identity.stored;
        let key = identity.signer_for(stored);
        let paths = self.graph.graph().paths(stored, me, dest, fleet::graph::MAX_HOPS);
        let deadline = Instant::now() + self.route_budget;
        let open = |path: &[String]| {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                anyhow::bail!("out of the {:?} its paths share", self.route_budget);
            }
            let header = fleet::route::Header::new(me, channel, path.to_vec());
            fleet::route::open(&self.socket_dir, &header, key, stored, left.min(fleet::routes::ATTEMPT))
        };
        fleet::routes::first_open(&paths, |hop| self.link_up(hop), open).map_err(|failed| fleet::routes::failure(dest, &failed))
    }

    /// `serve` on a thread named `name`, counted in [`channels`](Self::channels)
    /// until it returns; `false` when the thread didn't start.
    fn spawn_channel(&self, name: &str, serve: impl FnOnce() + Send + 'static) -> bool {
        struct Held(Arc<AtomicUsize>);
        impl Drop for Held {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.channels.fetch_add(1, Ordering::SeqCst);
        let held = Held(Arc::clone(&self.channels));
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _held = held;
                serve();
            })
            .is_ok()
    }

    /// Register before the thread starts, so the next holder snapshot counts
    /// it even if the client is gone by then.
    fn spawn_conn_as(self: &Arc<Self>, id: u64, stream: Stream, table: Table) {
        let (Ok(clone), Ok(writer), Ok(ctl)) = (stream.try_clone(), stream.try_clone(), stream.try_clone()) else {
            return;
        };
        // A socket option: covers every clone, responses and notifications.
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let out = Arc::new(Outbox::new(writer, ctl));
        if let Table::Peer(peer) = &table {
            let reach = (Arc::clone(peer), self.reach_files());
            out.post(|p| {
                p.reach = Some(reach);
                false
            });
        }
        let attach_only = Arc::new(AtomicBool::new(false));
        self.conns.lock().unwrap().insert(id, ConnEntry { stream: clone, out: Arc::clone(&out), attach_only: Arc::clone(&attach_only) });
        let shared = Arc::clone(self);
        let spawned = std::thread::Builder::new().name(format!("serve-conn-{id}")).spawn(move || {
            let handoff = serve_conn(id, stream, &shared, out, table, attach_only);
            shared.conns.lock().unwrap().remove(&id);
            *shared.idle_since.lock().unwrap() = Instant::now();
            // A `link.attach {channel}`: out of `conns` (the drain no longer
            // cuts it), its notifier gone; the stream is the channel's now.
            if let Some(Handoff { channel, peer, stream }) = handoff {
                let _ = stream.set_write_timeout(None);
                if !shared.serve_channel(channel, &peer, stream) {
                    log(&format!("connection {id}: refused a {} stream from {}", channel.as_str(), peer.name));
                }
            }
        });
        if spawned.is_err() {
            self.conns.lock().unwrap().remove(&id);
        }
    }

    fn close(&self) {
        let (lock, cvar) = &self.closed;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    /// Ask the accept loop to exit with `exit` (a handoff or shutdown) and
    /// wait (bounded) until it has closed the listener; see the comment at
    /// the end of [`run`].
    fn begin_stop(&self, exit: Exit) {
        self.stop.lock().unwrap().get_or_insert(exit);
        let (lock, cvar) = &self.closed;
        let guard = lock.lock().unwrap();
        let _ = cvar.wait_timeout_while(guard, Duration::from_secs(2), |closed| !*closed);
    }

    /// Flag a notification on every connection subscribed to `topic`.
    fn publish(&self, topic: Topic, set: impl Fn(&mut Pending)) {
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                let subscribed = p.topics.contains(&topic);
                if subscribed {
                    set(p);
                }
                subscribed
            });
        }
    }

    /// `inbox.shown`: a container message's status line, to `inbox`
    /// subscribers. Latest wins: under backpressure older lines are dropped.
    fn publish_shown(&self, instance: &str, line: String) {
        self.publish(Topic::Inbox, |p| p.shown = Some((instance.to_string(), line.clone())));
    }

    /// `opens.done`, to `opens` subscribers: the last [`OPENS_KEPT`] of
    /// them, so a burst of opens isn't one line.
    fn publish_open(&self, done: &crate::open::OpenDone) {
        let params = serde_json::to_value(opens::DoneParams::new(done)).unwrap_or_default();
        self.publish(Topic::Opens, |p| p.push_open(params.clone()));
    }

    /// `instances.snapshot` of root `dir`, to the connections watching it:
    /// `params` as is, or merged with the machines' rows for a `machines:
    /// true` watcher (merged once per asking peer: a link peer's own rows
    /// are left out of its push, `Pending::except`). Latest wins per root:
    /// under backpressure older snapshots are dropped.
    fn publish_snapshot(&self, dir: &Path, params: Arc<Value>) {
        self.locals.lock().unwrap().insert(dir.to_path_buf(), Arc::clone(&params));
        let mut merged: BTreeMap<Option<String>, Arc<Value>> = BTreeMap::new();
        let mut remotes: Option<(Vec<snapshots::Machine>, BTreeMap<String, Vec<String>>, BTreeSet<String>)> = None;
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                let push = match p.watched.get(dir) {
                    None => return false,
                    Some(false) => Arc::clone(&params),
                    Some(true) => {
                        let except = p.except();
                        let push = merged.entry(except.clone()).or_insert_with(|| {
                            let (remotes, exposed, granted) =
                                remotes.get_or_insert_with(|| (self.remotes(), self.exposed_workers(None), self.agent_granted()));
                            Arc::new(snapshots::merge(&params, remotes, except.as_deref(), exposed, granted))
                        });
                        Arc::clone(push)
                    }
                };
                p.snapshots.insert(dir.to_path_buf(), push);
                true
            });
        }
    }

    /// A machine's rows or state changed: every root watched with `machines:
    /// true` gets its last local params merged again, to those watchers
    /// only. A root with no local params yet waits for its first.
    /// One at a time: two at once could post in the other order, the older
    /// remotes replacing the newer under latest-wins, and with no poll
    /// nothing re-merges until the next change.
    fn publish_machines(&self) {
        let _one = self.publishing.lock().unwrap();
        // `locals` before the remotes: a root `watch_root` adds after this
        // clone merged its first push with remotes (and grants) read after
        // the change that called this one.
        let locals = self.locals.lock().unwrap().clone();
        let remotes = self.remotes();
        let exposed = self.exposed_workers(None);
        let granted = self.agent_granted();
        let mut merged: BTreeMap<(PathBuf, Option<String>), Arc<Value>> = BTreeMap::new();
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                let mut any = false;
                let except = p.except();
                for (dir, _) in p.watched.iter().filter(|(_, machines)| **machines) {
                    let Some(local) = locals.get(dir) else { continue };
                    let push = merged
                        .entry((dir.clone(), except.clone()))
                        .or_insert_with(|| Arc::new(snapshots::merge(local, &remotes, except.as_deref(), &exposed, &granted)));
                    p.snapshots.insert(dir.clone(), Arc::clone(push));
                    any = true;
                }
                any
            });
        }
    }

    /// Every root some live connection watches: what the host poll collects.
    /// A connection that hung up is gone from `conns`, its watches with it
    /// (and the root's local params, once nobody watches it). `locals` is
    /// held from the collection to the prune, so a [`watch_root`] (which
    /// adds its root's params and watch under it) is seen whole or not at
    /// all, never pruned between the two.
    fn watched_roots(&self) -> BTreeSet<PathBuf> {
        let mut locals = self.locals.lock().unwrap();
        let mut roots = BTreeSet::new();
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                roots.extend(p.watched.keys().cloned());
                false
            });
        }
        locals.retain(|dir, _| roots.contains(dir));
        roots
    }

    /// Whether a live connection watches some root with `machines: true`:
    /// the machine links' want (docs/architecture/daemon/machines.md, *Held
    /// links: `Machines`*).
    fn watching_machines(&self) -> bool {
        let mut any = false;
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                any |= p.watched.values().any(|m| *m);
                false
            });
        }
        any
    }

    /// The registry's events, to `forwards` subscribers: `forwards.changed`,
    /// and `forwards.status` lines, latest wins like `inbox.shown`.
    fn publish_forwards(&self, event: forwards::Event) {
        match event {
            forwards::Event::Changed => self.publish(Topic::Forwards, |p| p.forwards = true),
            forwards::Event::Status(line) => self.publish(Topic::Forwards, |p| p.forwards_status = Some(line.clone())),
        }
    }

    /// The reconnect hint: tell every subscriber the daemon is going away
    /// (`reason`: `handoff`, `shutdown` or `idle`), before the drain closes them.
    fn publish_closing(&self, reason: &'static str) {
        for conn in self.conns.lock().unwrap().values() {
            conn.out.post(|p| {
                let subscribed = !p.topics.is_empty();
                if subscribed {
                    p.closing = Some(reason);
                }
                subscribed
            });
        }
    }

    /// A verb's connection ends here too, leaving its verb running (the
    /// [`draining`](Self::draining) flag), so verbs never hold the drain;
    /// an attach-only one is cut and never waited on ([`ConnEntry::holds`]).
    fn drain(&self, timeout: Duration) {
        self.draining.store(true, Ordering::SeqCst);
        for conn in self.conns.lock().unwrap().values() {
            let _ = conn.stream.shutdown(Shutdown::Read);
        }
        let deadline = Instant::now() + timeout;
        while self.conns.lock().unwrap().values().any(ConnEntry::holds) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The `opens.done` notifications a connection keeps while its notifier
/// hasn't written them: opens are rare, but two in a burst are two lines,
/// and a client that stops reading still costs a bounded few.
const OPENS_KEPT: usize = 8;

/// The daemon's `open::OnDone`: each outcome to `opens` subscribers
/// ([`Shared::publish_open`]). Holds `shared` weakly: it ends up inside
/// [`Shared::open_here`], and a strong one would keep a stopped daemon's
/// `Shared` alive forever.
fn open_done(shared: &Arc<Shared>) -> crate::open::OnDone {
    let shared = Arc::downgrade(shared);
    Arc::new(move |done: crate::open::OpenDone| {
        if let Some(shared) = shared.upgrade() {
            shared.publish_open(&done);
        }
    })
}

struct ConnEntry {
    /// For the drain's shutdown.
    stream: Stream,
    out: Arc<Outbox>,
    /// Set while the connection's only business is `instances.attach`
    /// ([`ConnState::attach_only`]).
    attach_only: Arc<AtomicBool>,
}

impl ConnEntry {
    /// Whether it holds the daemon: a `clients` holder, waited on by the
    /// drain. An attach-only one doesn't: an `exec` left open in a
    /// terminal for days mustn't keep the daemon up, nor a handoff waiting
    /// (docs/architecture/daemon/role.md, *Holders and idle exit*).
    fn holds(&self) -> bool {
        !self.attach_only.load(Ordering::SeqCst)
    }
}

/// Everything written to one connection goes through here: the writer lock
/// serializes responses (from the connection thread) with notifications
/// (from its notifier thread), and `pending` holds the flagged
/// notifications the notifier hasn't written yet.
struct Outbox {
    writer: Mutex<Stream>,
    /// Unlocked clone, to cut the connection while a write holds `writer`.
    ctl: Stream,
    pending: Mutex<Pending>,
    wake: Condvar,
}

/// A connection's subscriptions and flagged notifications. Flags, not a
/// queue: notifications are coarse ("re-fetch"), so any number of changes
/// before the notifier runs is one line per topic.
#[derive(Default)]
struct Pending {
    topics: BTreeSet<Topic>,
    /// `inbox.changed` with this generation.
    inbox: Option<u64>,
    /// `inbox.shown` with `(instance, line)`, the latest only.
    shown: Option<(String, String)>,
    instances: bool,
    /// The roots this connection watches (`instances.watch`), each with its
    /// `machines` flag (federated snapshots); only while subscribed to
    /// `instances`.
    watched: BTreeMap<PathBuf, bool>,
    /// `instances.snapshot` per root, the latest only.
    snapshots: BTreeMap<PathBuf, Arc<Value>>,
    /// `forwards.changed`.
    forwards: bool,
    /// `forwards.status` with this line, the latest only.
    forwards_status: Option<String>,
    /// `opens.done` params, oldest first, the last [`OPENS_KEPT`].
    opens: Vec<Value>,
    /// `closing` with this reason; written last.
    closing: Option<&'static str>,
    /// The connection holds a `links.inbound` announce (a `fleet-link
    /// --peer`'s) of this neighbor: [`Shared::set_hub`] cycles it, as
    /// [`Shared::end_inbound`] does for its name.
    inbound: Option<String>,
    /// `links.cycle`, with why when it isn't this host's hub changing: the
    /// `fleet-link` hangs up.
    cycle: Option<Option<String>>,
    /// A `Peer` connection's peer, and where its reach is read from: its
    /// snapshots are cut to that reach, read per push
    /// (`lent::filter_snapshot`), by the notifier.
    reach: Option<(Arc<lent::Peer>, lent::Files)>,
    /// The connection ended: the notifier writes what's left and exits.
    closed: bool,
}

impl Pending {
    /// The machine a federated push to this connection leaves out: a link
    /// peer's own (its rows are never re-exported to it).
    fn except(&self) -> Option<String> {
        self.reach.as_ref().map(|(peer, _)| peer.name.clone())
    }

    /// One more `opens.done`, the oldest dropped past [`OPENS_KEPT`].
    fn push_open(&mut self, params: Value) {
        if self.opens.len() == OPENS_KEPT {
            self.opens.remove(0);
        }
        self.opens.push(params);
    }

    fn any(&self) -> bool {
        self.inbox.is_some()
            || self.shown.is_some()
            || self.instances
            || !self.snapshots.is_empty()
            || self.forwards
            || self.forwards_status.is_some()
            || !self.opens.is_empty()
            || self.closing.is_some()
            || self.cycle.is_some()
    }

    fn take(&mut self) -> Vec<Notification> {
        let mut out = Vec::new();
        if let Some(generation) = self.inbox.take() {
            out.push(Notification { method: "inbox.changed".into(), params: json!({ "generation": generation }) });
        }
        if let Some((instance, line)) = self.shown.take() {
            out.push(Notification { method: "inbox.shown".into(), params: json!({ "instance": instance, "line": line }) });
        }
        if std::mem::take(&mut self.instances) {
            out.push(Notification { method: "instances.changed".into(), params: json!({}) });
        }
        for params in std::mem::take(&mut self.snapshots).into_values() {
            out.push(Notification { method: "instances.snapshot".into(), params: Arc::unwrap_or_clone(params) });
        }
        if std::mem::take(&mut self.forwards) {
            out.push(Notification { method: "forwards.changed".into(), params: json!({}) });
        }
        if let Some(line) = self.forwards_status.take() {
            out.push(Notification { method: "forwards.status".into(), params: json!({ "line": line }) });
        }
        for params in std::mem::take(&mut self.opens) {
            out.push(Notification { method: "opens.done".into(), params });
        }
        if let Some(why) = self.cycle.take() {
            let params = match why {
                Some(why) => json!({ "why": why }),
                None => json!({}),
            };
            out.push(Notification { method: crate::commands::fleet_link::LINKS_CYCLE.into(), params });
        }
        if let Some(reason) = self.closing.take() {
            out.push(Notification { method: "closing".into(), params: json!({ "reason": reason }) });
        }
        out
    }
}

impl Outbox {
    fn new(writer: Stream, ctl: Stream) -> Self {
        Self { writer: Mutex::new(writer), ctl, pending: Mutex::new(Pending::default()), wake: Condvar::new() }
    }

    /// One JSON line, whole, under the writer lock.
    fn send(&self, msg: &impl serde::Serialize) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(&line)?;
        writer.flush()
    }

    /// Change `pending` under its lock; `f` says whether to wake the
    /// notifier.
    fn post(&self, f: impl FnOnce(&mut Pending) -> bool) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if f(&mut pending) {
            self.wake.notify_all();
        }
    }
}

/// A connection's notifier thread: write what's flagged, until the
/// connection ends. A failed (or timed-out) write cuts the connection, which
/// ends its reader too.
fn notifier(out: &Outbox) {
    loop {
        let (mut notes, closed, reach) = {
            let mut p = out.pending.lock().unwrap_or_else(|e| e.into_inner());
            while !p.any() && !p.closed {
                p = out.wake.wait(p).unwrap_or_else(|e| e.into_inner());
            }
            (p.take(), p.closed, p.reach.clone())
        };
        // A peer's snapshots are cut outside the lock: the reach reads files.
        if let Some((peer, files)) = reach
            && notes.iter().any(|n| n.method == "instances.snapshot")
        {
            let (reach, _) = lent::Reach::load(&files, peer.fleet_root.as_deref());
            for n in notes.iter_mut().filter(|n| n.method == "instances.snapshot") {
                lent::filter_snapshot(&mut n.params, &reach);
            }
        }
        for note in &notes {
            if out.send(note).is_err() {
                let _ = out.ctl.shutdown(Shutdown::Both);
                return;
            }
        }
        if closed {
            return;
        }
    }
}

/// The daemon-wide change watcher (see the module doc). Dropping it stops
/// and joins the thread (within [`WATCH`]).
struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    fn spawn(shared: &Arc<Shared>, instances: Option<Arc<AtomicU64>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (shared, stop) = (Arc::clone(shared), Arc::clone(&stop));
            std::thread::Builder::new().name("serve-watch".into()).spawn(move || watch(&shared, instances, &stop))
        };
        let thread = thread.map_err(|e| log(&format!("cannot start the notification thread: {e}"))).ok();
        Self { stop, thread }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `instances.changed` covers both the host's container poll (`instances`)
/// and `state.toml` writes (phases, new/removed/renamed instances, done
/// flags), stamped like the store each tick, so a build's phase reaches a
/// subscriber within [`WATCH`]. Config files are stamped too
/// ([`config_stamps`]): an edit wakes the poll, which re-reads them (holders,
/// snapshots), since no container event announces one.
fn watch(shared: &Shared, instances: Option<Arc<AtomicU64>>, stop: &AtomicBool) {
    let mut seen = ops::generation();
    let mut stamp = ops::stamp(&shared.inbox);
    let mut state_stamp = crate::inbox::store::stamp_at(&shared.state);
    let mut recorded = recorded_roots(&shared.state);
    let mut configs = config_stamps(recorded.iter().chain(&shared.watched_roots()));
    let instances_now = || instances.as_ref().map_or(0, |c| c.load(Ordering::Acquire));
    let mut instances_seen = instances_now();
    while !stop.load(Ordering::SeqCst) {
        let generation = ops::wait_changed(seen, WATCH);
        let now = ops::stamp(&shared.inbox);
        if generation != seen || now != stamp {
            (seen, stamp) = (generation, now);
            let g = shared.inbox_generation.fetch_add(1, Ordering::SeqCst) + 1;
            shared.publish(Topic::Inbox, |p| p.inbox = Some(g));
        }
        let n = instances_now();
        let state_now = crate::inbox::store::stamp_at(&shared.state);
        if n != instances_seen || state_now != state_stamp {
            // A phase moved, an instance came or went: the watched roots'
            // snapshots follow now, not on the poll's next tick.
            if state_now != state_stamp
                && let Some(poll) = shared.bridges.get()
                && !shared.watched_roots().is_empty()
            {
                poll.refresh();
            }
            if state_now != state_stamp {
                recorded = recorded_roots(&shared.state);
            }
            (instances_seen, state_stamp) = (n, state_now);
            shared.publish(Topic::Instances, |p| p.instances = true);
        }
        let configs_now = config_stamps(recorded.iter().chain(&shared.watched_roots()));
        if configs_now != configs {
            // A root merely added (a new instance, a new watch) gets its pass
            // from the state write or the watch itself; only a stamp that
            // moved under a root already known is an edit.
            let edited = configs_now.iter().any(|(root, s)| configs.get(root).is_some_and(|old| old != s));
            if edited && let Some(poll) = shared.bridges.get() {
                poll.refresh();
            }
            configs = configs_now;
        }
    }
}

/// The config roots `state.toml` records (empty if it won't load: the next
/// write retries).
fn recorded_roots(state: &Path) -> BTreeSet<PathBuf> {
    crate::state::State::load_at(state).map(|s| super::host::config_roots(&s).into_iter().collect()).unwrap_or_default()
}

/// Each root's config candidates' stamps ([`crate::config::candidates`]):
/// one `stat` per path, cheap enough for every [`WATCH`] tick.
fn config_stamps<'a>(roots: impl Iterator<Item = &'a PathBuf>) -> ConfigStamps {
    roots
        .map(|root| {
            let stamps = crate::config::candidates(root).map(|p| crate::inbox::store::stamp_at(&p)).collect();
            (root.clone(), stamps)
        })
        .collect()
}

type ConfigStamps = BTreeMap<PathBuf, Vec<Option<(std::time::SystemTime, u64)>>>;

/// The agent a peer's `AGENT` stream gets ([`Shared::serve_channel`]):
/// `host_agent`'s, while `member`'s `[[agent]]` grants member `peer` the
/// key it proved (`fleet::grants::granted`, against the membership log
/// beside it, both read per stream); none otherwise: no key proven, or a
/// member file, log or grant missing or not loading.
fn agent_for(member: Option<&Path>, peer: &str, key: Option<&fleet::identity::PublicKey>, host_agent: &HostAgent) -> Option<PathBuf> {
    let (member, key) = (member?, key?);
    let grants = fleet::member::load(member).ok()??.agent;
    let stored = fleet::membership_api::load(member).ok()??;
    fleet::grants::granted(&grants, &stored, peer, key).then(|| host_agent()).flatten()
}

/// A connection a `link.attach {channel}` turned into that channel's
/// stream: what its thread hands [`Shared::serve_channel`] once the attach
/// is answered.
struct Handoff {
    channel: lent::Channel,
    peer: Arc<lent::Peer>,
    stream: Stream,
}

/// What a connection may call.
enum Table {
    /// A local client: the whole API.
    Full,
    /// A remote caller (a machine over its link, or a `fleet-link`
    /// connection after its `link.attach`): the trust table (`fleet::lent`).
    Peer(Arc<lent::Peer>),
}

/// A connection's own state, on its thread.
struct ConnState {
    id: u64,
    table: Table,
    /// The peer runs as this daemon's uid (`endpoint::peer_is_own_user`):
    /// what a forced hello needs, besides the `Full` table.
    own_user: bool,
    /// The hello's `client` (`tui`, `cli`, `api:<name>`), for the log.
    client: String,
    /// What the store records as the client of this connection's user ops
    /// (`api::audit_client` of `client`).
    audit: String,
    out: Arc<Outbox>,
    notifier: Option<JoinHandle<()>>,
    /// The reader held bytes past the request line being handled: a
    /// `link.attach {channel}` is then refused rather than lose them.
    buffered: bool,
    /// Set by a `link.attach {channel}` (not `api`): the loop ends once its
    /// answer is out and the stream goes to that channel.
    handoff: Option<(lent::Channel, Arc<lent::Peer>)>,
    /// `links.inbound`'s announce: its neighbor is up while this
    /// connection is (a repeat replaces it).
    inbound: Option<machines::inbound::Guard>,
    /// `instances.attach`'s records, each held until the connection ends.
    attaches: Vec<super::attaches::Guard>,
    /// Set (with [`ConnEntry`]'s copy) once an `instances.attach` is taken on
    /// a connection that did nothing else but say hello; any other request
    /// clears it for good ([`ConnState::busy`]).
    attach_only: Arc<AtomicBool>,
    /// A request other than `hello` and `instances.attach` came: never
    /// attach-only again.
    busy: bool,
    /// What the links do once this request's answer is out
    /// (`machines::leave::After`): a `fleet.*` call whose change ends the
    /// link it came on.
    after: Option<machines::leave::After>,
}

impl ConnState {
    fn label(&self) -> String {
        match self.client.as_str() {
            "" => format!("connection {}", self.id),
            client => format!("connection {} ({client})", self.id),
        }
    }
}

/// One connection: a request per line, a response per request, until EOF,
/// or until a `link.attach {channel}` is answered: then the stream, with
/// nothing read past the attach line, is returned for that channel.
fn serve_conn(
    id: u64,
    stream: Stream,
    shared: &Shared,
    out: Arc<Outbox>,
    table: Table,
    attach_only: Arc<AtomicBool>,
) -> Option<Handoff> {
    let mut conn = ConnState {
        id,
        own_user: endpoint::peer_is_own_user(&stream),
        table,
        client: String::new(),
        audit: api::audit_client(""),
        out,
        notifier: None,
        buffered: false,
        handoff: None,
        inbound: None,
        attaches: Vec::new(),
        attach_only,
        busy: false,
        after: None,
    };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(proto::MAX_LINE).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        conn.buffered = !reader.buffer().is_empty();
        let too_long = !line.ends_with(b"\n") && line.len() as u64 >= proto::MAX_LINE;
        let response = if too_long {
            Response::err(None, "invalid", "request line too long")
        } else if line.trim_ascii().is_empty() {
            continue;
        } else {
            match handle(&line, shared, &mut conn) {
                Some(response) => response,
                // A verb's client hung up (or the daemon drains): no answer.
                None => break,
            }
        };
        let sent = conn.out.send(&response);
        if let Some(after) = conn.after.take() {
            // The answer goes back over the link this ends: let it leave.
            std::thread::sleep(AFTER_ANSWER);
            if let Some(ctx) = shared.machines.as_ref() {
                machines::leave::apply(after, ctx);
            }
        }
        if sent.is_err() || too_long {
            conn.handoff = None;
            break;
        }
        if conn.handoff.is_some() {
            break;
        }
    }
    conn.out.post(|p| {
        p.closed = true;
        true
    });
    if let Some(t) = conn.notifier.take() {
        let _ = t.join();
    }
    let (channel, peer) = conn.handoff.take()?;
    Some(Handoff { channel, peer, stream: reader.into_inner() })
}

fn handle(line: &[u8], shared: &Shared, conn: &mut ConnState) -> Option<Response> {
    let mut req: Request = match serde_json::from_slice(line) {
        Ok(req) => req,
        Err(e) => return Some(Response::err(None, "invalid", format!("not a request: {e}"))),
    };
    if !matches!(req.method.as_str(), "hello" | "instances.attach") {
        conn.busy = true;
        conn.attach_only.store(false, Ordering::SeqCst);
    }
    // By who the peer is, not by the trust table.
    if req.method.starts_with("fleet.") {
        return Some(fleet_call(req, shared, conn));
    }
    // A remote caller: the trust table, its params confined, before anything
    // else looks at the request.
    let reach = match &conn.table {
        Table::Peer(peer) => {
            let (reach, warning) = lent::Reach::load(&shared.reach_files(), peer.fleet_root.as_deref());
            if let Some(w) = warning {
                log(&format!("{}: {w}", conn.label()));
            }
            let admitted = lent::admit(&req.method, std::mem::take(&mut req.params), peer, &reach, &shared.state);
            match admitted {
                Ok(params) => req.params = params,
                Err(e) => {
                    if e.code == "denied" {
                        log(&format!("{}: refused {} from {}: {}", conn.label(), req.method, peer.name, e.message));
                    }
                    return Some(Response::err(req.id, e.code, e.message));
                }
            }
            Some(reach)
        }
        Table::Full => None,
    };
    match req.method.as_str() {
        "hello" => return Some(hello(req.id, req.params, shared, conn)),
        method if verbs::is_verb(method) => return verb(req.id, method, req.params, shared, conn),
        "helper.call" => return helper_call(req.id, req.params, shared, conn),
        // Only reached on `Full`: the trust table refuses `link.*`.
        "link.attach" => return Some(link_attach(req.id, &req.params, shared, conn)),
        "links.inbound" => return Some(links_inbound(req.id, req.params, shared, conn)),
        // Before `machine` routing: its `machine` is where this host attaches.
        "instances.attach" => return Some(instances_attach(req.id, req.params, shared, conn)),
        // A peer's only: never routed by a `machine` (it refuses one).
        "open.deliver" => return Some(open_deliver(req.id, req.params, shared, conn)),
        _ => {}
    }
    // On a machine: the remote's answer as is
    // (docs/architecture/daemon/machines.md, *Forwarded requests*).
    match fwd::machine_of(&req.params) {
        Err(e) => return Some(Response::err(req.id, e.code, e.message)),
        Ok(Some(machine)) if req.method == "instances.done" => {
            return Some(remote_done(req.id, req.params, &machine, shared));
        }
        Ok(Some(machine)) if fwd::is_quick(&req.method) => {
            return Some(forward(req.id, &req.method, req.params, &machine, shared));
        }
        // Served here, `machine` kept.
        Ok(Some(_)) if fwd::is_here(&req.method) => {}
        Ok(Some(_)) => {
            return Some(Response::err(req.id, "invalid", format!("`{}` can't take a machine yet", req.method)));
        }
        Ok(None) => {}
    }
    // A peer's `roots.list` listing (not a `paths` check) gets its fleet root.
    let lists_roots = req.method == "roots.list" && req.params.get("paths").is_none_or(Value::is_null);
    let answer = match req.method.as_str() {
        "subscribe" => subscribe(req.params, conn, true),
        "unsubscribe" => subscribe(req.params, conn, false),
        "instances.watch" => watch_root(req.params, shared, conn),
        "instances.unwatch" => unwatch_root(req.params, conn),
        // This host and every machine.
        "instances.resolve" if fwd::federated(&req.params) => {
            let local = local_call(&req.method, req.params.clone(), shared, conn);
            fwd::resolve_everywhere(&req.params, local, shared.links())
        }
        "sandboxes.list" if fwd::federated(&req.params) => {
            let local = local_call(&req.method, req.params, shared, conn);
            fwd::sandboxes_everywhere(local, shared.links())
        }
        "shutdown" => {
            log(&format!("{}: shutdown requested", conn.label()));
            shared.begin_stop(Exit::Shutdown);
            Ok(json!({ "ok": true }))
        }
        "capacity.get" => serde_json::to_value(shared.capacity()).map_err(|e| api::ApiError::internal(e.into())),
        // About the caller's own fleet root here, so only a peer has one.
        "code.haves" => match &conn.table {
            Table::Peer(peer) => match peer.fleet_root.as_deref() {
                Some(root) => fleet::code::haves_call(root, &req.params),
                None => Err(api::ApiError::denied("this host keeps no fleet root for the caller")),
            },
            Table::Full => Err(api::ApiError::denied("`code.haves` answers a link peer about its fleet root here: a local client has none")),
        },
        // A peer fetching back an instance it placed (`lent` admitted it:
        // under its fleet root, or any under `full`); the bundle goes
        // under its fleet root.
        "instances.fetch" => match &conn.table {
            Table::Peer(peer) => match peer.fleet_root.as_deref() {
                Some(root) => crate::state::State::load_at(&shared.state)
                    .map_err(api::ApiError::internal)
                    .and_then(|state| fleet::code::fetch::fetch_call(&state, root, &req.params)),
                None => Err(api::ApiError::denied("this host keeps no fleet root for the caller")),
            },
            Table::Full => Err(api::ApiError::denied(
                "`instances.fetch` answers a link peer, under its fleet root here: a local client fetches back with `placements.fetch`",
            )),
        },
        // Local only (`lent` refuses `placements.*` to every peer).
        "placements.fetch" => placements_fetch(&req.params, shared),
        // Local only: no link's table admits `member.*` (`fleet::lent`).
        method if method.starts_with("member.") => match &shared.member {
            Some(path) => {
                let trust = |p: &Path| fleet::member::load(p).ok().map(Option::unwrap_or_default).map(|m| m.trust);
                let before = trust(path);
                let answer = fleet::member::call(method, req.params, path, &shared.hostname, &|name, trust| {
                    shared.exposed_workers(Some((name, trust))).remove(name)
                })
                .unwrap_or_else(|| Err(api::ApiError::unknown_method(method)));
                // A changed trust is in this host's link report at once,
                // so the other members' exposure of it follows.
                if answer.is_ok() && trust(path) != before {
                    shared.graph.republish();
                }
                answer
            }
            None => Err(api::ApiError::failed("this daemon has no member.toml (no data dir)")),
        },
        // Local only (`lent::admit` refuses `graph.*`): the reports held here.
        "graph.get" => {
            let stored = match shared.member.as_deref() {
                Some(path) => fleet::membership_api::load(path),
                None => Ok(None),
            };
            stored.and_then(|s| serde_json::to_value(shared.graph.reports(s.as_ref())).map_err(|e| api::ApiError::internal(e.into())))
        }
        // Local only too: the proposals queued here, approved over the links.
        "membership.pending" | "membership.approve" | "membership.deny" | "membership.rotate" | "membership.hub" if shared.machines.is_some() => {
            let ctx = shared.machines.as_ref().expect("checked");
            machines::changes::call(&req.method, req.params, ctx).expect("a membership.pending|approve|deny|rotate|hub method")
        }
        // Local only too; an adopt wrote `hub` from the log and may have
        // promoted this host's next key: both followed here.
        method if method.starts_with("membership.") => match &shared.member {
            Some(path) => match fleet::membership_api::call(method, req.params, path, &shared.hostname) {
                Some((answer, follow)) => {
                    if let Some(follow) = follow {
                        shared.set_hub(follow.hub);
                        if follow.rotated {
                            shared.key_rotated();
                        }
                    }
                    answer
                }
                None => Err(api::ApiError::unknown_method(method)),
            },
            None => Err(api::ApiError::failed("this daemon has no member.toml (no data dir)")),
        },
        method if method.starts_with("machines.") && shared.machines.is_some() => {
            let ctx = shared.machines.as_ref().expect("checked");
            machines::api::call(method, req.params, ctx).unwrap_or_else(|| Err(api::ApiError::unknown_method(method)))
        }
        method => local_call(method, req.params, shared, conn),
    };
    let answer = match &reach {
        Some(reach) if lists_roots => answer.map(|v| lent::with_fleet_root(v, reach)),
        Some(reach) => answer.and_then(|v| lent::filter_answer(&req.method, v, reach, &shared.state)),
        None => answer,
    };
    Some(match answer {
        Ok(result) => Response::ok(req.id, result),
        Err(e) => {
            if e.code == "internal" {
                log(&format!("{}: {}: {}", conn.label(), req.method, e.message));
            }
            Response::err(req.id, e.code, e.message)
        }
    })
}

/// How long a [`ConnState::after`] waits once its answer is written.
const AFTER_ANSWER: Duration = Duration::from_millis(300);

/// `fleet.*` (docs/reference/api.md, *fleet*): admitted by the key the peer
/// proved (`lent::admit_fleet` against this host's membership log, read
/// per call), never from a local client, then run by
/// `machines::leave::fleet_call` (`fleet.place` by [`fleet_place`]); what
/// it ends of the links waits for the answer ([`ConnState::after`]).
fn fleet_call(req: Request, shared: &Shared, conn: &mut ConnState) -> Response {
    let peer = match &conn.table {
        Table::Peer(peer) => Some(Arc::clone(peer)),
        Table::Full => None,
    };
    let from = peer.as_ref().map(|p| p.name.clone());
    let ctx = shared.machines.as_ref();
    let own = shared.member.as_deref().and_then(|p| fleet::dialer::member_name(p, &shared.hostname).ok());
    let stored = match shared.member.as_deref().map(fleet::membership_api::load) {
        Some(Ok(stored)) => stored,
        Some(Err(e)) => {
            log(&format!("{}: {}: {}", conn.label(), req.method, e.message));
            None
        }
        None => None,
    };
    let key = shared.member.as_deref().and_then(|p| {
        use fleet::identity::Signer;
        let k = fleet::identity::FileKeyStore::new(fleet::member::key_path(p)).load().ok().flatten();
        k.map(|k| k.public_key())
    });
    let log_of = stored.as_ref().zip(own.as_deref()).map(|(stored, own)| lent::Log { stored, own, key });
    if let Err(e) = lent::admit_fleet(&req.method, &req.params, peer.as_deref(), log_of) {
        log(&format!("{}: refused {} from {}: {}", conn.label(), req.method, from.as_deref().unwrap_or("a local client"), e.message));
        return Response::err(req.id, e.code, e.message);
    }
    let (Some(ctx), Some(from)) = (ctx, from) else {
        return Response::err(req.id, "failed", "this daemon has no machines registry");
    };
    log(&format!("{}: {} from {from}", conn.label(), req.method));
    if matches!(req.method.as_str(), "fleet.report" | "fleet.graph") {
        // Admitted: this host has a log and a name.
        let (Some(stored), Some(me)) = (stored.as_ref(), own.as_deref()) else {
            return Response::err(req.id, "failed", "this host is in no fleet's membership log");
        };
        let answer = if req.method == "fleet.report" {
            fleet::reports::report_call(req.params, &from, &shared.graph, stored, me, log)
        } else {
            fleet::reports::graph_call(req.params, &from, &shared.graph, stored, me, log)
        };
        return match answer {
            Ok(result) => Response::ok(req.id, result),
            Err(e) => Response::err(req.id, e.code, e.message),
        };
    }
    if req.method == "fleet.place" {
        return match fleet_place(req.params, &from, shared) {
            Ok(result) => Response::ok(req.id, result),
            Err(e) => Response::err(req.id, e.code, e.message),
        };
    }
    let (answer, after) = machines::leave::fleet_call(&req.method, req.params, &from, ctx);
    conn.after = after;
    match answer {
        Ok(result) => Response::ok(req.id, result),
        Err(e) => Response::err(req.id, e.code, e.message),
    }
}

/// `fleet.place` on the hub, from member `from` (fleet plan decision 31):
/// where its run goes, by `placed::for_member` over this host (its
/// capacity now, its watched roots), the member's own report, and the
/// machines this host reaches (a link up, or a route) that the member
/// reaches too, each asked a fresh `capacity.get` as the hub's own
/// placement does.
fn fleet_place(params: Value, from: &str, shared: &Shared) -> Result<Value, api::ApiError> {
    use fleet::placed;
    let ask: placed::PlaceParams = api::parse(params)?;
    let hub = shared
        .member
        .as_deref()
        .and_then(|p| fleet::dialer::member_name(p, &shared.hostname).ok())
        .ok_or_else(|| api::ApiError::failed("this hub has no member name"))?;
    let machines = shared.remotes();
    let routed = shared.links().map(|l| l.routed()).unwrap_or_default();
    let fresh = fresh_capacities(shared, &placed::member_workers(&ask, from, &machines, &routed));
    let local_roots: Vec<Arc<Value>> = shared.locals.lock().unwrap().values().cloned().collect();
    let answer = placed::for_member(&ask, from, &hub, shared.capacity(), &local_roots, &machines, &routed, &fresh)
        .map_err(|e| api::ApiError::failed(e.to_string()))?;
    log(&format!("fleet.place from {from}: {} on {}", ask.spec.sandbox, answer.machine));
    serde_json::to_value(answer).map_err(|e| api::ApiError::internal(e.into()))
}

#[derive(serde::Deserialize)]
struct InboundParams {
    peer: String,
    /// Its link `Hello` (`fleet::update::hello_of`).
    version: String,
    #[serde(default)]
    hub: bool,
}

/// `links.inbound {peer, version, hub}`: a `fleet-link --peer <peer>` of
/// this user says the link `peer` dialed to this host is up, for as long as
/// this connection is (docs/reference/api.md, `links.inbound`). Refused for
/// a name `machines.toml` has (an inbound neighbor and a registry entry
/// never share a name). Once announced, the neighbor's roots are asked over
/// the link ([`inbound_roots`]) on a thread of its own, and the signed
/// configs followed from it ([`follow_configs`]).
fn links_inbound(id: Option<u64>, params: Value, shared: &Shared, conn: &mut ConnState) -> Response {
    if !matches!(conn.table, Table::Full) || !conn.own_user {
        return Response::err(id, "denied", "`links.inbound` is for this user's own `fleet-link`");
    }
    let p: InboundParams = match api::parse(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, e.code, e.message),
    };
    if let Err(e) = machines::check_name(&p.peer) {
        return Response::err(id, "invalid", format!("`peer`: {e:#}"));
    }
    let Some(m) = shared.machines.as_ref() else {
        return Response::err(id, "failed", "this daemon has no machines registry");
    };
    match machines::load(&m.registry) {
        Ok(entries) => {
            if let Some(e) = entries.iter().find(|e| e.name == p.peer) {
                let msg = format!(
                    "`{}` is already a machine here ({}): a link from a machine of the same name is refused; \
                     `devsandbox machine unlink {}` here, or another `name` in its member.toml",
                    p.peer,
                    e.label(),
                    p.peer
                );
                log(&format!("{}: links.inbound: {msg}", conn.label()));
                return Response::err(id, "denied", msg);
            }
        }
        Err(e) => return Response::err(id, "internal", format!("{e:#}")),
    }
    // `links.cycle` comes on this connection, unsubscribed.
    if let Err(e) = start_notifier(conn) {
        return Response::err(id, "internal", format!("cannot start this connection's notifier: {e}"));
    }
    let guard = m.inbound.announce(&p.peer, &p.version, p.hub);
    let set_roots = guard.roots_setter();
    conn.inbound = Some(guard);
    let name = p.peer.clone();
    conn.out.post(|p| {
        p.inbound = Some(name);
        false
    });
    log(&format!("{}: link from {} (devsandbox {}{})", conn.label(), p.peer, p.version, if p.hub { ", the hub" } else { "" }));
    follow_configs(shared.member.as_deref(), &shared.socket_dir, &p.peer);
    let (socket, peer) = (endpoint::fleet_socket(&shared.socket_dir, &p.peer), p.peer);
    let _ = std::thread::Builder::new().name("serve-inbound-roots".into()).spawn(move || match inbound_roots(&peer, &socket) {
        Ok(roots) => set_roots(roots),
        Err(e) => log(&format!("links.inbound: cannot list {peer}'s roots: {e:#}")),
    });
    Response::ok(id, json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachParams {
    /// An exact instance name or id: on this host, or on `machine`.
    instance: String,
    machine: Option<String>,
}

/// `instances.attach {instance, machine?}`: this user's client attaches to
/// `instance` (a local `exec`, a dashboard tab, `devsandbox vscode`) for as
/// long as this connection is (docs/reference/api.md, `instances.attach`).
/// Without `machine`, `instance` (exact name or id in `state.toml`) is this
/// host's: its latest attach is now from here. With `machine`, it's that
/// machine's, by the name this host gave it: recorded as this host's own
/// attach there. Local own-user connections only; never forwarded. A
/// connection that did nothing else is attach-only from then on: no holder,
/// not waited on by a drain ([`ConnEntry::holds`]).
fn instances_attach(id: Option<u64>, params: Value, shared: &Shared, conn: &mut ConnState) -> Response {
    if !matches!(conn.table, Table::Full) || !conn.own_user {
        return Response::err(id, "denied", "`instances.attach` is for this user's own local clients");
    }
    let p: AttachParams = match api::parse(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, e.code, e.message),
    };
    if p.instance.is_empty() {
        return Response::err(id, "invalid", "`instance` is empty");
    }
    let guard = match p.machine {
        Some(machine) => shared.attaches.begin_out(&machine, &p.instance, Instant::now()),
        None => {
            let state = match crate::state::State::load_at(&shared.state) {
                Ok(state) => state,
                Err(e) => return Response::err(id, "internal", format!("{e:#}")),
            };
            let found = state.instances.get(&p.instance).or_else(|| state.instances.values().find(|i| i.instance_id == p.instance));
            let Some(instance) = found else {
                return Response::err(id, "not-found", format!("no instance `{}`", p.instance));
            };
            shared.attaches.begin_in(&instance.instance_id, super::attaches::Origin::Here)
        }
    };
    conn.attaches.push(guard);
    if !conn.busy {
        conn.attach_only.store(true, Ordering::SeqCst);
    }
    Response::ok(id, json!({ "ok": true }))
}

/// `open.deliver {instance, root, url, profile}` from a fleet peer
/// (docs/architecture/daemon/fleet.md, *Opening URLs*): `opens::receive`'s
/// checks against this host's [`Shared::open_limit`] and its own outgoing
/// attaches, then the job handed to [`Shared::open_here`] (which doesn't
/// wait for the browser): `{ok: true}` means taken. A local client is
/// refused. Every refusal is logged.
fn open_deliver(id: Option<u64>, params: Value, shared: &Shared, conn: &mut ConnState) -> Response {
    let Table::Peer(peer) = &conn.table else {
        return Response::err(id, "denied", "`open.deliver` comes over a fleet link");
    };
    let from = peer.name.as_str();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let limit = |instance: &str| {
        let mut limit = shared.open_limit.lock().unwrap_or_else(|e| e.into_inner());
        limit.allow(&format!("{from}/{instance}"), None, Instant::now())
    };
    let attached = |instance: &str| shared.attaches.attached_out(from, instance, Instant::now(), crate::open::OPEN_ATTACH_WINDOW);
    let job = match opens::receive(params, from, now, limit, attached) {
        Ok(job) => job,
        Err(e) => {
            log(&format!("{}: refused open.deliver from {from}: {}", conn.label(), crate::open::printable(&e.message)));
            return Response::err(id, e.code, e.message);
        }
    };
    let Some(open_here) = shared.open_here.get() else {
        return Response::err(id, "failed", "this daemon opens nothing");
    };
    open_here(job);
    Response::ok(id, json!({ "ok": true }))
}

/// How long [`inbound_roots`] waits for the neighbor's hello.
const INBOUND_ROOTS_TIMEOUT: Duration = Duration::from_secs(20);

/// Inbound neighbor `peer`'s config roots: `roots.list` over one `API`
/// stream of its link, through `socket` (`fleet-<peer>.sock`, which its
/// `fleet-link` binds before announcing).
fn inbound_roots(peer: &str, socket: &Path) -> Result<Vec<PathBuf>> {
    let stream = super::fleet::link::connect(socket, crate::devsbd::proto::channel::API)
        .with_context(|| format!("cannot connect to {}", socket.display()))?;
    let mut link = machines::link::handshake(peer, machines::ssh::Duplex::socket(stream), INBOUND_ROOTS_TIMEOUT, machines::link::Over::Link)?;
    let r = link.conn.call("roots.list", json!({}))?;
    if let Some(e) = r.error {
        anyhow::bail!("roots.list: {}", e.message);
    }
    let rows: Vec<api::RootCandidate> = serde_json::from_value(r.result.unwrap_or(Value::Null)).context("bad roots.list answer")?;
    Ok(rows.into_iter().map(|c| c.root).collect())
}

/// `link.attach {peer, channel, key, introduced_by?}`: a connection of this
/// user (a `fleet-link --peer`'s, `commands::fleet_link::attach_stream`)
/// says the rest of it is one `channel` stream of link peer `peer`'s, which
/// proved `key` on the link (docs/reference/api.md, `link.attach`).
/// One-way: nothing turns it back to `Full`, and a second attach is refused
/// by the table itself.
///
/// The peer is `{peer, fleet_root: <data>/fleet/<peer>/ (created), env:
/// the daemon's, agent: its agent-link-<peer>.sock, key, introduced_by}`. `api` goes on served
/// with the trust table (`Table::Peer`), its pushed snapshots filtered too;
/// any other channel ends the line protocol once the answer is out, the
/// stream handed to [`Shared::serve_channel`] (`ConnState::handoff`). The
/// client sends nothing past the attach line before it has read the
/// answer: bytes already buffered past it, or a subscribed connection (its
/// notifier could still write), refuse the attach with `invalid`.
fn link_attach(id: Option<u64>, params: &Value, shared: &Shared, conn: &mut ConnState) -> Response {
    if !matches!(conn.table, Table::Full) || !conn.own_user {
        return Response::err(id, "denied", "`link.attach` is for this user's own relay connections");
    }
    let lent::Attach { name, channel, key, introduced_by } = match lent::attach_params(params) {
        Ok(parsed) => parsed,
        Err(e) => return Response::err(id, e.code, e.message),
    };
    if channel != lent::Channel::Api && conn.buffered {
        return Response::err(id, "invalid", "nothing may follow `link.attach {channel}` before its answer");
    }
    if channel != lent::Channel::Api && conn.notifier.is_some() {
        return Response::err(id, "invalid", "a subscribed connection can't become a channel stream");
    }
    let Some(data) = shared.data_dir() else {
        return Response::err(id, "failed", "this daemon has no data dir for a fleet root");
    };
    let peer = match shared.fleet_peer(data, &name, key, introduced_by, Some(endpoint::link_agent_socket(&shared.socket_dir, &name))) {
        Ok(peer) => peer,
        Err(e) => {
            log(&format!("{}: link.attach: {e:#}", conn.label()));
            return Response::err(id, "internal", format!("{e:#}"));
        }
    };
    let peer = Arc::new(peer);
    let files = shared.reach_files();
    let (reach, warning) = lent::Reach::load(&files, peer.fleet_root.as_deref());
    if let Some(w) = warning {
        log(&format!("{}: {w}", conn.label()));
    }
    let trust = serde_json::to_value(reach.trust).unwrap_or(Value::Null);
    let trust_name = trust.as_str().unwrap_or("?");
    match channel {
        lent::Channel::Api => {
            let filtered = (Arc::clone(&peer), files);
            conn.out.post(|p| {
                p.reach = Some(filtered);
                false
            });
            conn.table = Table::Peer(peer);
        }
        channel => conn.handoff = Some((channel, peer)),
    }
    // `AGENT` streams are mostly probes, one every `agent::PROBE`.
    if channel != lent::Channel::Agent {
        log(&format!("{}: {} stream from {name}, trust {trust_name}", conn.label(), channel.as_str()));
    }
    Response::ok(id, json!({ "ok": true, "trust": trust }))
}

/// An `api.rs` method, answered by this host.
fn local_call(method: &str, params: Value, shared: &Shared, conn: &ConnState) -> Result<Value, api::ApiError> {
    let bridges = shared.bridges.get().map(|w| w as &dyn api::Bridging);
    let forwards = shared.forwards.get().map(|h| h as &dyn api::Forwarding);
    let host = Some(api::HostPaths { state: &shared.state, home: shared.home.as_deref(), data: shared.data_dir() });
    api::call(method, params, &api::Ctx { inbox: &shared.inbox, client: &conn.audit, bridges, forwards, host })
}

/// A quick call with `machine`: the rest of its params to that machine's
/// daemon over its link (docs/architecture/daemon/machines.md, *Forwarded
/// requests*), answered with the remote's result or error, code and data
/// kept, under this request's id.
fn forward(id: Option<u64>, method: &str, params: Value, machine: &str, shared: &Shared) -> Response {
    let answer = match shared.links() {
        Some(links) => links.forward(machine, method, fwd::strip(params)),
        None => Err(api::ApiError::not_found(format!("no machine named `{machine}`"))),
    };
    match answer {
        Ok(r) => Response { id, ..r },
        Err(e) => Response::err(id, e.code, e.message),
    }
}

/// A verb request (`serve::verbs`): held on this connection until the verb
/// ends (or a `detach` answer), streaming its output. `None`: the client
/// hung up, or the daemon drains; nothing is answered.
fn verb(id: Option<u64>, method: &str, params: Value, shared: &Shared, conn: &ConnState) -> Option<Response> {
    let signs = matches!(method, "run" | "rebuild" | "start") && matches!(conn.table, Table::Full);
    match fwd::machine_of(&params) {
        Err(e) => return Some(Response::err(id, e.code, e.message)),
        Ok(Some(machine)) if method == "rm" => return remote_rm(id, params, &machine, shared, conn),
        Ok(Some(machine)) => {
            if signs {
                sign_remote_config(id, &params, shared, conn);
            }
            let peer = ConnPeer { out: &conn.out, shared };
            return remote_verb(id, method, params, &machine, shared, conn, &peer);
        }
        Ok(None) => {}
    }
    if signs && let Some(root) = params["root"].as_str().map(Path::new).filter(|r| r.is_absolute()) {
        sign_config(id, root, shared, conn);
    }
    if matches!(method, "run" | "rebuild" | "start" | "services.rebuild")
        && let Err(why) = check_mesh_copy(&params, shared)
    {
        log(&format!("{}: refused {method}: {why}", conn.label()));
        return Some(Response::err(id, "denied", why));
    }
    if method == "run"
        && let Some(answer) = placed_run(id, &params, shared, conn)
    {
        return answer;
    }
    let Some(runner) = shared.runner.get() else {
        return Some(Response::err(id, "internal", "this daemon runs no verbs"));
    };
    let req = match verbs::parse(method, params) {
        Ok(req) => req,
        Err(e) => return Some(Response::err(id, e.code, e.message)),
    };
    let peer = ConnPeer { out: &conn.out, shared };
    match verbs::serve(&req, id, runner, &shared.verbs, &peer, &conn.label(), log) {
        verbs::Outcome::Answer(Ok(result)) => Some(Response::ok(id, result)),
        verbs::Outcome::Answer(Err(f)) => {
            if f.code == "internal" {
                log(&format!("{}: {method}: {}", conn.label(), f.message));
            }
            Some(Response::err_with(id, f.code, f.message, f.data))
        }
        verbs::Outcome::Gone => None,
    }
}

/// A `run`/`rebuild`/`start` typed on the fleet's authority: config root
/// `root`'s version signed first when the root changed
/// (`changes::sign_config`), logged, and said in request `id`'s output
/// before the verb's own ([`config_notes`]). Best effort: the verb runs
/// whatever this does.
fn sign_config(id: Option<u64>, root: &Path, shared: &Shared, conn: &ConnState) {
    let Some(ctx) = shared.machines.as_ref() else { return };
    if let Some(signed) = machines::changes::sign_config(ctx, root) {
        say_signed(id, &signed, &root.display().to_string(), shared, conn);
    }
}

/// `signed` logged (`ConfigSigned::log_lines`) and said in request `id`'s
/// output ([`config_notes`], `told` keyed by `key`).
fn say_signed(id: Option<u64>, signed: &machines::changes::ConfigSigned, key: &str, shared: &Shared, conn: &ConnState) {
    for line in signed.log_lines() {
        log(&format!("{}: {line}", conn.label()));
    }
    let lines = config_notes(signed, key, &mut shared.told_not_authority.lock().unwrap_or_else(|e| e.into_inner()));
    let peer = ConnPeer { out: &conn.out, shared };
    for line in lines {
        let _ = verbs::Peer::notify(&peer, "output", verbs::output_params(id, "stderr", &format!("{line}\n")));
    }
}

/// What [`say_signed`] says in the verb's output (pure): `signed`'s
/// notes, except a `NotAuthority` whose `key` is already in `told`, so a
/// non-authority says it once per root per daemon. `key`: a local root's
/// path, a machine's root's name (no `/`, so the two never meet).
fn config_notes(signed: &machines::changes::ConfigSigned, key: &str, told: &mut BTreeSet<String>) -> Vec<String> {
    if matches!(signed, machines::changes::ConfigSigned::NotAuthority { .. }) && !told.insert(key.to_string()) {
        return Vec::new();
    }
    signed.notes()
}

/// A `run`/`rebuild`/`start`/`services.rebuild` whose `root` is a signed config's copy here
/// (under `mesh/tree/` in the data dir), from any client: admitted only
/// while `fleet::mesh::check_root_in` admits it, else why not (answered
/// `denied`) before a placement or the runner. The verb's subprocess
/// checks again at its command's entry (`fleet::mesh::check_root`), the
/// same function, so the two agree. `stop`/`rm` never come here: they
/// build nothing, and keep working on a changed copy.
fn check_mesh_copy(params: &Value, shared: &Shared) -> Result<(), String> {
    let (Some(member), Some(data)) = (shared.member.as_deref(), shared.data_dir()) else { return Ok(()) };
    let Some(root) = params["root"].as_str().map(Path::new).filter(|r| r.is_absolute()) else { return Ok(()) };
    fleet::mesh::check_root_in(data, member, root).map_err(|e| format!("{e:#}"))
}

/// This host's link to `peer` is up (where the handshake's newer log is
/// taken, and the next try after a failed pull): [`follow_configs_now`] on
/// a thread of its own.
fn follow_configs(member: Option<&Path>, socket_dir: &Path, peer: &str) {
    let (member, dir, from) = (member.map(Path::to_path_buf), socket_dir.to_path_buf(), peer.to_string());
    let spawned = std::thread::Builder::new().name("serve-mesh".into()).spawn(move || follow_configs_now(member.as_deref(), &dir, &from));
    if let Err(e) = spawned {
        log(&format!("fleet: cannot follow the signed configs (after {peer}): {e}"));
    }
}

/// Each config root whose signed head this host's copy isn't at installed,
/// from its stored bundle or one pulled from `peer` over the link
/// (`fleet::mesh::follow`), logged; returns once each is installed or
/// failed. On the request thread of a `fleet.sync` that moved a head
/// (`Ctx::log_from`), so its answer means this host has them; at link-up
/// through [`follow_configs`]. Best effort: the next such sync or link-up
/// tries again. Nothing without a `member.toml` (no data dir, no log).
fn follow_configs_now(member: Option<&Path>, socket_dir: &Path, peer: &str) {
    let Some((member, data)) = member.and_then(|m| Some((m, m.parent()?))) else { return };
    let heads = || -> Result<BTreeMap<String, (u64, [u8; 32])>> {
        let stored = fleet::membership_api::load(member).map_err(|e| anyhow::anyhow!("{}", e.message))?;
        Ok(stored.and_then(|s| s.log.membership().map(|m| m.config.clone())).unwrap_or_default())
    };
    let t = fleet::code::LinkTransfer::for_worker(socket_dir, peer);
    match fleet::mesh::follow(data, heads, &t) {
        Ok(done) => {
            for (due, r) in done {
                let (name, v) = (&due.name, due.version);
                if let Some(why) = &due.dropped {
                    log(&format!("fleet: dropped the stored bundle of config `{name}` v{v}, pulling it: {why}"));
                }
                let from = if due.stored { "its stored bundle" } else { peer };
                match r {
                    Ok(_) => log(&format!("fleet: installed config `{name}` v{v} from {from}")),
                    Err(e) => log(&format!("fleet: config `{name}` v{v} not installed (from {from}): {e:#}")),
                }
            }
        }
        Err(e) => log(&format!("fleet: cannot follow the signed configs (after {peer}): {e:#}")),
    }
}

/// [`sign_config`] for a verb aimed at a machine: its `root` is that
/// machine's path, so what signs is the local root bound to the same name
/// (`changes::bound_root`, the root's last component); unbound, nothing.
/// On a member that isn't the authority, bound or not, the verb is told
/// so (`NotAuthority`, once per root name): a host only the authority
/// binds names on is where a wrong setup order shows.
fn sign_remote_config(id: Option<u64>, params: &Value, shared: &Shared, conn: &ConnState) {
    use machines::changes::{self, ConfigSigned};
    let Some(ctx) = shared.machines.as_ref() else { return };
    let Some(name) = params["root"].as_str().and_then(|r| Path::new(r).file_name()).and_then(|n| n.to_str()) else { return };
    if let Some(authority) = changes::authority_elsewhere(ctx) {
        let signed = ConfigSigned::NotAuthority { root: name.into(), authority };
        return say_signed(id, &signed, name, shared, conn);
    }
    if let Some(root) = machines::changes::bound_root(ctx, name) {
        sign_config(id, &root, shared, conn);
    }
}

/// `helper.call` (`serve::helper`): held on this connection like a verb,
/// the call's records relayed until its last one, which is the answer.
/// `None`: the client hung up (the call is cancelled); nothing is answered.
fn helper_call(id: Option<u64>, params: Value, shared: &Shared, conn: &ConnState) -> Option<Response> {
    match fwd::machine_of(&params) {
        Err(e) => return Some(Response::err(id, e.code, e.message)),
        Ok(Some(_)) => return Some(Response::err(id, "invalid", "`helper.call` can't take a machine")),
        Ok(None) => {}
    }
    let Some(client) = shared.helper.get() else {
        return Some(Response::err(id, "internal", "this daemon runs no bridges"));
    };
    let p = match helper::parse(params) {
        Ok(p) => p,
        Err(e) => return Some(Response::err(id, e.code, e.message)),
    };
    let container = match crate::state::State::load_at(&shared.state) {
        Ok(state) => state.instances.get(&p.instance).map(|i| i.container.clone()),
        Err(e) => {
            log(&format!("{}: helper.call: {e:#}", conn.label()));
            return Some(Response::err(id, "internal", format!("{e:#}")));
        }
    };
    let Some(container) = container else {
        return Some(Response::err(id, "not-found", format!("no instance `{}`", p.instance)));
    };
    let peer = ConnPeer { out: &conn.out, shared };
    match helper::serve(client, &container, p, id, &conn.audit, &peer) {
        verbs::Outcome::Answer(Ok(result)) => Some(Response::ok(id, result)),
        verbs::Outcome::Answer(Err(f)) => Some(Response::err_with(id, f.code, f.message, f.data)),
        verbs::Outcome::Gone => None,
    }
}

/// A verb with `machine`: run there over a connection of its own
/// (`machines::forward::verb`, which says how a hang-up and a drain end it),
/// its notifications to `peer`.
fn remote_verb(
    id: Option<u64>,
    method: &str,
    params: Value,
    machine: &str,
    shared: &Shared,
    conn: &ConnState,
    peer: &dyn verbs::Peer,
) -> Option<Response> {
    let Some(links) = shared.links() else {
        return Some(Response::err(id, "not-found", format!("no machine named `{machine}`")));
    };
    let label = conn.label();
    let vctx = fwd::VerbCtx { links, held: &shared.remote_verbs, who: &label, log };
    fwd::verb(id, method, params, machine, &vctx, peer)
}

/// `rm` with `machine` (fleet plan decision 77; docs/architecture/daemon/fleet.md,
/// *Fetch-back*): an instance this host placed there is fetched back first
/// ([`fetch_placement`], its lines in the rm's output), and one that can't
/// be refuses the `rm`, nothing forwarded; `force` skips it (a line says
/// so). Every attempt fetches, the `ask` round's too (an `ask` that needs
/// no question removes): a second one brings nothing new. The fetch runs
/// on this connection's thread before the forward, so a hang-up meanwhile
/// is seen once the forward starts. Once the forwarded `rm` is answered
/// `ok`, the placement leaves `placements.json`. Any other instance's `rm`
/// is [`remote_verb`]'s as is.
fn remote_rm(id: Option<u64>, params: Value, machine: &str, shared: &Shared, conn: &ConnState) -> Option<Response> {
    use fleet::code::fetch::{self, Before};
    use fleet::placements;
    let peer = ConnPeer { out: &conn.out, shared };
    let say = |line: &str| {
        let _ = verbs::Peer::notify(&peer, "output", verbs::output_params(id, "stderr", &format!("{line}\n")));
    };
    let instance = params.get("instance").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = format!("{machine}/{instance}");
    let placement = placement_of(shared, machine, &instance);
    let refuse = |why: &str| {
        log(&format!("{}: rm {name}: refused: {why}", conn.label()));
        let message = format!(
            "cannot fetch {name} back, so it isn't removed: {why} (`devsandbox fetch {name}` retries; `rm --force` removes it without)"
        );
        Some(Some(Response::err_with(id, "failed", message, Some(json!({ "exit": null, "instance": null })))))
    };
    let refused = match (fetch::before("rm", &params, !matches!(placement, Ok(None))), &placement) {
        (Before::Required, Ok(Some(entry))) => match fetch_placement(shared, entry) {
            Ok(got) => {
                crate::commands::fetch::lines(&got).iter().for_each(|l| say(l));
                None
            }
            Err(e) => refuse(&format!("{e:#}")),
        },
        (Before::Required, Err(why)) => refuse(why),
        (Before::Skipped, _) => {
            say(&format!("not fetching {name} back (--force)"));
            None
        }
        _ => None,
    };
    if let Some(answer) = refused {
        return answer;
    }
    let answer = remote_verb(id, "rm", params, machine, shared, conn, &peer);
    if let (Some(r), Ok(Some(_))) = (&answer, &placement)
        && r.error.is_none()
    {
        let path = placements::path_next_to(&shared.state);
        if let Err(e) = placements::update(&path, |all| placements::remove(all, machine, &instance)) {
            log(&format!("rm: cannot drop the placement of {name} from {}: {e:#}", path.display()));
        }
    }
    answer
}

/// `instances.done` with `machine` (fleet plan decision 77): a `done` of an
/// instance this host placed there is fetched back first, best effort (a
/// failure logged and the answer's `warning`, the flag set either way);
/// then [`forward`]ed as any quick call.
fn remote_done(id: Option<u64>, params: Value, machine: &str, shared: &Shared) -> Response {
    use fleet::code::fetch::{self, Before};
    let instance = params.get("instance").and_then(Value::as_str).unwrap_or_default().to_string();
    let placement = placement_of(shared, machine, &instance);
    let warning = match (fetch::before("instances.done", &params, !matches!(placement, Ok(None))), placement) {
        (Before::BestEffort, Ok(Some(entry))) => fetch_placement(shared, &entry).err().map(|e| format!("{e:#}")),
        (Before::BestEffort, Err(why)) => Some(why),
        _ => None,
    };
    let mut r = forward(id, "instances.done", params, machine, shared);
    if let (Some(why), Some(Value::Object(result))) = (warning, r.result.as_mut()) {
        result.insert("warning".into(), json!(format!("cannot fetch {machine}/{instance} back: {why}")));
    }
    r
}

/// The `placements.json` entry of `machine/instance`, if this host placed
/// it; why that isn't known (a file that doesn't load).
fn placement_of(shared: &Shared, machine: &str, instance: &str) -> std::result::Result<Option<fleet::placements::Entry>, String> {
    use fleet::placements;
    let entries = placements::load(&placements::path_next_to(&shared.state)).map_err(|e| format!("{e:#}"))?;
    Ok(placements::find(&entries, Some(machine), instance).ok().cloned())
}

/// Fetch `entry`'s instance back over its worker's link or a route
/// (`fleet::code::fetch::fetch_back`: `instances.fetch` there, the pull,
/// the hidden refs here), logged either way: `placements.fetch`'s, and a
/// placed instance's `rm` and `done` typed here ([`remote_rm`],
/// [`remote_done`]).
fn fetch_placement(shared: &Shared, entry: &fleet::placements::Entry) -> anyhow::Result<fleet::code::fetch::FetchedBack> {
    use fleet::code::{LinkTransfer, fetch};
    let t = LinkTransfer::for_worker(&shared.socket_dir, &entry.machine);
    let got = fetch::fetch_back(entry, |haves| t.fetch_instance(&entry.instance, haves), &t);
    match &got {
        Ok(got) => log(&format!("fetch {}/{}: {} bytes into {}", entry.machine, entry.instance, got.fetched, got.folder.display())),
        Err(e) => log(&format!("fetch {}/{}: {e:#}", entry.machine, entry.instance)),
    }
    got
}

/// `run --place` without `machine`, placed (docs/architecture/daemon/fleet.md,
/// *Placement*; `fleet::placed`): `None` when it runs here (a note, if
/// any, already in its output), as every run without `place` does
/// ([`fleet::placed::asked`]: a typed run never asks the hub), else the
/// answer of the run forwarded to
/// the worker it was placed on, in the worker's copy of the config root,
/// or the failure to move the code there. On the hub (`member.toml`'s
/// `hub = "self"`, [`Shared::is_hub`]) this host places it, looking
/// further than itself only when some machine's link is up or a routed
/// one has a path (`placed::worth_placing`: any kind); on a member the hub
/// does (`fleet.place`, [`ask_hub`]), and the code moves over this host's
/// own paths either way ([`move_code`]: a link, else a route); in no fleet
/// it runs here. Moving the
/// code (each folder's snapshot and bundle, the transfers) runs on this
/// connection's thread before anything is forwarded; a hang-up meanwhile
/// is seen once the forward starts.
fn placed_run(id: Option<u64>, params: &Value, shared: &Shared, conn: &ConnState) -> Option<Option<Response>> {
    use fleet::member::Hub;
    use fleet::placed::{self, Placed};
    let str_of = |k: &str| params.get(k).and_then(Value::as_str);
    if !placed::asked(params) {
        return None;
    }
    let hub = lock_hub(&shared.hub).clone();
    if hub == Hub::None {
        return None;
    }
    let sandbox = str_of("sandbox")?;
    let machines = shared.remotes();
    let routed = shared.links().map(|l| l.routed()).unwrap_or_default();
    if hub.is_this() && !placed::worth_placing(&machines, &routed) {
        return None;
    }
    // A bad `root` is the local parse's to refuse.
    let root = PathBuf::from(str_of("root")?);
    if !root.is_absolute() {
        return None;
    }
    let peer = ConnPeer { out: &conn.out, shared };
    let say = |line: &str| {
        let _ = verbs::Peer::notify(&peer, "output", verbs::output_params(id, "stderr", &format!("{line}\n")));
    };
    // What ties the sandbox here is known before anything is asked; a
    // config that doesn't load is the local run's to report.
    if let Some(Placed::LocalWithNote(note)) = fleet::code::ties_of(&root, sandbox).ok().and_then(|t| placed::tied(&t)) {
        log(&format!("{}: run: {note}", conn.label()));
        say(&note);
        return None;
    }
    let locals: Vec<(PathBuf, Arc<Value>)> = shared.locals.lock().unwrap().iter().map(|(d, v)| (d.clone(), Arc::clone(v))).collect();
    let spec = placed::spec(sandbox, locals.iter().find(|(d, _)| *d == root).map(|(_, v)| v.as_ref()));
    let local_roots: Vec<Arc<Value>> = locals.into_iter().map(|(_, v)| v).collect();
    let ask = placed::Ask { branch: str_of("branch"), base: str_of("base") };
    let prepare = |worker: &str| move_code(shared, worker, &root, sandbox, ask.branch);
    let decided = match &hub {
        Hub::Other(k) => {
            let reachable = placed::reachable_workers(&machines, &routed);
            let answer = ask_hub(shared, k, &placed::PlaceParams::of(&spec, shared.capacity(), reachable.clone()));
            placed::by_hub(&ask, k, answer, &reachable, prepare)
        }
        _ => {
            let fresh = fresh_capacities(shared, &placed::reachable_workers(&machines, &routed));
            let candidates = placed::candidates(&spec, Some(shared.capacity()), &local_roots, &machines, &routed, &fresh);
            placed::decide(&ask, &spec, &candidates, prepare)
        }
    };
    match decided {
        Placed::Local => None,
        Placed::LocalWithNote(note) => {
            log(&format!("{}: run: {note}", conn.label()));
            say(&note);
            None
        }
        Placed::Here(lines) => {
            log(&format!("{}: run placed here by the hub", conn.label()));
            for line in &lines {
                say(line);
            }
            None
        }
        Placed::Failed(why) => {
            log(&format!("{}: run: {why}", conn.label()));
            Some(Some(Response::err_with(id, "failed", why, Some(json!({ "exit": null, "instance": null })))))
        }
        Placed::Worker { name, root: remote_root, base, lines, folder } => {
            log(&format!("{}: run placed on {name} ({})", conn.label(), remote_root.display()));
            for line in &lines {
                say(line);
            }
            let params = placed::forwarded(params.clone(), &name, &remote_root, base.as_deref());
            let tagged = placed::Tagged { inner: &peer, machine: &name };
            let answer = remote_verb(id, "run", params, &name, shared, conn, &tagged);
            if let (Some(r), Some(folder)) = (&answer, &folder) {
                record_placement(shared, r, &name, sandbox, &root, folder);
            }
            Some(answer.map(|r| placed::tag_answer(r, &name)))
        }
    }
}

/// Move the code of `sandbox` (config root `root`, `--branch` `branch`)
/// into this host's fleet root on `worker`, over its link or a route
/// (`route::connect`): the `prepare` of a placed run, the hub's own or a
/// member's placed by the hub. Then, on a member placing on a worker that
/// isn't its hub, the hub mirror (fleet plan decision 75, `code::mirror`):
/// the same refs into the hub's cache, best effort, each failure a note
/// in the run's output (`Prepared::notes`) and the log.
fn move_code(
    shared: &Shared,
    worker: &str,
    root: &Path,
    sandbox: &str,
    branch: Option<&str>,
) -> std::result::Result<fleet::code::Prepared, fleet::code::Unprepared> {
    use fleet::code::{self, LinkTransfer, Unprepared};
    use fleet::member::Hub;
    // The machine says where this host's fleet root is, over the same path.
    let worker_root = fleet_root_on(shared, worker).map_err(Unprepared::NotMovable)?;
    let t = LinkTransfer::for_worker(&shared.socket_dir, worker);
    let mut prepared = code::prepare(&t, &worker_root, root, sandbox, branch)?;
    let hub = lock_hub(&shared.hub).clone();
    if let Hub::Other(k) = hub
        && k != worker
    {
        for (folder, why) in code::mirror(&prepared.sent, &LinkTransfer::for_worker(&shared.socket_dir, &k)) {
            let note = format!("note: the hub `{k}` has no copy of {}: {why}", folder.display());
            log(&format!("run: {note}"));
            prepared.notes.push(note);
        }
    }
    Ok(prepared)
}

/// A placed run's answer `r` naming its instance on worker `machine` (a
/// successful or detached one, never a failure): the placement into
/// `placements.json` (`fleet::placements`), so fetch-back knows the local
/// checkout it came from. A write that fails is logged; the run is the
/// worker's either way.
fn record_placement(shared: &Shared, r: &Response, machine: &str, sandbox: &str, root: &Path, folder: &fleet::placed::MovedFolder) {
    use fleet::placements::{self, Entry};
    if r.error.is_some() {
        return;
    }
    let Some(instance) = r.result.as_ref().and_then(|v| v.get("instance")).and_then(Value::as_str) else { return };
    let entry = Entry {
        machine: machine.to_string(),
        instance: instance.to_string(),
        sandbox: sandbox.to_string(),
        root: root.to_path_buf(),
        folder: folder.local.clone(),
        remote: folder.remote.clone(),
        repo: folder.repo.clone(),
        branch: folder.branch.clone(),
    };
    let path = placements::path_next_to(&shared.state);
    if let Err(e) = placements::update(&path, |all| placements::record(all, entry)) {
        log(&format!("run: cannot record the placement of {machine}/{instance} in {}: {e:#}", path.display()));
    }
}

/// `placements.fetch {machine?, instance}` (a local client's; fleet plan
/// decision 76): the `placements.json` entry it names (`not-found` when
/// this host didn't place it; a bare `instance` when exactly one placement
/// has that name), fetched back over the worker's link or a route
/// (`fleet::code::fetch::fetch_back`: `instances.fetch` there, the pull,
/// the hidden refs here); `failed` with why it didn't.
fn placements_fetch(params: &Value, shared: &Shared) -> std::result::Result<Value, api::ApiError> {
    use fleet::code::fetch;
    use fleet::placements;
    let instance = params
        .get("instance")
        .and_then(Value::as_str)
        .ok_or_else(|| api::ApiError::invalid(format!("`{}` needs `instance`", fetch::PLACEMENTS_FETCH)))?;
    let machine = fwd::machine_of(params)?;
    let entries = placements::load(&placements::path_next_to(&shared.state)).map_err(api::ApiError::internal)?;
    let entry = placements::find(&entries, machine.as_deref(), instance).map_err(api::ApiError::not_found)?.clone();
    let got = fetch_placement(shared, &entry)
        .map_err(|e| api::ApiError::failed(format!("cannot fetch {}/{} back: {e:#}", entry.machine, entry.instance)))?;
    serde_json::to_value(got).map_err(|e| api::ApiError::internal(e.into()))
}

/// The hub `hub`'s `fleet.place` answer to `ask`, over a fresh `API`
/// stream of any path to it (`Handle::open_member`: its link while up,
/// else a route; the dial and hello bounded by `placed::CAPACITY_WAIT`,
/// the answer by `placed::PLACE_WAIT`); why none, for the note of a run
/// that then stays here.
fn ask_hub(
    shared: &Shared,
    hub: &str,
    ask: &fleet::placed::PlaceParams,
) -> std::result::Result<fleet::placed::PlaceAnswer, fleet::placed::Unplaced> {
    use fleet::placed::{CAPACITY_WAIT, PLACE_WAIT, Unplaced};
    let down = |why: String| Unplaced::Unreachable(why);
    let links = shared.links().ok_or_else(|| down("this daemon has no machines".into()))?;
    let duplex = links.open_member(hub).map_err(down)?;
    let mut link = link::handshake(hub, duplex, CAPACITY_WAIT, link::Over::Link).map_err(|e| down(e.to_string()))?;
    let params = serde_json::to_value(ask).map_err(|e| Unplaced::Refused(e.to_string()))?;
    let r = link.conn.call_within("fleet.place", params, PLACE_WAIT).map_err(|e| down(format!("{e:#}")))?;
    let r = r.ok_or_else(|| down(format!("no `fleet.place` answer within {}s", PLACE_WAIT.as_secs())))?;
    if let Some(e) = r.error {
        return Err(Unplaced::Refused(format!("fleet.place: {}", e.message)));
    }
    serde_json::from_value(r.result.unwrap_or(Value::Null)).map_err(|e| Unplaced::Refused(format!("bad fleet.place answer: {e}")))
}

/// This host's fleet root on machine `worker`, as its `roots.list` answer
/// over a fresh `API` stream of any path to it (`Handle::open_member`)
/// flags it (`fleet::placed::fleet_root`); why not, for the note of a run
/// that then stays here.
fn fleet_root_on(shared: &Shared, worker: &str) -> std::result::Result<PathBuf, String> {
    use fleet::placed::CAPACITY_WAIT;
    let links = shared.links().ok_or("this daemon has no machines")?;
    let unknown = |why: String| format!("its fleet root for this host isn't known ({why})");
    let duplex = links.open_member(worker).map_err(unknown)?;
    let mut link = link::handshake(worker, duplex, CAPACITY_WAIT, link::Over::Link).map_err(|e| unknown(e.to_string()))?;
    let r = link.conn.call_within("roots.list", json!({}), CAPACITY_WAIT).map_err(|e| unknown(format!("{e:#}")))?;
    let r = r.ok_or_else(|| unknown("`roots.list` timed out".into()))?;
    if let Some(e) = r.error {
        return Err(unknown(format!("roots.list: {}", e.message)));
    }
    let rows: Vec<api::RootCandidate> =
        serde_json::from_value(r.result.unwrap_or(Value::Null)).map_err(|e| unknown(format!("bad roots.list answer: {e}")))?;
    fleet::placed::fleet_root(&rows).ok_or_else(|| unknown("none listed".into()))
}

/// Each of `workers`' `capacity.get`, asked in parallel over any path to
/// it (`Handle::open_member`: a fresh `API` stream of its link, else a
/// route; hello, then the call), each bounded by `placed::CAPACITY_WAIT`
/// for its dial-and-hello and again for its answer. A worker that doesn't
/// answer in time, or fails, is left out: its saved report stands.
fn fresh_capacities(shared: &Shared, workers: &[String]) -> BTreeMap<String, super::capacity::Capacity> {
    use fleet::placed::CAPACITY_WAIT;
    let Some(links) = shared.links() else { return BTreeMap::new() };
    std::thread::scope(|s| {
        let asks: Vec<_> = workers
            .iter()
            .map(|worker| {
                let links = &links;
                s.spawn(move || {
                    let duplex = links.open_member(worker).ok()?;
                    let mut link = link::handshake(worker, duplex, CAPACITY_WAIT, link::Over::Link).ok()?;
                    let r = link.conn.call_within("capacity.get", json!({}), CAPACITY_WAIT).ok()??;
                    let capacity = serde_json::from_value(r.result?).ok()?;
                    Some((worker.clone(), capacity))
                })
            })
            .collect();
        asks.into_iter().filter_map(|a| a.join().ok().flatten()).collect()
    })
}

/// A verb's view of its connection.
struct ConnPeer<'a> {
    out: &'a Outbox,
    shared: &'a Shared,
}

impl verbs::Peer for ConnPeer<'_> {
    fn notify(&self, method: &str, params: Value) -> std::io::Result<()> {
        self.out.send(&Notification { method: method.into(), params })
    }

    fn hung_up(&self, tick: Duration) -> bool {
        hung_up(&self.out.ctl, tick)
    }

    fn draining(&self) -> bool {
        self.shared.draining.load(Ordering::SeqCst)
    }
}

/// Wait up to `tick` for `stream`'s peer to hang up, without reading:
/// readable and a peeked 0 bytes is EOF. Pending data (a request pipelined
/// behind the verb) is left for the reader, and waited out by sleeping
/// rather than polling again, which would return at once and spin.
pub(crate) fn hung_up(stream: &Stream, tick: Duration) -> bool {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let ms = libc::c_int::try_from(tick.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one valid pollfd, alive for the call.
    if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
        return false;
    }
    let mut byte = 0u8;
    // SAFETY: a one-byte buffer we own; MSG_PEEK leaves the data queued.
    let n = unsafe { libc::recv(fd, (&raw mut byte).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    match n {
        0 => true,
        1.. => {
            std::thread::sleep(tick);
            false
        }
        _ => {
            let e = std::io::Error::last_os_error();
            !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted)
        }
    }
}

/// The connection's notifier, started once.
fn start_notifier(conn: &mut ConnState) -> std::io::Result<()> {
    if conn.notifier.is_none() {
        let out = Arc::clone(&conn.out);
        let spawned = std::thread::Builder::new().name(format!("serve-notify-{}", conn.id)).spawn(move || notifier(&out));
        conn.notifier = Some(spawned?);
    }
    Ok(())
}

/// `subscribe` (`on`) / `unsubscribe`. The first subscribe starts the
/// connection's notifier. Nothing is sent for the past: a client subscribes,
/// then fetches.
fn subscribe(params: Value, conn: &mut ConnState, on: bool) -> Result<Value, api::ApiError> {
    let topics = api::topics(params)?;
    if on {
        start_notifier(conn).map_err(|e| api::ApiError::internal(e.into()))?;
    }
    conn.out.post(|p| {
        for topic in topics {
            if on {
                p.topics.insert(topic);
            } else {
                p.topics.remove(&topic);
                match topic {
                    Topic::Inbox => {
                        p.inbox = None;
                        p.shown = None;
                    }
                    Topic::Instances => {
                        p.instances = false;
                        p.watched.clear();
                        p.snapshots.clear();
                    }
                    Topic::Forwards => {
                        p.forwards = false;
                        p.forwards_status = None;
                    }
                    Topic::Opens => p.opens.clear(),
                }
            }
        }
        false
    });
    Ok(json!({ "ok": true }))
}

/// `instances.watch {dir, machines}`: push `instances.snapshot` for root
/// `dir` to this connection while it's connected and subscribed to
/// `instances` (a watch needs that subscription first: the pushes are
/// notifications). The first one goes out at once: the cached snapshot when
/// another watcher keeps it fresh, else one from disk (no runtime call)
/// while the poll is woken for the listing and stats. With `machines`, every
/// push is merged with the machines' rows (the first with what they have
/// now), and the machine links are wanted while it lasts. A re-watch with
/// the other flag switches.
fn watch_root(params: Value, shared: &Shared, conn: &ConnState) -> Result<Value, api::ApiError> {
    let machines = api::watch_machines(&params)?;
    let dir = api::watch_dir(params, true)?;
    // A link peer's federated watch: a member watching its hub. Only the
    // hub federates for a peer (it watches its members plain), so no two
    // members re-export each other's rows.
    let except = match &conn.table {
        Table::Peer(peer) if machines => {
            if !shared.is_hub() {
                return Err(api::ApiError::denied("only the fleet's hub serves a federated watch to a link peer"));
            }
            Some(peer.name.clone())
        }
        _ => None,
    };
    let mut subscribed = false;
    conn.out.post(|p| {
        subscribed = p.topics.contains(&Topic::Instances);
        false
    });
    if !subscribed {
        return Err(api::ApiError::invalid(
            "subscribe to `instances` before `instances.watch`: snapshots are notifications",
        ));
    }
    let cached = shared.snapshots.cached(&dir);
    let first = match &cached {
        Some(params) => Some(Arc::clone(params)),
        None => snapshots::params(&dir, &(shared.join)(&dir, &crate::snapshot::RuntimeView::disk())),
    };
    // Under `locals`, the root's params, its watch and the first push's
    // merge go in together: `watched_roots` can't prune the params before
    // the watch lands, and a `publish_machines` either sees both (and
    // posts after, with remotes read later) or neither (and this merge
    // reads remotes after the change it was called for). Every remotes or
    // grants change calls its own `publish_machines`, so none is missed.
    let mut locals = shared.locals.lock().unwrap();
    if cached.is_none()
        && let Some(first) = &first
    {
        // Nothing fresher for a machine's push to merge with (with no
        // poll, as in tests, the only one).
        locals.insert(dir.clone(), Arc::clone(first));
    }
    conn.out.post(|p| {
        let switched = p.watched.insert(dir.clone(), machines).is_some_and(|was| was != machines);
        // A poll's snapshot already pending (a re-watch) is newer, unless
        // it's the other variant.
        match first {
            Some(first) if switched || !p.snapshots.contains_key(&dir) => {
                let first = if machines {
                    let merged = snapshots::merge(&first, &shared.remotes(), except.as_deref(), &shared.exposed_workers(None), &shared.agent_granted());
                    Arc::new(merged)
                } else {
                    first
                };
                p.snapshots.insert(dir, first);
                true
            }
            _ => false,
        }
    });
    drop(locals);
    if cached.is_none()
        && let Some(poll) = shared.bridges.get()
    {
        poll.refresh();
    }
    if machines && let Some(links) = shared.links() {
        links.wake();
    }
    Ok(json!({ "ok": true }))
}

/// `instances.unwatch {dir}`: stop pushing root `dir` here (a pending one is
/// dropped). Not watching it is fine.
fn unwatch_root(params: Value, conn: &ConnState) -> Result<Value, api::ApiError> {
    let dir = api::watch_dir(params, false)?;
    conn.out.post(|p| {
        p.watched.remove(&dir);
        p.snapshots.remove(&dir);
        false
    });
    Ok(json!({ "ok": true }))
}

fn hello(id: Option<u64>, params: Value, shared: &Shared, conn: &mut ConnState) -> Response {
    let params: HelloParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "invalid", format!("bad hello params: {e}")),
    };
    conn.client.clone_from(&params.client);
    conn.audit = api::audit_client(&params.client);
    let client = Version { semver: params.version, build: params.build };
    // A fleet hub's hello never hands this daemon off: only a local client's
    // newer binary replaces it, or any binary of this user's that forces it
    // (a fleet worker following its hub's version, a downgrade included:
    // ADR 0042).
    let local = matches!(conn.table, Table::Full);
    let forced = params.force && local && conn.own_user;
    if params.force && !forced {
        log(&format!("{}: refused a forced handoff (not a local client of this user)", conn.label()));
    }
    let handoff = local && (forced || client.is_newer_than(&shared.version));
    if handoff {
        log(&format!(
            "client `{}` has version {} build {}: handing off{}",
            params.client,
            client.semver,
            client.build,
            if forced { " (forced)" } else { "" }
        ));
        shared.begin_stop(Exit::Handoff);
    }
    let result = HelloResult {
        version: shared.version.semver.clone(),
        build: shared.version.build,
        protocol: proto::PROTOCOL,
        handoff,
    };
    Response::ok(id, serde_json::to_value(result).unwrap_or(Value::Null))
}

/// One line to stderr, which the lazy start points at `serve.log`.
pub(super) fn log(msg: &str) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    eprintln!("[{secs}] devsandbox serve: {msg}");
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::serve::client::{self, Hello};

    pub(crate) fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Not the authority: said once per root; a sign or a failure every
    /// time.
    #[test]
    fn not_the_authority_is_said_once_per_root() {
        use machines::changes::ConfigSigned;
        let mut told = BTreeSet::new();
        let other = |root: &str| ConfigSigned::NotAuthority { root: root.into(), authority: "lap".into() };
        assert_eq!(config_notes(&other("a"), "/r/a", &mut told), other("a").notes());
        assert_eq!(config_notes(&other("a"), "/r/a", &mut told), Vec::<String>::new());
        assert_eq!(config_notes(&other("b"), "/r/b", &mut told).len(), 1, "another root");
        assert_eq!(config_notes(&other("a"), "a", &mut told).len(), 1, "a machine's root, by name");
        assert_eq!(config_notes(&other("a"), "a", &mut told), Vec::<String>::new());
        let signed = ConfigSigned::Signed { root: "a".into(), version: 1, unpushed: vec![] };
        let failed = ConfigSigned::Failed { root: "a".into(), error: "x".into() };
        for _ in 0..2 {
            assert_eq!(config_notes(&signed, "/r/a", &mut told), signed.notes());
            assert_eq!(config_notes(&failed, "/r/a", &mut told), failed.notes());
        }
    }

    #[test]
    fn config_stamps_move_on_edit_create_and_rename() {
        let root = scratch("cfg-stamps");
        std::fs::create_dir_all(&root).unwrap();
        let roots = [root.clone()];
        let at = || config_stamps(roots.iter());
        let empty = at();
        assert_eq!(empty[&root].len(), 4, "three names plus the legacy one");
        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.a]\n").unwrap();
        let created = at();
        assert_ne!(created, empty);
        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.a]\ninbox = true\n").unwrap();
        let edited = at();
        assert_ne!(edited, created, "the length moved");
        std::fs::rename(root.join("devsandboxes.toml"), root.join("devsandboxes.yaml")).unwrap();
        assert_ne!(at(), edited);
        assert_eq!(at(), at(), "no change, no difference");
        let _ = std::fs::remove_dir_all(&root);
    }

    pub(crate) fn opts(build: u64, idle_ms: u64) -> Options {
        Options {
            version: Version { semver: "0.6.0".into(), build },
            keep_alive: false,
            // Never the user's: a test hub that checks it sets its own.
            remotes: Some(std::env::temp_dir().join(format!("dsv-remotes-{}", std::process::id())).join("remotes.json")),
            idle_timeout: Duration::from_millis(idle_ms),
            host: false,
            inbox: Some(std::env::temp_dir().join(format!("dsv-inbox-{}", std::process::id())).join("inbox.json")),
            // Never this binary on the user's state: a verb sent to a test
            // daemon that doesn't set its own fails fast.
            verbs: Some(verbs::Runner {
                program: vec!["false".into()],
                spool: std::env::temp_dir().join(format!("dsv-verbs-{}", std::process::id())),
                state: std::env::temp_dir().join(format!("dsv-verbs-{}", std::process::id())).join("state.toml"),
            }),
            // Never the user's: the watcher only stamps it, but a test that
            // listens for `instances.changed` sets its own.
            state: Some(std::env::temp_dir().join(format!("dsv-state-{}", std::process::id())).join("state.toml")),
            join: Some(fake_join()),
            // Never the user's home, registry or ssh.
            home: Some(std::env::temp_dir().join(format!("dsv-home-{}", std::process::id()))),
            machines: Some(std::env::temp_dir().join(format!("dsv-machines-{}", std::process::id())).join("machines.toml")),
            probe: Some(Arc::new(|e: &machines::Entry| {
                anyhow::bail!("a test daemon has no add-time dial (machine {}): set Options.probe", e.name)
            })),
            link_transport: Some(crate::serve::fleet::transport::Transport::new(|target, _| {
                anyhow::bail!("a test daemon has no transport (to {target}): set Options.link_transport")
            })),
            machine_timing: service::Timing {
                backoff: Duration::from_millis(50),
                backoff_cap: Duration::from_millis(200),
                call: Duration::from_secs(5),
                handshake: Duration::from_secs(5),
                tick: Duration::from_millis(20),
            },
            helper: None,
            // Never the user's: absent unless a test writes its own.
            member: Some(std::env::temp_dir().join(format!("dsv-member-{}", std::process::id())).join("member.toml")),
            // Never a download or a handoff of this test binary.
            update_steps: Some(Arc::new(crate::serve::fleet::update::tests::Fake::default())),
            peer_env: None,
            // Never the runtime's `info`: an empty report (caps aside).
            local_capacity: Some(Arc::new(crate::serve::capacity::Capacity::default)),
            // Never the user's agent.
            host_agent: Some(Arc::new(|| None)),
            // Short, but each probe of a routed member is a routed opening.
            agent_probe: Duration::from_millis(200),
            pty_exec: None,
            // Never a browser on the test host: taken opens go nowhere.
            open_here: Some(Arc::new(|_| {})),
        }
    }

    /// A [`Join`] that reads nothing: no rows, the view's depth as the
    /// runtime's name (`Disk` for a new watch's first snapshot).
    pub(crate) fn fake_join() -> Join {
        Arc::new(|_, view| api::InstancesList {
            snapshot: {
                let mut s = crate::serve::snapshots::tests::snapshot(Vec::new(), view.depth == crate::snapshot::Depth::Full);
                s.runtime_name = format!("{:?}", view.depth);
                s
            },
            thread_children: Default::default(),
        })
    }

    pub(crate) fn spawn_daemon(dir: &Path, opts: Options) -> std::thread::JoinHandle<Result<Exit>> {
        let dir = dir.to_owned();
        std::thread::spawn(move || run(&dir, &opts))
    }

    fn v(build: u64) -> Version {
        Version { semver: "0.6.0".into(), build }
    }

    /// Connect (retrying while the daemon binds) and say hello.
    fn hello_at(dir: &Path, version: &Version) -> (Stream, Hello) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stream) = endpoint::connect(dir) {
                let mut stream = stream;
                let hello = client::hello(&mut stream, version, "test").unwrap();
                return (stream, hello);
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn concurrent_starts_have_one_winner_and_all_clients_reach_it() {
        let dir = scratch("race");
        let daemons: Vec<_> = (0..8).map(|_| spawn_daemon(&dir, opts(1, 300))).collect();
        let clients: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || hello_at(&dir, &v(1)).1)
            })
            .collect();
        for c in clients {
            match c.join().unwrap() {
                Hello::Ready(result) => assert_eq!(result.build, 1),
                other => panic!("{other:?}"),
            }
        }
        let mut exits: Vec<_> = daemons.into_iter().map(|d| d.join().unwrap().unwrap()).collect();
        exits.sort_by_key(|e| *e != Exit::Idle);
        assert_eq!(exits[0], Exit::Idle);
        assert!(exits[1..].iter().all(|e| *e == Exit::AlreadyRunning), "{exits:?}");
        assert!(!endpoint::socket_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_hello_hands_off_drains_and_exits() {
        let dir = scratch("handoff");
        let daemon = spawn_daemon(&dir, opts(1, 60_000));

        // Equal and older clients just proceed.
        let (mut idle_client, equal) = hello_at(&dir, &v(1));
        assert!(matches!(equal, Hello::Ready(_)), "{equal:?}");
        let (_older_conn, older) = hello_at(&dir, &Version { semver: "0.5.9".into(), build: 9 });
        assert!(matches!(older, Hello::Ready(_)), "{older:?}");

        let started = Instant::now();
        let (_newer_conn, newer) = hello_at(&dir, &v(2));
        assert!(matches!(newer, Hello::Handoff), "{newer:?}");
        // Nothing answers once the handoff reply is out.
        assert!(endpoint::connect(&dir).is_err());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        // Idle connections are closed, not waited out.
        assert!(started.elapsed() < DRAIN_TIMEOUT);
        let mut buf = [0u8; 1];
        assert_eq!(idle_client.read(&mut buf).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A forced hello from this user's local client hands off even when the
    /// client is older (a fleet worker's downgrade).
    #[test]
    fn a_forced_hello_hands_off_to_an_older_binary() {
        let dir = scratch("forced");
        let daemon = spawn_daemon(&dir, opts(5, 60_000));
        drop(hello_at(&dir, &v(5)));
        let mut stream = endpoint::connect(&dir).unwrap();
        let older = Version { semver: "0.5.0".into(), build: 1 };
        let answer = client::hello_with(&mut stream, &older, "fleet-update", true).unwrap();
        assert!(matches!(answer, Hello::Handoff), "{answer:?}");
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_methods_and_garbage_get_errors_with_ids() {
        let dir = scratch("unknown");
        let daemon = spawn_daemon(&dir, opts(1, 200));
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut ask = |line: &str| {
            writer.write_all(format!("{line}\n").as_bytes()).unwrap();
            let mut out = String::new();
            reader.read_line(&mut out).unwrap();
            serde_json::from_str::<Value>(&out).unwrap()
        };
        let r = ask(r#"{"id":7,"method":"inbox.poll.vote"}"#);
        assert_eq!(r["id"], 7);
        assert_eq!(r["error"]["code"], "unknown-method");
        let r = ask("not json");
        assert_eq!(r["error"]["code"], "invalid");
        let r = ask(r#"{"id":8,"method":"hello","params":{"nope":1}}"#);
        assert_eq!((r["id"].as_u64(), r["error"]["code"].as_str()), (Some(8), Some("invalid")));
        drop(ask);
        drop(writer);
        drop(reader);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn connect(dir: &Path) -> client::Conn {
        client::connect_with(dir, "test", &v(1), client::START_TIMEOUT, &|_, _| Ok(())).unwrap()
    }

    fn note(path: &Path, msg: &str) {
        use crate::devsbd::notify::{Level, Record};
        let record = Record { level: Level::Info, key: None, link: None, msg: msg.into(), at: 1 };
        crate::inbox::store::update_at(path, |i| i.push("w-id".into(), "w".into(), record, true)).unwrap();
    }

    /// Notifications from other tests' store writes (the in-process
    /// generation is process-wide) are harmless extras: drop what's queued.
    fn drain_notes(conn: &mut client::Conn, wait: Duration) {
        while conn.next_notification(wait).unwrap().is_some() {}
    }

    #[test]
    fn subscribers_hear_inbox_changes_between_responses() {
        let dir = scratch("notify");
        let store = scratch("notify-store").join("inbox.json");
        let daemon = spawn_daemon(&dir, Options { inbox: Some(store.clone()), ..opts(1, 200) });
        let mut conn = connect(&dir);
        assert_eq!(conn.daemon.protocol, proto::PROTOCOL);

        let r = conn.call("subscribe", json!({"topics": ["nope"]})).unwrap();
        assert_eq!(r.error.unwrap().code, "invalid");
        let r = conn.call("subscribe", json!({"topics": ["inbox"]})).unwrap();
        assert_eq!(r.result, Some(json!({"ok": true})));
        drain_notes(&mut conn, Duration::from_millis(200));

        note(&store, "one");
        let n = conn.next_notification(Duration::from_secs(3)).unwrap().expect("no inbox.changed");
        assert_eq!(n.method, "inbox.changed");
        assert!(n.params["generation"].as_u64().unwrap() >= 1, "{n:?}");

        // A notification sent before a response doesn't break the call: it's
        // set aside, and handed out without touching the socket.
        drain_notes(&mut conn, Duration::from_millis(100));
        note(&store, "two");
        std::thread::sleep(Duration::from_millis(300));
        let r = conn.call("inbox.threads.list", json!({"view": "all"})).unwrap();
        assert_eq!(r.result.unwrap().as_array().unwrap().len(), 2);
        let n = conn.next_notification(Duration::ZERO).unwrap().expect("not set aside by call");
        assert_eq!(n.method, "inbox.changed");

        // API errors carry the request's id.
        let r = conn.call("inbox.thread.get", json!({"thread": 999})).unwrap();
        assert_eq!(r.error.unwrap().code, "not-found");

        let r = conn.call("unsubscribe", json!({"topics": ["inbox"]})).unwrap();
        assert!(r.error.is_none());
        drain_notes(&mut conn, Duration::ZERO);
        note(&store, "three");
        assert!(conn.next_notification(Duration::from_millis(800)).unwrap().is_none());

        drop(conn);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    /// A `state.toml` write (a phase, a new instance, a rename) is an
    /// `instances.changed`, with no host side polling containers.
    #[test]
    fn subscribers_hear_state_changes() {
        let dir = scratch("state-watch");
        let state = scratch("state-watch-file").join("state.toml");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        let daemon = spawn_daemon(&dir, Options { state: Some(state.clone()), ..opts(1, 200) });
        let mut conn = connect(&dir);
        let r = conn.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        assert!(conn.next_notification(Duration::from_millis(700)).unwrap().is_none(), "nothing moved yet");

        std::fs::write(&state, "[instance.a]\n").unwrap();
        wait_for(&mut conn, "instances.changed");
        // A second write (longer: the size moves even within mtime's grain).
        std::fs::write(&state, "[instance.a]\n[instance.b]\n").unwrap();
        wait_for(&mut conn, "instances.changed");

        drop(conn);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(state.parent().unwrap());
    }

    /// The next notification named `method`, skipping others (stray
    /// `inbox.changed` from other tests' writes).
    fn wait_for(conn: &mut client::Conn, method: &str) -> Notification {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let n = conn.next_notification(left).unwrap().unwrap_or_else(|| panic!("no {method}"));
            if n.method == method {
                return n;
            }
        }
    }

    #[test]
    fn two_dashboards_hear_one_write() {
        let dir = scratch("two");
        let store = scratch("two-store").join("inbox.json");
        let daemon = spawn_daemon(&dir, Options { inbox: Some(store.clone()), ..opts(1, 200) });
        let (mut a, mut b) = (connect(&dir), connect(&dir));
        for conn in [&mut a, &mut b] {
            conn.call("subscribe", json!({"topics": ["inbox"]})).unwrap();
        }
        note(&store, "one");
        let id = ops::load(&store).unwrap().threads[0].id;
        drain_notes(&mut a, Duration::from_millis(700));
        drain_notes(&mut b, Duration::ZERO);
        // One write through the API, from `a`: both hear it.
        let r = a.call("inbox.thread.markRead", json!({ "thread": id })).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        wait_for(&mut a, "inbox.changed");
        wait_for(&mut b, "inbox.changed");
        drop((a, b));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    #[test]
    fn shown_lines_reach_inbox_subscribers_latest_first() {
        use std::os::unix::net::UnixStream;
        let shared = Arc::new(Shared::new(v(1), scratch("shown-store").join("inbox.json"), scratch("shown-store").join("state.toml"), fake_join()));
        let mut clients = Vec::new();
        for (id, topics) in [(1, json!(["inbox"])), (2, json!(["inbox"])), (3, json!(["instances"]))] {
            let (server, client) = UnixStream::pair().unwrap();
            shared.spawn_conn(id, server);
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let mut writer = client;
            writeln!(writer, "{}", json!({"id": 1, "method": "subscribe", "params": {"topics": topics}})).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["result"], json!({"ok": true}));
            reader.get_ref().set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            clients.push((reader, writer));
        }
        // Coalescing keeps the latest line when two land before a write;
        // either way the last one seen is the newest.
        shared.publish_shown("disp", "disp: first".into());
        shared.publish_shown("disp", "disp: second".into());
        for (reader, _) in &mut clients[..2] {
            let last = loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let n: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(n["method"], "inbox.shown", "{n}");
                assert_eq!(n["params"]["instance"], "disp");
                if n["params"]["line"] == "disp: second" {
                    break n;
                }
                assert_eq!(n["params"]["line"], "disp: first");
            };
            assert!(last.get("id").is_none());
        }
        // Not subscribed to `inbox`: nothing.
        let (reader, _) = &mut clients[2];
        reader.get_ref().set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut line = String::new();
        assert!(reader.read_line(&mut line).is_err() || line.is_empty(), "{line}");
    }

    /// A connection over a socket pair, subscribed to `topics`.
    fn subscribed(shared: &Arc<Shared>, id: u64, topics: Value) -> BufReader<std::os::unix::net::UnixStream> {
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        shared.spawn_conn(id, server);
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut writer = client;
        writeln!(writer, "{}", json!({"id": 1, "method": "subscribe", "params": {"topics": topics}})).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["result"], json!({"ok": true}));
        reader.get_ref().set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        // Dropping `writer` closes one fd; the reader's clone keeps the socket.
        reader
    }

    /// Send `method` on a [`subscribed`] connection; its answer, and the
    /// notifications that came before it.
    fn ask(reader: &mut BufReader<std::os::unix::net::UnixStream>, method: &str, params: Value) -> (Value, Vec<Value>) {
        writeln!(reader.get_ref(), "{}", json!({"id": 9, "method": method, "params": params})).unwrap();
        let mut notes = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id").is_some() {
                return (msg, notes);
            }
            notes.push(msg);
        }
    }

    /// The next `instances.snapshot` (from `notes` first), or `None` within
    /// `wait`.
    fn next_snapshot(reader: &mut BufReader<std::os::unix::net::UnixStream>, notes: &mut Vec<Value>, wait: Duration) -> Option<Value> {
        if let Some(i) = notes.iter().position(|n| n["method"] == "instances.snapshot") {
            return Some(notes.remove(i)["params"].clone());
        }
        reader.get_ref().set_read_timeout(Some(wait)).unwrap();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                return None;
            }
            let n: Value = serde_json::from_str(&line).unwrap();
            if n["method"] == "instances.snapshot" {
                return Some(n["params"].clone());
            }
        }
    }

    fn bare_shared(tag: &str) -> Arc<Shared> {
        let store = scratch(tag);
        Arc::new(Shared::new(v(1), store.join("inbox.json"), store.join("state.toml"), fake_join()))
    }

    /// `instances.attach` records while its connection holds, and only for a
    /// local client of this user.
    #[test]
    fn an_attach_records_while_its_connection_holds() {
        use crate::serve::attaches::Origin;
        use crate::serve::fleet::member::{self, Member};
        let store = scratch("attach-rec");
        std::fs::create_dir_all(&store).unwrap();
        let instance = "[instance.w]\ninstance_id = \"id-w\"\nsandbox = \"web\"\ncontainer = \"devsandbox-w\"\n\
                        folder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\n";
        std::fs::write(store.join("state.toml"), instance).unwrap();
        let mut s = Shared::new(v(1), store.join("inbox.json"), store.join("state.toml"), fake_join());
        s.member = Some(store.join("member.toml"));
        let shared = Arc::new(s);
        let window = crate::open::OPEN_ATTACH_WINDOW;

        let mut c = subscribed(&shared, 1, json!(["instances"]));
        let mut attach = |params: Value| ask(&mut c, "instances.attach", params).0;
        assert_eq!(attach(json!({"instance": "w"}))["result"], json!({"ok": true}));
        assert_eq!(attach(json!({"instance": "id-w"}))["result"], json!({"ok": true}), "by id");
        assert_eq!(attach(json!({"instance": "w-1", "machine": "box"}))["result"], json!({"ok": true}), "on a machine");
        assert_eq!(attach(json!({"instance": "nope"}))["error"]["code"], "not-found");
        assert_eq!(attach(json!({"instance": "w", "root": "/x"}))["error"]["code"], "invalid", "strict params");
        assert_eq!(attach(json!({}))["error"]["code"], "invalid");
        assert_eq!(attach(json!({"instance": ""}))["error"]["code"], "invalid");
        assert_eq!((shared.attaches.latest("id-w", Instant::now()), shared.attaches.live_in("id-w")), (Some(Origin::Here), true));
        assert!(shared.attaches.attached_out("box", "w-1", Instant::now() + 2 * window, window), "live");
        drop(c);
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.attaches.live_in("id-w") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!shared.attaches.live_in("id-w"), "the hang-up ended it");
        assert_eq!(shared.attaches.latest("id-w", Instant::now()), Some(Origin::Here), "ended, still the latest");
        assert!(shared.attaches.attached_out("box", "w-1", Instant::now(), window), "ended just now");
        assert!(!shared.attaches.attached_out("box", "w-1", Instant::now() + window, window), "ended");

        // A link peer, `worker` or `full`: never.
        let peer = |shared: &Arc<Shared>| {
            let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
            let p = lent::Peer { name: "lap".into(), fleet_root: None, env: None, agent: None, key: None, introduced_by: None };
            shared.spawn_conn_as(9, server, Table::Peer(Arc::new(p)));
            let reader = BufReader::new(client);
            reader.get_ref().set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            reader
        };
        let mut worker = peer(&shared);
        assert_eq!(ask(&mut worker, "instances.attach", json!({"instance": "w"})).0["error"]["code"], "denied");
        assert_eq!(ask(&mut worker, "instances.attach", json!({"instance": "w", "machine": "box"})).0["error"]["code"], "denied");
        member::write_for_test(&store.join("member.toml"), &Member { trust: crate::worker_policy::Trust::Full, ..Default::default() });
        let mut full = peer(&shared);
        assert_eq!(ask(&mut full, "instances.attach", json!({"instance": "id-w"})).0["error"]["code"], "denied");
        drop((worker, full));
        assert!(!shared.attaches.live_in("id-w"));
        let _ = std::fs::remove_dir_all(&store);
    }

    /// A connection whose only business is `instances.attach` (a hello
    /// aside) is no `clients` holder and doesn't hold up a drain; one that
    /// did anything else, before or after, is and does.
    #[test]
    fn an_attach_only_connection_holds_nothing() {
        let store = scratch("attach-only");
        std::fs::create_dir_all(&store).unwrap();
        let instance = "[instance.w]\ninstance_id = \"id-w\"\nsandbox = \"web\"\ncontainer = \"devsandbox-w\"\n\
                        folder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\n";
        std::fs::write(store.join("state.toml"), instance).unwrap();
        let shared = Arc::new(Shared::new(v(1), store.join("inbox.json"), store.join("state.toml"), fake_join()));
        let open = |id: u64| {
            let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
            shared.spawn_conn(id, server);
            let reader = BufReader::new(client);
            reader.get_ref().set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            reader
        };
        let clients = || shared.holders(None, None).clients;

        let mut attach_only = open(1);
        assert_eq!(clients(), 1, "not attached yet: a client");
        assert!(ask(&mut attach_only, "hello", json!({"version": "0.0.0", "build": 0, "client": "cli"})).0.get("error").is_none());
        assert_eq!(ask(&mut attach_only, "instances.attach", json!({"instance": "w"})).0["result"], json!({"ok": true}));
        assert_eq!(ask(&mut attach_only, "instances.attach", json!({"instance": "w-1", "machine": "box"})).0["result"], json!({"ok": true}));
        assert_eq!(clients(), 0, "attach-only");
        assert!(shared.attaches.live_in("id-w"));

        let mut busy_first = open(2);
        ask(&mut busy_first, "subscribe", json!({"topics": ["instances"]}));
        ask(&mut busy_first, "instances.attach", json!({"instance": "w"}));
        assert_eq!(clients(), 1, "it subscribed first");
        let mut busy_after = open(3);
        ask(&mut busy_after, "instances.attach", json!({"instance": "w"}));
        assert_eq!(clients(), 1);
        ask(&mut busy_after, "inbox.list", json!({}));
        assert_eq!(clients(), 2, "a call after the attach: a client again");
        drop((busy_first, busy_after));
        let deadline = Instant::now() + Duration::from_secs(3);
        while clients() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(clients(), 0);

        // The drain cuts it and doesn't wait on it.
        let started = Instant::now();
        shared.drain(DRAIN_TIMEOUT);
        assert!(started.elapsed() < DRAIN_TIMEOUT / 2, "{:?}", started.elapsed());
        let mut line = String::new();
        assert_eq!(attach_only.read_line(&mut line).unwrap(), 0, "cut");
        let _ = std::fs::remove_dir_all(&store);
    }

    /// A root dir that exists, for `instances.watch`.
    fn root(tag: &str) -> PathBuf {
        let dir = scratch(tag);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_new_watch_gets_a_disk_snapshot_then_only_its_roots() {
        let shared = bare_shared("snap-store");
        let (a, b) = (root("snap-a"), root("snap-b"));
        let mut on_a = subscribed(&shared, 1, json!(["instances"]));
        let mut on_b = subscribed(&shared, 2, json!(["instances"]));
        let mut idle = subscribed(&shared, 3, json!(["instances"]));

        let (r, mut notes) = ask(&mut on_a, "instances.watch", json!({"dir": a}));
        assert_eq!(r["result"], json!({"ok": true}), "{r}");
        let first = next_snapshot(&mut on_a, &mut notes, Duration::from_secs(3)).expect("no first snapshot");
        assert_eq!(first["dir"], a.to_str().unwrap());
        assert_eq!(first["runtime_name"], "Disk", "nothing cached: from disk");
        let (_, mut notes_b) = ask(&mut on_b, "instances.watch", json!({"dir": b}));
        next_snapshot(&mut on_b, &mut notes_b, Duration::from_secs(3)).expect("no first snapshot");
        assert_eq!(shared.watched_roots(), [a.clone(), b.clone()].into());

        // Two pushes before the notifier writes: the latest, or both in
        // order; never the older last. Only `a`'s watcher hears them.
        let push = |dir: &Path, n: u64| {
            let mut p = (*snapshots::params(dir, &fake_join()(dir, &crate::snapshot::RuntimeView::disk())).unwrap()).clone();
            p["sandbox_count"] = json!(n);
            shared.publish_snapshot(dir, Arc::new(p));
        };
        push(&a, 1);
        push(&a, 2);
        let mut none = Vec::new();
        let last = loop {
            let s = next_snapshot(&mut on_a, &mut none, Duration::from_secs(3)).expect("no push");
            if s["sandbox_count"] == 2 {
                break s;
            }
            assert_eq!(s["sandbox_count"], 1);
        };
        assert_eq!(last["dir"], a.to_str().unwrap());
        assert!(next_snapshot(&mut on_b, &mut none, Duration::from_millis(300)).is_none(), "b doesn't watch a");
        assert!(next_snapshot(&mut idle, &mut none, Duration::from_millis(100)).is_none(), "watches nothing");

        // Unwatch: nothing more.
        let (r, _) = ask(&mut on_a, "instances.unwatch", json!({"dir": a}));
        assert_eq!(r["result"], json!({"ok": true}));
        push(&a, 3);
        assert!(next_snapshot(&mut on_a, &mut none, Duration::from_millis(300)).is_none());
        assert_eq!(shared.watched_roots(), [b.clone()].into());

        // Unsubscribing from `instances` lets go of the watches too.
        let (r, _) = ask(&mut on_b, "unsubscribe", json!({"topics": ["instances"]}));
        assert!(r.get("error").is_none(), "{r}");
        assert!(shared.watched_roots().is_empty());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn a_watch_of_a_cached_root_gets_the_cached_snapshot() {
        let shared = bare_shared("snap-cache-store");
        let a = root("snap-cache");
        let view = |depth| crate::snapshot::RuntimeView { depth, ..crate::snapshot::RuntimeView::disk() };
        // What the poll left for another watcher.
        shared.snapshots.collect(&view(crate::snapshot::Depth::Full), &[a.clone()].into(), &*fake_join());
        let mut conn = subscribed(&shared, 1, json!(["instances"]));
        let (_, mut notes) = ask(&mut conn, "instances.watch", json!({"dir": a}));
        let first = next_snapshot(&mut conn, &mut notes, Duration::from_secs(3)).expect("no cached snapshot");
        assert_eq!(first["runtime_name"], "Full");
        let _ = std::fs::remove_dir_all(&a);
    }

    #[test]
    fn watches_need_instances_and_end_with_the_connection() {
        let dir = scratch("watch");
        let a = root("watch-root");
        let daemon = spawn_daemon(&dir, opts(1, 200));
        let mut conn = connect(&dir);
        let code = |r: proto::Response| r.error.map(|e| e.code);
        let r = conn.call("instances.watch", json!({"dir": a})).unwrap();
        let e = r.error.clone().unwrap();
        assert_eq!(e.code, "invalid");
        assert!(e.message.contains("subscribe to `instances`"), "{}", e.message);
        conn.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        assert_eq!(code(conn.call("instances.watch", json!({"dir": "rel"})).unwrap()).as_deref(), Some("invalid"));
        let gone = a.join("gone");
        assert_eq!(code(conn.call("instances.watch", json!({"dir": gone})).unwrap()).as_deref(), Some("not-found"));
        assert_eq!(code(conn.call("instances.unwatch", json!({"dir": gone})).unwrap()), None);

        // No host side: the watch still answers, with its disk snapshot.
        assert_eq!(conn.call("instances.watch", json!({"dir": a})).unwrap().result, Some(json!({"ok": true})));
        let n = wait_for(&mut conn, "instances.snapshot");
        assert_eq!(n.params["dir"], a.to_str().unwrap());
        dts_check(&n);

        // A hang-up lets go of the watch (and the daemon idles out).
        drop(conn);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&a);
    }

    fn dts_check(n: &Notification) {
        let ty = crate::serve::dts::prop_type("ApiEvents", &n.method).unwrap();
        crate::serve::dts::assert_matches(&ty, &n.params);
    }

    #[test]
    fn a_hang_up_releases_the_watch() {
        let shared = bare_shared("watch-hangup-store");
        let a = root("watch-hangup");
        let mut conn = subscribed(&shared, 1, json!(["instances"]));
        ask(&mut conn, "instances.watch", json!({"dir": a}));
        assert_eq!(shared.watched_roots(), [a.clone()].into());
        drop(conn);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !shared.watched_roots().is_empty() {
            assert!(Instant::now() < deadline, "still watched after the hang-up");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&a);
    }

    #[test]
    fn forwards_subscribers_hear_changes_and_status_lines() {
        let shared = Arc::new(Shared::new(v(1), scratch("fwd-store").join("inbox.json"), scratch("fwd-store").join("state.toml"), fake_join()));
        let mut fwd = subscribed(&shared, 1, json!(["forwards"]));
        let mut other = subscribed(&shared, 2, json!(["inbox", "instances"]));
        shared.publish_forwards(forwards::Event::Changed);
        let mut line = String::new();
        fwd.read_line(&mut line).unwrap();
        let n: Value = serde_json::from_str(&line).unwrap();
        assert_eq!((n["method"].as_str(), &n["params"]), (Some("forwards.changed"), &json!({})));
        shared.publish_forwards(forwards::Event::Status("3000: connection refused".into()));
        line.clear();
        fwd.read_line(&mut line).unwrap();
        let n: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(n["method"], "forwards.status");
        assert_eq!(n["params"]["line"], "3000: connection refused");
        // Not subscribed to `forwards`: nothing.
        other.get_ref().set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        line.clear();
        assert!(other.read_line(&mut line).is_err() || line.is_empty(), "{line}");
    }

    /// The daemon's `OnDone` pushes `opens.done` to `opens` subscribers
    /// only, each open its own notification, typed as the npm package says.
    #[test]
    fn opens_subscribers_hear_every_outcome() {
        use crate::open::{Loopback, OpenDone, Outcome};
        let shared = Arc::new(Shared::new(v(1), scratch("opens-store").join("inbox.json"), scratch("opens-store").join("state.toml"), fake_join()));
        let mut opens = subscribed(&shared, 1, json!(["opens"]));
        let mut other = subscribed(&shared, 2, json!(["inbox", "instances", "forwards"]));
        let done = open_done(&shared);
        let at = |outcome| OpenDone { instance: "web".into(), url: "https://x.example/".into(), profile: Some("work".into()), outcome };
        done(at(Outcome::Opened { unforwarded: vec![Loopback { port: 8085, callback: true }] }));
        done(OpenDone { profile: None, ..at(Outcome::Refused("rate limit (5 per 10s)".into())) });
        let mut seen = Vec::new();
        while seen.len() < 2 {
            let mut line = String::new();
            opens.read_line(&mut line).unwrap();
            let n: Notification = serde_json::from_str(&line).unwrap();
            assert_eq!(n.method, "opens.done");
            dts_check(&n);
            crate::serve::dts::assert_matches("OpensDone", &n.params);
            seen.push(n.params);
        }
        assert_eq!(
            seen[0],
            json!({"instance": "web", "url": "https://x.example/", "profile": "work", "outcome": "opened",
                "why": "cannot forward callback port 8085",
                "line": "web: opened https://x.example/ (work); cannot forward callback port 8085"})
        );
        assert_eq!(
            seen[1],
            json!({"instance": "web", "url": "https://x.example/", "profile": null, "outcome": "refused",
                "why": "rate limit (5 per 10s)", "line": "web: open refused: rate limit (5 per 10s)"})
        );
        // Not subscribed to `opens`: nothing.
        other.get_ref().set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut line = String::new();
        assert!(other.read_line(&mut line).is_err() || line.is_empty(), "{line}");
        // A stopped daemon's hook is a no-op, not a leak.
        drop(shared);
        done(at(Outcome::Failed("x".into())));
    }

    /// Unwritten `opens.done` pile up to [`OPENS_KEPT`], oldest dropped, and
    /// go out in order; an unsubscribe drops them.
    #[test]
    fn pending_opens_keep_the_last_few_in_order() {
        let mut p = Pending::default();
        assert!(!p.any());
        for n in 0..OPENS_KEPT + 3 {
            p.push_open(json!({"n": n}));
        }
        assert!(p.any());
        let out = p.take();
        let ns: Vec<u64> = out.iter().map(|n| n.params["n"].as_u64().unwrap()).collect();
        assert_eq!(ns, (3..OPENS_KEPT as u64 + 3).collect::<Vec<_>>());
        assert!(out.iter().all(|n| n.method == "opens.done"));
        assert!(!p.any() && p.take().is_empty());
    }

    #[test]
    fn active_forwards_are_holders() {
        let shared = Shared::new(v(1), scratch("fwd-hold").join("inbox.json"), scratch("fwd-hold").join("state.toml"), fake_join());
        let registry = Registry::spawn(
            |_| {},
            scratch("fwd-hold-store").join("forwards.toml"),
            scratch("fwd-hold-store").join("machines.toml"),
            Arc::default(),
            Arc::new(Vec::new),
            scratch("fwd-hold-store"),
            Box::new(|_| {}),
        );
        assert_eq!(shared.holders(None, Some(&registry)), Holders::default());
        let dir = scratch("fwd-hold-root");
        std::fs::create_dir_all(&dir).unwrap();
        let req = forwards::AddRequest {
            dir: dir.clone(),
            instance: None,
            service: Some("db".into()),
            bind: std::net::Ipv4Addr::LOCALHOST.into(),
            host_port: crate::devsbd::forward::HostPort::Prefer(0),
            container_port: 5432,
            machine: None,
        };
        let h = registry.handle();
        let added = api::Forwarding::add(&h, req).unwrap();
        // A request behind the post-command poll: the count is current.
        api::Forwarding::list(&h, None, false).unwrap();
        let holders = shared.holders(None, Some(&registry));
        assert_eq!(holders, Holders { forwards: 1, ..Default::default() });
        assert!(holders.any());
        api::Forwarding::rm(&h, added.id).unwrap();
        api::Forwarding::list(&h, None, false).unwrap();
        assert!(!shared.holders(None, Some(&registry)).any());
        drop(registry);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subscribers_get_a_closing_hint_on_handoff() {
        let dir = scratch("closing");
        let daemon = spawn_daemon(&dir, opts(1, 60_000));
        let mut conn = connect(&dir);
        conn.call("subscribe", json!({"topics": ["inbox", "instances"]})).unwrap();
        let (_newer, h) = hello_at(&dir, &v(2));
        assert!(matches!(h, Hello::Handoff));
        let closing = loop {
            let n = conn.next_notification(Duration::from_secs(3)).unwrap().expect("no closing hint");
            if n.method == "closing" {
                break n;
            }
        };
        assert_eq!(closing.params["reason"], "handoff");
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_answers_drains_and_exits() {
        let dir = scratch("shutdown");
        let daemon = spawn_daemon(&dir, Options { keep_alive: true, ..opts(1, 50) });
        let mut sub = connect(&dir);
        sub.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        let (mut idle_client, _) = hello_at(&dir, &v(1));

        let started = Instant::now();
        let mut conn = connect(&dir);
        let r = conn.call("shutdown", json!({})).unwrap();
        assert_eq!(r.result, Some(json!({"ok": true})), "{r:?}");
        // The answer goes out once nothing answers on the socket any more.
        assert!(endpoint::connect(&dir).is_err());
        let closing = wait_for(&mut sub, "closing");
        assert_eq!(closing.params, json!({"reason": "shutdown"}));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        // Idle connections are closed, not waited out.
        assert!(started.elapsed() < DRAIN_TIMEOUT);
        let mut buf = [0u8; 1];
        assert_eq!(idle_client.read(&mut buf).unwrap(), 0);
        assert!(!endpoint::socket_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every notification is an `ApiEvents` key in the npm typings, its
    /// params matching the interface it names (`serve::dts`).
    #[test]
    fn npm_typings_match_every_notification() {
        use crate::serve::dts;
        let mut p = Pending {
            inbox: Some(1),
            shown: Some(("disp".into(), "disp: hi".into())),
            instances: true,
            forwards: true,
            forwards_status: Some("forwarding".into()),
            closing: Some("idle"),
            ..Pending::default()
        };
        let dir = PathBuf::from("/cfg");
        let list = fake_join()(&dir, &crate::snapshot::RuntimeView::disk());
        p.snapshots.insert(dir.clone(), snapshots::params(&dir, &list).unwrap());
        let notes = p.take();
        assert_eq!(notes.len(), 7, "a Pending flag isn't covered here");
        for n in notes {
            let ty = dts::prop_type("ApiEvents", &n.method)
                .unwrap_or_else(|| panic!("index.d.ts `ApiEvents` lacks `'{}'`", n.method));
            if ty == "{}" {
                assert_eq!(n.params, json!({}), "{}", n.method);
            } else {
                dts::assert_matches(&ty, &n.params);
            }
        }
        // A `machines: true` watcher's variant.
        let machine = snapshots::Machine { name: "m".into(), target: Some("me@m".into()), ..Default::default() };
        let merged = snapshots::merge(&snapshots::params(&dir, &list).unwrap(), &[machine], None, &Default::default(), &Default::default());
        dts::assert_matches(&dts::prop_type("ApiEvents", "instances.snapshot").unwrap(), &merged);
        dts::assert_matches("SnapshotMachine", &merged["machines"][0]);
    }

    /// A daemon whose verbs are one script: `$*` (the argv) picks the case.
    fn verb_daemon(tag: &str, idle_ms: u64, script: &str) -> (PathBuf, PathBuf, JoinHandle<Result<Exit>>) {
        let dir = scratch(tag);
        let (runner, root) = verbs::tests::script_runner(tag, script);
        let daemon = spawn_daemon(&dir, Options { verbs: Some(runner), ..opts(1, idle_ms) });
        (dir, root, daemon)
    }

    fn verb_line(id: u64, method: &str, root: &Path, extra: Value) -> String {
        let mut params = json!({"root": root, "env": {"PATH": std::env::var("PATH").unwrap_or_default()}});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        format!("{}\n", json!({"id": id, "method": method, "params": params}))
    }

    fn wait_file(path: &Path, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while !path.exists() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    const VERB_SCRIPT: &str = r#"case "$*" in
        *slow*) trap 'echo int > "$DIR/int"; exit 130' INT; echo ready; sleep 2; touch "$DIR/done" ;;
        *start*) sleep 1; touch "$DIR/started" ;;
        *) echo "hi $*"; echo 'Error: nope' >&2; exit 2 ;;
    esac"#;

    #[test]
    fn verbs_stream_over_the_socket_and_a_hang_up_cancels() {
        let (dir, root, daemon) = verb_daemon("verb-sock", 200, VERB_SCRIPT);
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        // A request pipelined behind the verb is neither a hang-up nor lost.
        let lines = verb_line(1, "stop", &root, json!({"instance": "fast"})) + r#"{"id":2,"method":"nope"}"# + "\n";
        writer.write_all(lines.as_bytes()).unwrap();
        let mut seen = Vec::new();
        let answer = loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id").is_some() {
                break msg;
            }
            seen.push(msg);
        };
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["error"], json!({"code": "failed", "message": "nope", "data": {"exit": 2, "instance": null}}));
        let out: String = seen
            .iter()
            .filter(|n| n["method"] == "output" && n["params"]["stream"] == "stdout")
            .map(|n| n["params"]["data"].as_str().unwrap())
            .collect();
        assert!(out.starts_with("hi --in-process -C "), "{out}");
        assert!(seen.iter().all(|n| n["params"]["request"] == 1), "{seen:?}");
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["error"]["code"], "unknown-method");

        // Hang up mid-verb: SIGINT to its group.
        writer.write_all(verb_line(3, "stop", &root, json!({"instance": "slow"})).as_bytes()).unwrap();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.contains("ready") {
                break;
            }
        }
        drop((reader, writer));
        assert!(wait_file(&root.join("int"), Duration::from_secs(3)), "no SIGINT");
        assert!(!root.join("done").exists());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `helper.call` over the socket: records as `helper.record`, the last
    /// one the answer, request errors before any stream, and a hang-up
    /// mid-call cancels it.
    #[test]
    fn helper_call_streams_records_and_a_hang_up_cancels() {
        use crate::devsbd::proto::caps;
        use crate::serve::helper::tests::{Fake, logged, respond};
        let dir = scratch("helper-call");
        let state_dir = scratch("helper-call-state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = state_dir.join("state.toml");
        std::fs::write(
            &state,
            "[instance.web]\nsandbox = \"web\"\ncontainer = \"devsandbox-hc-web\"\nfolder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\n",
        )
        .unwrap();
        let fake = Fake::default();
        fake.add("devsandbox-hc-web", caps::RPC, 1, respond);
        let client = helper::HelperClient::new(fake, crate::serve::helper::tests::log);
        let daemon = spawn_daemon(&dir, Options { helper: Some(client), state: Some(state), ..opts(1, 200) });
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let line = |id: u64, params: Value| format!("{}\n", json!({"id": id, "method": "helper.call", "params": params}));
        let lines = line(1, json!({"instance": "web", "method": "chatty"}))
            + &line(2, json!({"instance": "nope", "method": "chatty"}))
            + &line(3, json!({"instance": "web", "method": "x", "machine": "box"}));
        writer.write_all(lines.as_bytes()).unwrap();
        let mut next = || {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        };
        let mut notes = Vec::new();
        let answer = loop {
            let msg = next();
            if msg.get("id").is_some() {
                break msg;
            }
            notes.push(msg);
        };
        assert_eq!(answer, json!({"id": 1, "result": {"exit": 3}}));
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(notes[0], json!({"method": "helper.record", "params": {"request": 1, "record": "stdout", "data": "hi\n"}}));
        assert_eq!(next()["error"]["code"], "not-found");
        assert_eq!(next()["error"]["code"], "invalid");
        assert!(logged("helper devsandbox-hc-web: chatty from api:test: exit 3"), "caller is the connection's client");

        writer.write_all(line(4, json!({"instance": "web", "method": "hang"})).as_bytes()).unwrap();
        assert_eq!(next()["params"]["data"], "working");
        drop((reader, writer));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !logged("helper devsandbox-hc-web: hang from api:test: cancelled") {
            assert!(Instant::now() < deadline, "not cancelled");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// The dashboard's held verbs (`stop`, `rm`, `services.rebuild`) carry
    /// `detach`: a quit mid-verb leaves it running to its exit.
    #[test]
    fn a_detached_held_verb_survives_a_hang_up() {
        let (dir, root, daemon) = verb_daemon("verb-held-detach", 150, VERB_SCRIPT);
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        writer.write_all(verb_line(1, "stop", &root, json!({"instance": "slow", "detach": true})).as_bytes()).unwrap();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(!line.contains(r#""id":1"#), "answered early: {line}");
            if line.contains("ready") {
                break;
            }
        }
        drop((reader, writer));
        assert!(wait_file(&root.join("done"), Duration::from_secs(5)), "the verb didn't finish");
        assert!(!root.join("int").exists(), "SIGINTed");
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_detached_verb_holds_the_daemon_until_reaped() {
        let (dir, root, daemon) = verb_daemon("verb-hold", 150, VERB_SCRIPT);
        let mut conn = connect(&dir);
        let env = json!({"PATH": std::env::var("PATH").unwrap_or_default()});
        let r = conn.call("start", json!({"root": root, "env": env, "all": true, "detach": true})).unwrap();
        assert_eq!(r.result, Some(json!({"instance": null, "detached": true})), "{r:?}");
        drop(conn);
        std::thread::sleep(Duration::from_millis(600));
        assert!(!daemon.is_finished(), "idled out with a verb in flight");
        assert!(!root.join("started").exists());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        assert!(root.join("started").exists(), "exited before the verb did");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_drain_leaves_a_verb_running() {
        let (dir, root, daemon) = verb_daemon("verb-drain", 60_000, VERB_SCRIPT);
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        writer.write_all(verb_line(1, "stop", &root, json!({"instance": "slow"})).as_bytes()).unwrap();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.contains("ready") {
                break;
            }
        }
        let started = Instant::now();
        let r = connect(&dir).call("shutdown", json!({})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        // The drain didn't wait the verb out, and didn't cancel it.
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
        let mut rest = String::new();
        let _ = reader.read_to_string(&mut rest);
        assert!(!rest.contains("\"id\":1"), "answered: {rest}");
        assert!(wait_file(&root.join("done"), Duration::from_secs(4)), "the verb didn't finish");
        assert!(!root.join("int").exists(), "the drain cancelled it");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_predecessors_live_verb_is_adopted_and_holds_until_it_exits() {
        let tag = "verb-adopt";
        let dir = scratch(tag);
        let (runner, root) = verbs::tests::script_runner(tag, "exit 0");
        let spool = runner.spool.clone();
        std::fs::create_dir_all(&spool).unwrap();
        let mut live = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let mut dead = std::process::Command::new("true").spawn().unwrap();
        dead.wait().unwrap();
        let files = |pid: u32| [spool.join(format!("{pid}.out")), spool.join(format!("{pid}.err"))];
        for path in files(live.id()).iter().chain(&files(dead.id())) {
            std::fs::write(path, "x").unwrap();
        }
        let daemon = spawn_daemon(&dir, Options { verbs: Some(runner), ..opts(1, 150) });
        // Seen through the socket: the sweep ran before the listener answers.
        let _ = hello_at(&dir, &v(1));
        assert!(files(dead.id()).iter().all(|p| !p.exists()), "a dead verb's files were kept");
        std::thread::sleep(Duration::from_millis(500));
        assert!(!daemon.is_finished(), "idled out with an adopted verb running");
        assert!(files(live.id()).iter().all(|p| p.exists()));
        // Reaped here (a zombie is still alive to `kill(0)`).
        live.kill().unwrap();
        live.wait().unwrap();
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        assert!(files(live.id()).iter().all(|p| !p.exists()), "an adopted verb's files were kept");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_connected_client_holds_and_keep_alive_never_idles() {
        let dir = scratch("hold");
        let daemon = spawn_daemon(&dir, opts(1, 150));
        let (conn, _) = hello_at(&dir, &v(1));
        std::thread::sleep(Duration::from_millis(500));
        assert!(!daemon.is_finished(), "exited while a client was connected");
        drop(conn);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);

        let keep = Options { keep_alive: true, ..opts(1, 50) };
        let daemon = spawn_daemon(&dir, keep);
        let _ = hello_at(&dir, &v(1));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!daemon.is_finished(), "keep-alive daemon idled out");
        // Stop it with a handoff (`shutdown` is the other way, tested above).
        let (_c, h) = hello_at(&dir, &v(2));
        assert!(matches!(h, Hello::Handoff));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `link.attach {channel: api}` turns its connection into a `Peer` one: the
    /// trust table from there on, read per request from `member.toml`.
    #[test]
    fn an_attached_connection_gets_the_trust_table() {
        use crate::serve::fleet::member::{self, Member};
        let dir = scratch("attach");
        let data = scratch("attach-data");
        let (own, other) = (data.join("own"), data.join("other"));
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let (own, other) = (std::fs::canonicalize(own).unwrap(), std::fs::canonicalize(other).unwrap());
        let state = data.join("state.toml");
        let instance = "[instance.w]\ninstance_id = \"id-w\"\nsandbox = \"web\"\ncontainer = \"devsandbox-w\"\n\
                        folder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\n";
        std::fs::write(&state, format!("{instance}config_dir = {:?}\n", own.to_str().unwrap())).unwrap();
        let options = Options { state: Some(state), member: Some(data.join("member.toml")), ..opts(1, 1000) };
        let daemon = spawn_daemon(&dir, options);

        let (mut stream, answer) = attach_raw(&dir, json!({"peer": "lap.local", "channel": "api", "key": attach_key()}), b"");
        assert_eq!(answer["result"]["trust"], "worker", "{answer}");
        let Hello::Ready(hello) = client::hello(&mut stream, &v(1), "machine-link").unwrap() else { panic!("no hello") };
        let mut conn = client::Conn::new(stream, hello).unwrap();
        let code = |conn: &mut client::Conn, method: &str, params: Value| conn.call(method, params).unwrap().error.map(|e| e.code);

        assert_eq!(code(&mut conn, "member.get", json!({})), Some("denied".into()));
        assert_eq!(code(&mut conn, "instances.list", json!({"dir": own})), None, "an own root");
        assert_eq!(code(&mut conn, "instances.list", json!({"dir": other})), Some("denied".into()));
        assert_eq!(code(&mut conn, "machines.list", json!({})), Some("denied".into()));
        assert_eq!(code(&mut conn, "link.attach", json!({"peer": "again", "channel": "api", "key": attach_key()})), Some("denied".into()), "one-way");
        // `code.haves`: about this peer's own fleet root only.
        for (peer, hash) in [("lap.local", "0123456789abcdef"), ("other", "fedcba9876543210")] {
            std::fs::create_dir_all(data.join("fleet").join(peer).join("roots").join(hash)).unwrap();
        }
        let r = conn.call("code.haves", json!({"repos": ["00000000000000aa"]})).unwrap();
        assert_eq!(r.result, Some(json!({"roots": ["0123456789abcdef"], "tips": {}})), "{r:?}");
        assert_eq!(code(&mut conn, "code.haves", json!({"repos": ["../x"]})), Some("invalid".into()));

        // `full` is read per request: the same connection opens up, but
        // `member.*` and `link.*` stay local.
        member::write_for_test(&data.join("member.toml"), &Member { trust: crate::worker_policy::Trust::Full, ..Default::default() });
        assert_eq!(code(&mut conn, "machines.list", json!({})), None);
        assert_eq!(code(&mut conn, "instances.list", json!({"dir": other})), None);
        assert_eq!(code(&mut conn, "member.get", json!({})), Some("denied".into()));
        assert_eq!(code(&mut conn, "membership.get", json!({})), Some("denied".into()));
        assert_eq!(code(&mut conn, "link.attach", json!({"peer": "again", "channel": "api", "key": attach_key()})), Some("denied".into()));
        let r = conn.call("code.haves", json!({"repos": []})).unwrap();
        assert_eq!(r.result, Some(json!({"roots": ["0123456789abcdef"], "tips": {}})), "full: still its own fleet root: {r:?}");
        drop(conn);

        // A bad `peer`, or no `channel`, leaves the connection as it was.
        let mut plain = connect(&dir);
        assert_eq!(code(&mut plain, "link.attach", json!({"peer": "", "channel": "api", "key": attach_key()})), Some("invalid".into()));
        assert_eq!(code(&mut plain, "link.attach", json!({"peer": "lap"})), Some("invalid".into()));
        // No proven key: no link stream.
        assert_eq!(code(&mut plain, "link.attach", json!({"peer": "lap", "channel": "api"})), Some("invalid".into()));
        assert_eq!(code(&mut plain, "member.get", json!({})), None, "still a local client");
        assert_eq!(code(&mut plain, "code.haves", json!({"repos": []})), Some("denied".into()), "a local client has no fleet root");
        drop(plain);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A member key, as `link.attach`'s `key` (the one the link proved).
    fn attach_key() -> String {
        use crate::serve::fleet::identity::{Signer, SigningKey};
        SigningKey::from_seed([7; 32]).public_key().to_string()
    }

    /// A raw connection to `dir` (retrying while the daemon binds) whose
    /// first line is `link.attach` with `params`, `extra` in the same
    /// write; the answer read byte by byte, so nothing past it is taken.
    fn attach_raw(dir: &Path, params: Value, extra: &[u8]) -> (Stream, Value) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            if let Ok(s) = endpoint::connect(dir) {
                break s;
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        };
        let line = format!("{}\n", json!({"id": 0, "method": "link.attach", "params": params}));
        stream.write_all(&[line.as_bytes(), extra].concat()).unwrap();
        let mut answer = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
            answer.push(byte[0]);
        }
        (stream, serde_json::from_slice(&answer).unwrap())
    }

    /// `data` with `own` and `other` roots (canonical) and a `state.toml`
    /// whose one instance `w` is in `own`.
    fn attach_fixture(data: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let (own, other) = (data.join("own"), data.join("other"));
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let (own, other) = (std::fs::canonicalize(own).unwrap(), std::fs::canonicalize(other).unwrap());
        let state = data.join("state.toml");
        let instance = "[instance.w]\ninstance_id = \"id-w\"\nsandbox = \"web\"\ncontainer = \"devsandbox-w\"\n\
                        folder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\n";
        std::fs::write(&state, format!("{instance}config_dir = {:?}\n", own.to_str().unwrap())).unwrap();
        (state, own, other)
    }

    /// `link.attach {channel}`: the peer's fleet root `fleet/<peer>/` is
    /// created and reached under `worker`; `api` stays an API connection,
    /// `transfer` lands there; a bad name, an unknown channel, bytes past
    /// the line or a subscribed connection are refused.
    #[test]
    fn a_channel_attach_is_the_peers_with_its_fleet_root() {
        let dir = scratch("attach-ch");
        let data = scratch("attach-ch-data");
        std::fs::create_dir_all(&data).unwrap();
        let (state, own, other) = attach_fixture(&data);
        let options = Options { state: Some(state), member: Some(data.join("member.toml")), ..opts(1, 1000) };
        let daemon = spawn_daemon(&dir, options);
        let fleet = data.join("fleet/hub");

        let (mut stream, answer) = attach_raw(&dir, json!({"peer": "hub", "channel": "api", "key": attach_key()}), b"");
        assert_eq!(answer, json!({"id": 0, "result": {"ok": true, "trust": "worker"}}));
        assert!(fleet.is_dir(), "the fleet root is created");
        let cfg = fleet.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let Hello::Ready(hello) = client::hello(&mut stream, &v(1), "fleet-link").unwrap() else { panic!("no hello") };
        let mut conn = client::Conn::new(stream, hello).unwrap();
        let code = |conn: &mut client::Conn, method: &str, params: Value| conn.call(method, params).unwrap().error.map(|e| e.code);
        assert_eq!(code(&mut conn, "instances.list", json!({"dir": cfg})), None, "under the fleet root");
        assert_eq!(code(&mut conn, "instances.list", json!({"dir": own})), None, "an own root");
        assert_eq!(code(&mut conn, "instances.list", json!({"dir": other})), Some("denied".into()));
        assert_eq!(code(&mut conn, "member.get", json!({})), Some("denied".into()));
        drop(conn);

        // A transfer: a root tar, unpacked under `fleet/hub/roots/`.
        use crate::serve::fleet::{code as fcode, transfer};
        let src = data.join("src/cfg");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("devsandboxes.toml"), "[sandbox.web]\nimage = \"img\"\n").unwrap();
        let tar = fcode::pack_root(&src, Path::new("cfg")).unwrap();
        let (size, sha256) = transfer::digest(&tar[..]).unwrap();
        let header = transfer::Header {
            kind: fcode::ROOT_TAR_KIND.into(),
            name: format!("root-{}.tar", fcode::short_hash(&tar)),
            size,
            sha256,
            to: None,
            repo: None,
            refs: None,
        };
        let (stream, answer) = attach_raw(&dir, json!({"peer": "hub", "channel": "transfer", "key": attach_key()}), b"");
        assert_eq!(answer["result"]["ok"], true, "{answer}");
        transfer::send(stream, &header, &tar[..]).unwrap();
        let landed = fleet.join("roots").join(fcode::short_hash(&tar)).join("cfg/devsandboxes.toml");
        assert_eq!(std::fs::read_to_string(landed).unwrap(), "[sandbox.web]\nimage = \"img\"\n");

        // Refusals leave a local connection as it was.
        let refused = |params: Value, extra: &[u8]| {
            let (stream, answer) = attach_raw(&dir, params, extra);
            drop(stream);
            answer["error"]["code"].as_str().map(str::to_string)
        };
        assert_eq!(refused(json!({"peer": "a/b", "channel": "api", "key": attach_key()}), b""), Some("invalid".into()), "not a machine name");
        assert_eq!(refused(json!({"peer": "hub", "channel": "rpc", "key": attach_key()}), b""), Some("invalid".into()));
        assert!(!data.join("fleet/a").exists());
        assert_eq!(
            refused(json!({"peer": "hub", "channel": "tunnel", "key": attach_key()}), b"{\"root\":\"/\"}\n"),
            Some("invalid".into()),
            "bytes past the attach line"
        );
        let mut subscribed = connect(&dir);
        assert!(subscribed.call("subscribe", json!({"topics": ["inbox"]})).unwrap().error.is_none());
        let r = subscribed.call("link.attach", json!({"peer": "hub", "channel": "pty", "key": attach_key()})).unwrap();
        assert_eq!(r.error.map(|e| e.code), Some("invalid".into()), "a subscribed connection");
        assert_eq!(subscribed.call("member.get", json!({})).unwrap().error, None, "still a local client");
        drop(subscribed);

        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// `pty` and `tunnel` streams of an attached peer go through its reach:
    /// a root outside it is refused with `denied`, before any helper or
    /// runtime is touched.
    #[test]
    fn pty_and_tunnel_channels_are_confined_to_the_peers_reach() {
        use crate::devsbd::rpc::{self, Call, Obj, Record};
        let dir = scratch("attach-pty");
        let data = scratch("attach-pty-data");
        std::fs::create_dir_all(&data).unwrap();
        let (state, _own, other) = attach_fixture(&data);
        let helper = helper::HelperClient::new(crate::serve::helper::tests::Fake::default(), crate::serve::helper::tests::log);
        let options = Options { state: Some(state), member: Some(data.join("member.toml")), helper: Some(helper), ..opts(1, 1000) };
        let daemon = spawn_daemon(&dir, options);
        let other = other.to_str().unwrap();

        let (stream, answer) = attach_raw(&dir, json!({"peer": "hub", "channel": "pty", "key": attach_key()}), b"");
        assert_eq!(answer["result"]["trust"], "worker", "{answer}");
        let params = Obj::new().with("root", other).with("instance", "w").with("session", "1700000000-ab12");
        let header = Record::Call(Call { method: crate::serve::fleet::pty::METHOD.into(), params, meta: Obj::new() });
        rpc::write_record(&mut &stream, &header).unwrap();
        match rpc::read_record(&mut &stream).unwrap() {
            Some(Record::Error { code, .. }) => assert_eq!(code, "denied"),
            other => panic!("{other:?}"),
        }
        drop(stream);

        let (mut stream, answer) = attach_raw(&dir, json!({"peer": "hub", "channel": "tunnel", "key": attach_key()}), b"");
        assert_eq!(answer["result"]["ok"], true, "{answer}");
        stream.write_all(format!("{}\n", json!({"root": other, "instance": "w", "port": 80})).as_bytes()).unwrap();
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).unwrap();
        assert!(reply.contains("error") && reply.contains(other), "{reply}");
        drop(stream);

        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// An `agent` stream gets this host's agent only while member.toml
    /// grants it to the key the peer proved: else `NOT_SERVING` and the
    /// close.
    #[test]
    fn an_agent_channel_follows_the_grant_of_the_peers_key() {
        use crate::serve::fleet::agent::{NOT_SERVING, SERVING};
        let dir = scratch("attach-agent");
        let data = scratch("attach-agent-data");
        std::fs::create_dir_all(&data).unwrap();
        let agent = data.join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&agent).unwrap();
        std::thread::spawn(move || {
            for c in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut &c, &mut &c);
                });
            }
        });
        let host_agent: HostAgent = {
            let agent = agent.clone();
            Arc::new(move || Some(agent.clone()))
        };
        let options = Options {
            machines: Some(data.join("machines.toml")),
            member: Some(data.join("member.toml")),
            host_agent: Some(host_agent),
            ..opts(1, 1000)
        };
        let daemon = spawn_daemon(&dir, options);
        let status = |stream: &mut Stream| {
            let mut b = [0u8; 1];
            stream.read_exact(&mut b).unwrap();
            b[0]
        };
        let closed = |stream: &mut Stream| stream.read(&mut [0u8; 1]).unwrap() == 0;

        use crate::serve::fleet::grants;
        use crate::serve::fleet::identity::{Signer, SigningKey};
        use crate::serve::fleet::membership_api::test_support;
        let member = data.join("member.toml");
        let pk = |n: u8| SigningKey::from_seed([n; 32]).public_key();
        let attached = || {
            let (mut stream, answer) = attach_raw(&dir, json!({"peer": "hub", "channel": "agent", "key": attach_key()}), b"");
            assert_eq!(answer["result"]["ok"], true, "{answer}");
            let got = status(&mut stream);
            (stream, got)
        };
        // No log, no grant; then a log (`hub` is key 7) and no grant, a
        // grant for another member, one for `hub` by another key.
        let (mut stream, got) = attached();
        assert_eq!(got, NOT_SERVING, "nothing");
        assert!(closed(&mut stream));
        grants::grant(&member, "hub", pk(7)).unwrap();
        let (mut stream, got) = attached();
        assert_eq!(got, NOT_SERVING, "a grant but no log");
        assert!(closed(&mut stream));
        grants::withdraw(&member, "hub").unwrap();
        test_support::write(&member, &test_support::fleet("me", &SigningKey::from_seed([9; 32]), "me", &[("hub", pk(7)), ("other", pk(8))]));
        for (name, key) in [(None, 0), (Some("other"), 8), (Some("hub"), 8)] {
            if let Some(name) = name {
                grants::grant(&member, name, pk(key)).unwrap();
            }
            let (mut stream, got) = attached();
            assert_eq!(got, NOT_SERVING, "{name:?} {key}");
            assert!(closed(&mut stream));
        }

        grants::grant(&member, "hub", pk(7)).unwrap();
        let (mut stream, _) = attach_raw(&dir, json!({"peer": "hub", "channel": "agent", "key": attach_key()}), b"");
        assert_eq!(status(&mut stream), SERVING);
        stream.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        stream.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"ping", "relayed to the agent");
        drop(stream);

        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// `hub = "self"` in member.toml implies keep-alive.
    #[test]
    fn a_hub_never_idles() {
        use crate::serve::fleet::member::{self, Member};
        let dir = scratch("hub-idle");
        let data = scratch("hub-idle-data");
        member::write_for_test(&data.join("member.toml"), &Member { hub: "self".into(), ..Default::default() });
        let hub = Options { remotes: Some(data.join("remotes.json")), member: Some(data.join("member.toml")), ..opts(1, 50) };
        let daemon = spawn_daemon(&dir, hub);
        let _ = hello_at(&dir, &v(1));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!daemon.is_finished(), "a hub idled out");
        assert!(connect(&dir).call("shutdown", json!({})).unwrap().error.is_none());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// An adopted membership log flips the hub under a running daemon: no
    /// longer the hub lets the daemon idle out again; a change sends
    /// `links.cycle` on each held `links.inbound` connection (and nothing on
    /// a repeat).
    #[test]
    fn a_hub_change_cycles_the_inbound_links_and_the_keep_alive_follows() {
        use crate::serve::fleet::identity::{Signer, SigningKey};
        use crate::serve::fleet::member::{self, Member};
        use crate::serve::fleet::membership::Op;
        use crate::serve::fleet::membership_api::test_support;
        let dir = scratch("hub-flip");
        let data = scratch("hub-flip-data");
        let member_toml = data.join("member.toml");
        member::write_for_test(&member_toml, &Member { name: "lap".into(), hub: "self".into(), ..Default::default() });
        // `lap` the authority and the hub; then `box` added and made the hub.
        let lap = test_support::key_of(&member_toml);
        let box_key = SigningKey::from_seed([2; 32]).public_key();
        let first = test_support::fleet("lap", &lap, "lap", &[]);
        let mut second = first.clone();
        second.log.append(Op::Add { name: "box".into(), key: box_key }, "lap", &lap).unwrap();
        second.log.append(Op::Hub { name: "box".into() }, "lap", &lap).unwrap();
        assert_ne!(lap.public_key(), box_key);
        let options = Options {
            remotes: Some(data.join("remotes.json")),
            member: Some(data.join("member.toml")),
            machines: Some(data.join("machines.toml")),
            ..opts(1, 300)
        };
        let daemon = spawn_daemon(&dir, options);
        let mut inbound = connect(&dir);
        let r = inbound.call("links.inbound", json!({"peer": "lap", "version": "0.6.0 a dev x/y member box"})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let mut local = connect(&dir);
        let r = local.call("membership.adopt", serde_json::to_value(first.to_wire()).unwrap()).unwrap();
        assert_eq!(r.result.unwrap()["summary"]["hub"], "lap");
        assert_eq!(member::load(&member_toml).unwrap().unwrap().hub(), member::Hub::This);
        assert!(inbound.next_notification(Duration::from_millis(300)).unwrap().is_none(), "no change, no cycle");
        let r = local.call("membership.adopt", serde_json::to_value(second.to_wire()).unwrap()).unwrap();
        assert_eq!(r.result.unwrap()["summary"]["hub"], "box");
        let n = inbound.next_notification(Duration::from_secs(5)).unwrap().expect("no links.cycle");
        assert_eq!(n.method, crate::commands::fleet_link::LINKS_CYCLE);
        assert_eq!(member::load(&member_toml).unwrap().unwrap().hub(), member::Hub::Other("box".into()));
        // No longer the hub: it idles out once nobody holds it.
        drop((inbound, local));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }
}
