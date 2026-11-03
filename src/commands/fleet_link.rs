//! `devsandbox fleet-link --peer <dialer>` (hidden)
//! (docs/architecture/daemon/fleet.md, *Peer links*): the dialed end of a
//! link `machine add <this host>` made on `<dialer>` ([`fleet_link_peer`],
//! [`run_peer`]). The opener (it writes the sentinel), saying
//! `update::hello_of` (its build, whether this host is the hub, and its
//! fleet's hub, as `member.toml` says when it starts); a dialer naming
//! another hub is refused (`update::same_fleet`). IN:
//! each stream the dialer opens on `API`, `PTY`, `TUNNEL`, `TRANSFER`,
//! `AGENT` (answered by this host's agent grant for the key the dialer
//! proved, `fleet::agent`) or `ROUTE` (a routed stream to relay or answer,
//! `fleet::route`) is handed to this host's daemon as a connection whose
//! first line is `link.attach {peer: <dialer>, channel, key, introduced_by}`
//! ([`attach_stream`]: the key and introducer the dialer proved), so this
//! host's trust table for the dialer applies to every one. Right after the `Hello`s both
//! ends prove their member keys (`fleet::proof`; this host's identity read
//! once, at the start: no key or log, no link): the dialer must prove the
//! key `<dialer>` has in the membership log (or a held introduction), and
//! when this host's log is behind, the dialer's newer entries are stored
//! through this host's daemon (`membership.adopt`) before anything is
//! announced, the link ending if they moved the hub. OUT: `fleet-<dialer>.sock` and
//! its `.pty`, `.tunnel`, `.transfer` siblings, each accept a stream to the
//! dialer; `agent-link-<dialer>.sock` (`endpoint::link_agent_socket`,
//! `fleet::agent::serve`) while the dialer serves `AGENT`. Once the
//! `Hello`s are through it announces the dialer as an inbound neighbor
//! (`links.inbound`, a refusal ends the link with why) and holds that
//! connection for the link's life, re-announcing on a new one when it drops
//! (a version handoff of this host's daemon) ([`Announce`]); a
//! `links.cycle` notification on it (this host's hub changed) ends the
//! link, so the dialer redials and both ends say the new hub. Versions
//! follow the hub flag ([`follows`]): this end, not the hub, follows a hub
//! dialer's build, then ends the link (the dialer redials the new binary);
//! this end, the hub, pushes its dev build to a dialer that would take it.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::devsbd::mux;
use crate::devsbd::proto::{caps, channel};
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::fleet::follow::{Incoming, PUSH_WAIT};
use crate::serve::fleet::lent::Channel;
use crate::serve::fleet::transfer::Receivers;
use crate::serve::fleet::update::{Build, Decision};
use crate::serve::fleet::follow::Follow;
use crate::serve::fleet::{self, agent, link, update};
use crate::serve::machines::check_name;

/// This process's own binary: on Linux `/proc/self/exe`, the inode it runs
/// even when the path was replaced since; elsewhere `current_exe()`. The
/// member checks what it got reports this link's `Hello` anyway.
pub(crate) fn own_binary() -> io::Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    return Ok("/proc/self/exe".into());
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe()
}

/// When a hub saying `ours` pushes to a member saying `theirs`
/// (`update::hub_pushes`), `binary()`'s file to it on a thread of its own
/// through the link's socket `socket` (`update::push`; its
/// connection waits in the backlog until the opener's accept loop runs).
/// A failure is on stderr; the member's wait ends on its own.
pub fn push_build(
    ours: &str,
    theirs: &str,
    socket: std::path::PathBuf,
    binary: impl FnOnce() -> io::Result<std::path::PathBuf> + Send + 'static,
) -> Option<std::thread::JoinHandle<Result<()>>> {
    if !update::hub_pushes(ours, theirs) {
        return None;
    }
    let pushed = std::thread::Builder::new().name("fleet-push".into()).spawn(move || {
        let pushed = binary().context("cannot locate this binary").and_then(|b| update::push(&socket, &b));
        if let Err(e) = &pushed {
            eprintln!("fleet-link: cannot send this build to the member: {e:#}");
        }
        pushed
    });
    pushed.ok()
}

/// The first wait before re-announcing a dropped `links.inbound`, doubled
/// up to [`REANNOUNCE_MAX`].
pub const REANNOUNCE: Duration = Duration::from_millis(100);
const REANNOUNCE_MAX: Duration = Duration::from_secs(5);

/// How long [`attach_stream`] waits for `link.attach`'s answer.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// What a peer link's dialed end needs besides its stdio ([`run_peer`]).
pub struct PeerOptions {
    /// The socket dir: this host's daemon socket, and where the link's
    /// sockets are bound.
    pub dir: PathBuf,
    /// What this end says (`update::hello_of`): its role and its fleet's
    /// hub, as `member.toml` said when this process started.
    pub hello: &'static str,
    /// A fresh connection to this host's daemon (lazy start), for
    /// `links.inbound`.
    pub daemon: Arc<dyn Fn() -> Result<Conn> + Send + Sync>,
    /// How this end follows a hub dialer's build.
    pub follow: Follow,
    /// The re-announce backoff's first wait ([`REANNOUNCE`]).
    pub reannounce: Duration,
    /// How long a wait for the hub's binary waits ([`PUSH_WAIT`]).
    pub push_wait: Duration,
    /// This host's `member.toml`: its name, and the `member.key` and
    /// `membership.json` next to it the link proves with
    /// (`fleet::proof::Identity`).
    pub member: PathBuf,
}

/// The `--peer` CLI entry: the socket dir (created 0700 if missing), this
/// end's hello from `member.toml`'s `hub` and name (each dial is a process
/// of its own: read once), then [`run_peer`] on stdin/stdout. Following the
/// hub uses the release steps (`update::Release`) and `member.toml`'s
/// `dev-builds` / trust `full`, read at the `Hello`s.
pub fn fleet_link_peer(peer: &str) -> Result<()> {
    check_name(peer)?;
    let dir = endpoint::socket_dir()?;
    endpoint::ensure_private_dir(&dir)?;
    let member = fleet::member::path()?;
    let this = fleet::member::load(&member)?.unwrap_or_default();
    let own = this.name_or(&crate::serve::machines::link::hostname()).ok();
    let daemon_dir = dir.clone();
    let opts = PeerOptions {
        dir: dir.clone(),
        hello: update::hello_for(&this.hub(), own.as_deref()),
        daemon: Arc::new(move || client::connect(&daemon_dir, "fleet-link").context("cannot reach this host's devsandbox serve")),
        follow: Follow {
            steps: Arc::new(update::Release { socket_dir: dir }),
            dev_builds: {
                let member = member.clone();
                Arc::new(move || fleet::member::load(&member).ok().flatten().is_some_and(|m| m.takes_dev_builds()))
            },
        },
        reannounce: REANNOUNCE,
        push_wait: PUSH_WAIT,
        member: member.clone(),
    };
    run_peer(io::stdin(), io::stdout(), peer, opts)
}

/// What the end saying `ours` does about a peer saying `theirs` (both peer
/// link `Hello`s, `update::hello_of`; pure): it follows `theirs` only when
/// that says hub and `ours` doesn't (`update::decide`, `dev_builds` as
/// `member.toml` says); any other pair is [`Decision::Same`] (two non-hubs
/// need only the frame protocol's match, which the `Hello`s checked; a hub
/// is followed, never follows).
pub fn follows(ours: &str, theirs: &str, dev_builds: bool) -> Decision {
    if update::is_hub(ours) || !update::is_hub(theirs) {
        return Decision::Same;
    }
    update::decide(&Build::parse(ours), &Build::parse(theirs), update::platform_of(ours), update::platform_of(theirs), dev_builds)
}

/// The dialed end of a peer link with `peer` (its dialer) over
/// `reader`/`writer` (see the module doc). `Ok` when the dialer went away;
/// an error when `links.inbound` was refused, the link failed, or this end
/// followed the hub (the update's outcome: handed off, or why not). Either
/// way the link has ended, so the dialer redials.
pub fn run_peer(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, peer: &str, opts: PeerOptions) -> Result<()> {
    check_name(peer)?;
    let identity = fleet::proof::Identity::load(&opts.member, &crate::serve::machines::link::hostname())?;
    let dir = opts.dir.clone();
    let socket = endpoint::fleet_socket(&dir, peer);
    // While the hub's binary is on its way nothing but it is served.
    let receiving = Arc::new(AtomicBool::new(false));
    let incoming = Arc::new(Incoming::default());
    let binary = Arc::new(incoming.receiver(Receivers::new(|m| eprintln!("{m}"))));
    // Who the dialer proved it is, set once the `Hello`s and the proof are
    // through (before any stream is served).
    let proven: Arc<OnceLock<fleet::proof::Proven>> = Arc::default();
    let attached = |ch: Channel| {
        let (dir, proven, receiving) = (dir.clone(), Arc::clone(&proven), Arc::clone(&receiving));
        move || if receiving.load(Ordering::Relaxed) { None } else { attach_stream(&dir, ch, proven.get()?) }
    };
    let transfers = {
        let (attach, receiving) = (attached(Channel::Transfer), Arc::clone(&receiving));
        move || if receiving.load(Ordering::Relaxed) { binary.accept() } else { attach() }
    };
    let serves = link::Serves::default()
        .with(channel::API, attached(Channel::Api))
        .with(channel::PTY, attached(Channel::Pty))
        .with(channel::TUNNEL, attached(Channel::Tunnel))
        .with(channel::AGENT, attached(Channel::Agent))
        .with(channel::ROUTE, attached(Channel::Route))
        .with(channel::TRANSFER, transfers);
    let outlet = Arc::new(link::Outlet::default());
    let over = Arc::new(AtomicBool::new(false));
    let spec = link::Spec { hello: opts.hello, ..fleet::LINK };
    let refused: Arc<Mutex<Option<String>>> = Arc::default();
    let mut wanted = None;
    let mut held = None;
    let mut agent_thread = None;
    let auth = fleet::proof::Auth { identity, expect: peer.to_string(), log: |m| eprintln!("fleet-link: {m}") };
    let endpoint = link::Endpoint { socket: Some(&socket), serves, outlet: Some(&outlet), auth: Some(auth) };
    let end = link::opener_with(&spec, reader, writer, endpoint, |theirs| {
        let Some(p) = &theirs.proven else {
            bail!("{peer} proved no member key");
        };
        let _ = proven.set(fleet::proof::Proven { newer: None, ..p.clone() });
        if let Some(newer) = theirs.proven.as_ref().and_then(|p| p.newer.as_ref()) {
            adopt(&mut (opts.daemon)()?, newer).with_context(|| format!("cannot take {peer}'s newer membership entries"))?;
            eprintln!("fleet-link: took {peer}'s newer membership log ({})", newer.log.head());
            if let Some(why) = fleet::proof::hub_moved(spec.hello, newer) {
                bail!("{why}");
            }
        }
        update::same_fleet(peer, spec.hello, &theirs.hash).map_err(|why| anyhow!("{why}"))?;
        let label = Build::parse(&theirs.hash).label();
        match follows(spec.hello, &theirs.hash, (opts.follow.dev_builds)()) {
            Decision::Same => {}
            Decision::Download { version } => {
                wanted = Some(version);
                bail!("{peer}, the hub, runs devsandbox {label}: updating this one first");
            }
            Decision::Stop(why) => bail!("{why}"),
            Decision::Receive => {
                let scratch = update::scratch_dir(&std::env::temp_dir()).context("cannot take the hub's build")?;
                receiving.store(true, Ordering::Relaxed);
                let hang_up = Arc::clone(&outlet);
                let seq = incoming.expect(theirs.hash.clone(), scratch, Some(Box::new(move || hang_up.hang_up())));
                incoming.watch(seq, opts.push_wait);
                eprintln!("fleet-link: {peer}, the hub, runs devsandbox {label}: receiving its binary");
                return Ok(());
            }
        }
        let params = json!({ "peer": peer, "version": theirs.hash, "hub": update::is_hub(&theirs.hash) });
        let mut conn = (opts.daemon)()?;
        if let Err(why) = announce(&mut conn, &params)? {
            bail!("refused: {why}");
        }
        let on_end = {
            let (refused, outlet, peer) = (Arc::clone(&refused), Arc::clone(&outlet), peer.to_string());
            move |end: AnnounceEnd| {
                let why = match end {
                    AnnounceEnd::Refused(why) => format!("refused: {why}"),
                    AnnounceEnd::Cycled(Some(why)) => why,
                    AnnounceEnd::Cycled(None) => format!("this host's hub changed: the link ends so {peer} redials it"),
                };
                eprintln!("fleet-link: {why}");
                *refused.lock().unwrap() = Some(why);
                outlet.hang_up();
            }
        };
        held = Some(Announce::hold(conn, params, Arc::clone(&opts.daemon), opts.reannounce, on_end));
        if theirs.caps & caps::AGENT != 0 {
            let (path, outlet, over) = (endpoint::link_agent_socket(&dir, peer), Arc::clone(&outlet), Arc::clone(&over));
            agent_thread = std::thread::Builder::new()
                .name("fleet-link-agent".into())
                .spawn(move || {
                    if let Err(e) = agent::serve(&path, &*outlet, &over, agent::PROBE) {
                        eprintln!("fleet-link: {e:#}");
                    }
                })
                .ok();
        }
        if update::is_hub(spec.hello) && !update::is_hub(&theirs.hash) {
            push_build(spec.hello, &theirs.hash, socket.clone(), own_binary);
        }
        Ok(())
    });
    over.store(true, Ordering::Relaxed);
    drop(held);
    if let Some(t) = agent_thread {
        let _ = t.join();
    }
    let steps = opts.follow.steps.as_ref();
    if let Some(want) = incoming.take() {
        let label = Build::parse(&want.hello).label();
        let installed = match &want.received {
            Some(binary) => update::install_received(steps, binary, &want.hello),
            None => Err(anyhow!("{}", want.why.as_deref().unwrap_or("the link ended before the hub's binary arrived"))),
        };
        let _ = std::fs::remove_dir_all(&want.dir);
        installed.with_context(|| format!("update to {label} failed"))?;
        bail!("updated to devsandbox {label}, handing off: the link ends so {peer} redials it");
    }
    if let Some(version) = wanted {
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        update::update(steps, &version, os, arch, &std::env::temp_dir()).with_context(|| format!("update to {version} failed"))?;
        bail!("updated to devsandbox {version}, handing off: the link ends so {peer} redials it");
    }
    if let Some(why) = refused.lock().unwrap().take() {
        bail!("{why}");
    }
    end
}

/// `membership.adopt` of `newer` on `conn` (this host's daemon writes the
/// log, and `hub` from it).
fn adopt(conn: &mut Conn, newer: &fleet::membership::Stored) -> Result<()> {
    let r = conn.call("membership.adopt", serde_json::to_value(newer.to_wire())?)?;
    match r.error {
        Some(e) => bail!("{}", e.message),
        None => Ok(()),
    }
}

/// The notification on the held `links.inbound` connection that ends the
/// link: `daemon::Shared::set_hub` (this host's hub changed, so both ends
/// redial saying the new one), or `daemon::Shared::end_inbound` (the peer
/// is out of this host's fleet; params `{why}`).
pub const LINKS_CYCLE: &str = "links.cycle";

/// Why [`Announce`]'s hold ends the link.
#[derive(Debug, PartialEq, Eq)]
pub enum AnnounceEnd {
    /// A re-announce was refused, with why.
    Refused(String),
    /// The daemon sent [`LINKS_CYCLE`], with its `why` when it gave one.
    Cycled(Option<String>),
}

/// `links.inbound` with `params` on `conn`: `Ok(Err(why))` when the daemon
/// refused it (a name clash), `Err` when the call failed.
fn announce(conn: &mut Conn, params: &Value) -> Result<Result<(), String>> {
    let r = conn.call("links.inbound", params.clone())?;
    Ok(match r.error {
        Some(e) => Err(e.message),
        None => Ok(()),
    })
}

/// The held `links.inbound` connection: while the link is up, a thread waits
/// for it to drop (the daemon handed off or restarted) and then announces
/// again on a new one, backing off from the first wait to
/// [`REANNOUNCE_MAX`] while the daemon can't be reached. A refusal then, or
/// a [`LINKS_CYCLE`] notification on it, goes to `on_end` (which ends the
/// link) and the thread ends. Dropping it stops the thread and closes the
/// connection.
pub struct Announce {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Announce {
    fn hold(
        conn: Conn,
        params: Value,
        daemon: Arc<dyn Fn() -> Result<Conn> + Send + Sync>,
        first_wait: Duration,
        on_end: impl FnOnce(AnnounceEnd) + Send + 'static,
    ) -> Announce {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("fleet-link-announce".into())
            .spawn(move || {
                let mut conn = conn;
                loop {
                    // Only `links.cycle` means something on it; an error is the drop.
                    while !stopping.load(Ordering::Relaxed) {
                        match conn.next_notification(Duration::from_millis(100)) {
                            Ok(Some(n)) if n.method == LINKS_CYCLE => {
                                let why = n.params.get("why").and_then(Value::as_str).map(str::to_string);
                                return on_end(AnnounceEnd::Cycled(why));
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    let mut wait = first_wait;
                    conn = loop {
                        if !sleep_unless(&stopping, wait) {
                            return;
                        }
                        wait = (wait * 2).min(REANNOUNCE_MAX);
                        match daemon().and_then(|mut c| Ok((announce(&mut c, &params)?, c))) {
                            Ok((Ok(()), c)) => break c,
                            Ok((Err(why), _)) => return on_end(AnnounceEnd::Refused(why)),
                            Err(e) => eprintln!("fleet-link: cannot announce the link again yet: {e:#}"),
                        }
                    };
                }
            })
            .ok();
        Announce { stop, thread }
    }
}

impl Drop for Announce {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Sleep `d` in short steps; `false` once `stop` is set.
fn sleep_unless(stop: &AtomicBool, d: Duration) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(until.saturating_duration_since(Instant::now()).min(Duration::from_millis(20)));
    }
    !stop.load(Ordering::Relaxed)
}

/// One stream peer `peer` (who its link proved: name, key, introducer)
/// opens on `channel`, handed to this host's daemon in `dir`: a socket pair
/// whose far end a thread of its own takes (this runs on the link's read
/// thread). The thread connects the daemon, sends `link.attach {peer,
/// channel, key, introduced_by}` as the connection's first line, reads the
/// answer (nothing past it), and splices the stream to the connection (an
/// `API` stream's first byte is the daemon's, fleet plan decision 27). A
/// refused attach closes the stream, with why on stderr. `None` when no
/// pair or thread could be made.
pub fn attach_stream(dir: &Path, channel: Channel, peer: &fleet::proof::Proven) -> Option<mux::Conn> {
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().ok()?;
    let params = attach_params(peer, channel);
    let dir = dir.to_path_buf();
    let spawned = std::thread::Builder::new().name("fleet-link-attach".into()).spawn(move || {
        let daemon = match attach(&dir, params) {
            Ok(daemon) => daemon,
            Err(e) => return eprintln!("fleet-link: cannot hand a {} stream to this host's devsandbox: {e:#}", channel.as_str()),
        };
        link::splice(ours, daemon.into());
    });
    spawned.ok()?;
    Some(theirs.into())
}

/// `link.attach`'s params for a `channel` stream of `peer` (pure).
pub fn attach_params(peer: &fleet::proof::Proven, channel: Channel) -> Value {
    json!({ "peer": peer.name, "channel": channel.as_str(), "key": peer.key, "introduced_by": peer.introduced_by })
}

/// A connection to the daemon in `dir` past `link.attach`'s `ok` (with
/// `params`, [`attach_params`]), its answer read byte by byte so nothing
/// past it is taken.
fn attach(dir: &Path, params: Value) -> Result<endpoint::Stream> {
    let mut stream = endpoint::connect(dir).context("cannot connect")?;
    stream.set_read_timeout(Some(ATTACH_TIMEOUT)).context("cannot set a read timeout")?;
    let line = json!({ "id": 0, "method": "link.attach", "params": params });
    stream.write_all(format!("{line}\n").as_bytes()).context("cannot send link.attach")?;
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).context("no link.attach answer")? {
            0 => bail!("the daemon closed the connection before answering link.attach"),
            _ if byte[0] == b'\n' => break,
            _ if answer.len() as u64 >= crate::serve::proto::MAX_LINE => bail!("link.attach's answer is too long"),
            _ => answer.push(byte[0]),
        }
    }
    let answer: Value = serde_json::from_slice(&answer).context("bad link.attach answer")?;
    if let Some(e) = answer.get("error").filter(|e| !e.is_null()) {
        bail!("link.attach refused: {}", e.get("message").and_then(Value::as_str).unwrap_or("?"));
    }
    stream.set_read_timeout(None).context("cannot clear the read timeout")?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::*;
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::daemon::Exit;
    use crate::serve::fleet::membership_api::test_support;
    use crate::serve::fleet::proof::{Auth, Identity};
    use crate::serve::machines::{self, Entry, ssh};
    use crate::serve::proto::{HelloResult, Version};

    fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A raw connection past a 0.0.0 hello (never a handoff), retrying
    /// while the daemon binds.
    fn stream_at(dir: &Path) -> endpoint::Stream {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(mut stream) = endpoint::connect(dir) {
                match client::hello(&mut stream, &Version { semver: "0.0.0".into(), build: 0 }, "test").unwrap() {
                    client::Hello::Ready(_) => return stream,
                    other => panic!("{other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn conn_at(dir: &Path) -> Conn {
        let reply = HelloResult { version: "0.0.0".into(), build: 0, protocol: 1, handoff: false };
        Conn::new(stream_at(dir), reply).unwrap()
    }

    /// A daemon for a peer link's end in `base/<name>`: its data dir (where
    /// fleet roots go) `base/<name>-data`, its registry there, a state whose
    /// one instance is in its own root `base/<name>-own` (canonical, returned
    /// with the socket dir and the data dir).
    fn peer_daemon(base: &Path, name: &str) -> (PathBuf, PathBuf, PathBuf, crate::serve::daemon::Options) {
        let (dir, data, own) = (base.join(name), base.join(format!("{name}-data")), base.join(format!("{name}-own")));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&own).unwrap();
        let own = std::fs::canonicalize(own).unwrap();
        let state = data.join("state.toml");
        std::fs::write(
            &state,
            format!(
                "[instance.w]\ninstance_id = \"id-w\"\nsandbox = \"web\"\ncontainer = \"devsandbox-w\"\nfolder = \"/w\"\n\
                 workspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                own.to_str().unwrap()
            ),
        )
        .unwrap();
        let mut o = opts(1, 60_000);
        o.state = Some(state);
        o.member = Some(data.join("member.toml"));
        o.machines = Some(data.join("machines.toml"));
        o.home = Some(base.join(format!("{name}-home")));
        (dir, std::fs::canonicalize(&data).unwrap(), own, o)
    }

    /// A handler whose every stream gets `tag` written, then closed.
    fn tagged(tag: &'static [u8]) -> impl Fn() -> Option<crate::devsbd::mux::Conn> + Send + Sync + 'static {
        move || {
            let (mut ours, theirs) = UnixStream::pair().ok()?;
            std::thread::spawn(move || {
                let _ = ours.write_all(tag);
            });
            Some(theirs.into())
        }
    }

    /// K (`k_member`, its `member.toml`) and L (`l_member`) in one fleet,
    /// K its authority and hub; the fleet's log and L's identity (the fake
    /// dialer's).
    fn fleet(k_member: &Path, l_member: &Path) -> (crate::serve::fleet::membership::Stored, Identity) {
        for m in [k_member, l_member] {
            std::fs::create_dir_all(m.parent().unwrap()).unwrap();
        }
        let stored = test_support::fleet_of("K", &[("K", k_member), ("L", l_member)]);
        (stored, Identity::load(l_member, "L").unwrap())
    }

    /// [`fleet`] in `base/k-id` and `base/l-id`: K's `member.toml` and L's
    /// identity.
    fn ids(base: &Path) -> (PathBuf, Identity) {
        let k = base.join("k-id/member.toml");
        let (_, l) = fleet(&k, &base.join("l-id/member.toml"));
        (k, l)
    }

    /// The dialer's end, faked as 7d's daemon answers it: the answerer of a
    /// peer link saying `hello`, proving `identity` (L's) and expecting `K`,
    /// each `API` stream K opens handed to the daemon in `daemon` as peer
    /// `K`'s (`attach_stream`, the in-process path's stand-in), `PTY`
    /// streams answered `dialer pty`; its outlet `outlet`.
    fn fake_dialer(io: UnixStream, hello: &'static str, identity: Identity, daemon: PathBuf, outlet: Arc<link::Outlet>) -> std::thread::JoinHandle<link::End> {
        let (r, w) = (io.try_clone().unwrap(), io);
        std::thread::spawn(move || {
            let proven: Arc<OnceLock<fleet::proof::Proven>> = Arc::default();
            let k = Arc::clone(&proven);
            let serves = link::Serves::default()
                .with(channel::API, move || attach_stream(&daemon, Channel::Api, k.get()?))
                .with(channel::PTY, tagged(b"dialer pty"));
            let spec = link::Spec { hello, ..fleet::LINK };
            let auth = Auth { identity, expect: "K".into(), log: |_| {} };
            let endpoint = link::Endpoint { serves, outlet: Some(&outlet), auth: Some(auth), ..link::Endpoint::default() };
            let ready = |peer: &link::Ready| {
                let _ = proven.set(peer.proven.clone().ok_or_else(|| "K proved no key".to_string())?);
                Ok(())
            };
            link::answerer_with(&spec, r, w, endpoint, ready, || {})
        })
    }

    /// K's `fleet-link --peer L` over `io`, its daemon and sockets in `dir`,
    /// its `member.toml` `member`, a member; following through `steps` (dev
    /// builds taken).
    fn peer_link(io: UnixStream, dir: PathBuf, member: PathBuf, steps: Arc<dyn update::Steps>) -> std::thread::JoinHandle<Result<()>> {
        peer_link_saying(io, dir, member, steps, update::hello_of(false, None))
    }

    /// [`peer_link`], K saying `hello`.
    fn peer_link_saying(
        io: UnixStream,
        dir: PathBuf,
        member: PathBuf,
        steps: Arc<dyn update::Steps>,
        hello: &'static str,
    ) -> std::thread::JoinHandle<Result<()>> {
        let (r, w) = (io.try_clone().unwrap(), io);
        let daemon_dir = dir.clone();
        let opts = PeerOptions {
            dir,
            hello,
            daemon: Arc::new(move || Ok(conn_at(&daemon_dir))),
            follow: Follow { steps, dev_builds: Arc::new(|| true) },
            reannounce: Duration::from_millis(20),
            push_wait: Duration::from_secs(5),
            member,
        };
        std::thread::spawn(move || run_peer(r, w, "L", opts))
    }

    /// What the dialer proved goes into `link.attach` as the daemon parses
    /// it: name, channel, key, introducer.
    #[test]
    fn an_attach_carries_the_proven_key_and_introducer() {
        use crate::serve::fleet::identity::{Signer, SigningKey};
        use crate::serve::fleet::lent;
        let key = SigningKey::from_seed([3; 32]).public_key();
        let proven = fleet::proof::Proven { name: "pi".into(), key, introduced_by: Some("lap".into()), newer: None };
        let got = lent::attach_params(&attach_params(&proven, Channel::Pty)).unwrap();
        assert_eq!(got, lent::Attach { name: "pi".into(), channel: Channel::Pty, key, introduced_by: Some("lap".into()) });
        let member = fleet::proof::Proven { introduced_by: None, ..proven };
        assert_eq!(lent::attach_params(&attach_params(&member, Channel::Api)).unwrap().introduced_by, None);
    }

    fn machine_rows(dir: &Path) -> Vec<Value> {
        let rows = conn_at(dir).call("machines.list", Value::Null).unwrap().result.unwrap();
        rows.as_array().unwrap().clone()
    }

    fn inbound_row(dir: &Path) -> Option<Value> {
        machine_rows(dir).into_iter().find(|r| r["kind"] == "inbound")
    }

    /// A peer link end to end: K's `fleet-link --peer L` announces L, which
    /// `machines.list` shows with the roots L's daemon answers over the link
    /// (its own root and its fleet root for K); L's `API` streams reach K's
    /// daemon under K's trust table for L; K's local clients reach L's
    /// table through `fleet-L.sock`; the neighbor goes with the link.
    #[test]
    fn a_peer_link_announces_its_dialer_and_hands_its_streams_to_the_daemon() {
        let base = scratch("peer-e2e");
        let (k_dir, k_data, k_own, k_opts) = peer_daemon(&base, "k");
        let (l_dir, l_data, l_own, l_opts) = peer_daemon(&base, "l");
        let k_member = k_opts.member.clone().unwrap();
        let (_, l_id) = fleet(&k_member, l_opts.member.as_ref().unwrap());
        let daemons = [spawn_daemon(&k_dir, k_opts), spawn_daemon(&l_dir, l_opts)];
        drop((conn_at(&k_dir), conn_at(&l_dir)));

        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let outlet = Arc::new(link::Outlet::default());
        let dialer = fake_dialer(dialer_io, update::hello_of(false, None), l_id, l_dir.clone(), Arc::clone(&outlet));
        let k_end = peer_link(k_io.try_clone().unwrap(), k_dir.clone(), k_member, Arc::new(update::tests::Fake::default()));
        let l_fleet = l_data.join("fleet/K");
        let want = json!([l_own, l_fleet]);
        // A machine on K: its held link's state and its daemon's semver.
        wait_for(|| inbound_row(&k_dir).is_some_and(|r| r["roots"] == want && r["state"] == "connected"), "L listed with its roots");
        let row = inbound_row(&k_dir).unwrap();
        assert_eq!((&row["name"], &row["target"], &row["state"]), (&json!("L"), &Value::Null, &json!("connected")));
        assert_eq!(row["version"], "0.6.0");
        let socket = endpoint::fleet_socket(&k_dir, "L");
        assert!(socket.exists(), "{}", socket.display());
        assert!(!endpoint::link_agent_socket(&k_dir, "L").exists(), "the dialer serves no AGENT");

        // A registry entry can't take a live neighbor's name.
        let mut k = conn_at(&k_dir);
        let key = crate::serve::fleet::identity::Signer::public_key(&crate::serve::fleet::identity::SigningKey::from_seed([9; 32]));
        let add = json!({"hub": false, "target": "me@l", "name": "L", "roots": ["/x"], "key": key});
        let e = k.call("machines.add", add).unwrap().error.unwrap();
        assert!(e.code == "denied" && e.message.contains("inbound neighbor"), "{e:?}");

        // L → K: an `API` stream, K's trust table for L (`worker`): its
        // fleet root `fleet/L/` and K's own root, nothing else.
        wait_for(|| outlet.is_up(), "the dialer's outlet is up");
        let crate::devsbd::mux::Conn::Unix(stream) = outlet.open(channel::API).unwrap() else { panic!("a socket pair") };
        let duplex = ssh::Duplex::socket(stream.into());
        let mut api = crate::serve::machines::link::handshake("K", duplex, Duration::from_secs(5), crate::serve::machines::link::Over::Link).unwrap();
        let cfg = k_data.join("fleet/L/cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let code = |c: &mut Conn, m: &str, p: Value| c.call(m, p).unwrap().error.map(|e| e.code);
        assert_eq!(code(&mut api.conn, "instances.list", json!({"dir": cfg})), None, "under L's fleet root");
        assert_eq!(code(&mut api.conn, "instances.list", json!({"dir": k_own})), None, "K's own root");
        assert_eq!(code(&mut api.conn, "instances.list", json!({"dir": base})), Some("denied".into()));
        assert_eq!(code(&mut api.conn, "member.get", json!({})), Some("denied".into()));
        assert_eq!(code(&mut api.conn, "links.inbound", json!({"peer": "x", "version": "v"})), Some("denied".into()));
        drop(api);

        // K → L: a local client on K reaches the dialer's table.
        let mut got = Vec::new();
        link::connect(&socket, channel::PTY).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, b"dialer pty");

        // The link ends: K's end returns, its socket and the neighbor go.
        k_io.shutdown(Shutdown::Both).unwrap();
        k_end.join().unwrap().unwrap();
        assert!(!endpoint::fleet_socket(&k_dir, "L").exists());
        wait_for(|| inbound_row(&k_dir).is_none(), "L no longer listed");
        assert!(matches!(dialer.join().unwrap(), link::End::Closed));
        drop(k);
        for (dir, daemon) in [&k_dir, &l_dir].into_iter().zip(daemons) {
            assert!(conn_at(dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A dialer named like a registry entry here is refused: the link ends
    /// with why, nothing listed but the entry.
    #[test]
    fn a_dialer_named_like_a_registry_entry_is_refused() {
        let base = scratch("peer-clash");
        let (k_dir, k_data, _, k_opts) = peer_daemon(&base, "k");
        machines::update(&k_data.join("machines.toml"), |e| e.push(Entry::ssh("L", "me@l"))).unwrap();
        let k_member = k_opts.member.clone().unwrap();
        let (_, l_id) = fleet(&k_member, &base.join("l-id/member.toml"));
        let daemon = spawn_daemon(&k_dir, k_opts);
        drop(conn_at(&k_dir));
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let dialer = fake_dialer(dialer_io, update::hello_of(false, None), l_id, base.join("nowhere"), Arc::default());
        let err = peer_link(k_io, k_dir.clone(), k_member, Arc::new(update::tests::Fake::default())).join().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("refused: `L` is already a machine here (me@l)"), "{err:#}");
        assert!(!endpoint::fleet_socket(&k_dir, "L").exists());
        assert!(matches!(dialer.join().unwrap(), link::End::Closed));
        let rows = machine_rows(&k_dir);
        assert_eq!((rows.len(), &rows[0]["kind"]), (1, &json!("ssh")));
        assert!(conn_at(&k_dir).call("shutdown", json!({})).unwrap().error.is_none());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// K's daemon goes away under a live link (a handoff): the link stays,
    /// and the next daemon learns the neighbor from a new announce.
    #[test]
    fn the_neighbor_is_announced_again_to_the_next_daemon() {
        let base = scratch("peer-again");
        let (k_dir, _, _, k_opts) = peer_daemon(&base, "k");
        let (l_dir, _, _, l_opts) = peer_daemon(&base, "l");
        let again = crate::serve::daemon::Options { machines: k_opts.machines.clone(), state: k_opts.state.clone(), member: k_opts.member.clone(), ..opts(2, 60_000) };
        let k_member = k_opts.member.clone().unwrap();
        let (_, l_id) = fleet(&k_member, l_opts.member.as_ref().unwrap());
        let first = spawn_daemon(&k_dir, k_opts);
        let l_daemon = spawn_daemon(&l_dir, l_opts);
        drop((conn_at(&k_dir), conn_at(&l_dir)));
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let dialer = fake_dialer(dialer_io, update::hello_of(false, None), l_id, l_dir.clone(), Arc::default());
        let k_end = peer_link(k_io.try_clone().unwrap(), k_dir.clone(), k_member, Arc::new(update::tests::Fake::default()));
        wait_for(|| inbound_row(&k_dir).is_some(), "L listed");
        assert!(conn_at(&k_dir).call("shutdown", json!({})).unwrap().error.is_none());
        assert_eq!(first.join().unwrap().unwrap(), Exit::Shutdown);
        let second = spawn_daemon(&k_dir, again);
        wait_for(|| endpoint::connect(&k_dir).is_ok() && inbound_row(&k_dir).is_some(), "L announced again");
        assert!(endpoint::fleet_socket(&k_dir, "L").exists(), "the link stayed up");
        k_io.shutdown(Shutdown::Both).unwrap();
        k_end.join().unwrap().unwrap();
        assert!(matches!(dialer.join().unwrap(), link::End::Closed));
        for (dir, daemon) in [(&k_dir, second), (&l_dir, l_daemon)] {
            assert!(conn_at(dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// K's hub changes (a newer membership log adopted on K's daemon): K's
    /// end gets `links.cycle` on its held announce and ends the link,
    /// saying why.
    #[test]
    fn a_hub_change_on_the_dialed_host_ends_the_link() {
        use crate::serve::fleet::membership::Op;
        let base = scratch("peer-cycle");
        let (k_dir, _, _, k_opts) = peer_daemon(&base, "k");
        let k_member = k_opts.member.clone().unwrap();
        let (l_dir, _, _, l_opts) = peer_daemon(&base, "l");
        let (mut k_fleet, l_id) = fleet(&k_member, l_opts.member.as_ref().unwrap());
        k_fleet.log.append(Op::Hub { name: "L".into() }, "K", &test_support::key_of(&k_member)).unwrap();
        let daemons = [spawn_daemon(&k_dir, k_opts), spawn_daemon(&l_dir, l_opts)];
        drop((conn_at(&k_dir), conn_at(&l_dir)));
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let dialer = fake_dialer(dialer_io, update::hello_of(false, None), l_id, l_dir.clone(), Arc::default());
        let k_end = peer_link(k_io.try_clone().unwrap(), k_dir.clone(), k_member, Arc::new(update::tests::Fake::default()));
        wait_for(|| inbound_row(&k_dir).is_some(), "L listed");
        let adopt = serde_json::to_value(k_fleet.to_wire()).unwrap();
        assert!(conn_at(&k_dir).call("membership.adopt", adopt).unwrap().error.is_none());
        let err = k_end.join().unwrap().unwrap_err();
        assert_eq!(format!("{err:#}"), "this host's hub changed: the link ends so L redials it");
        // `fleet-link` exits there: its stdio closes.
        k_io.shutdown(Shutdown::Both).unwrap();
        assert!(matches!(dialer.join().unwrap(), link::End::Closed));
        wait_for(|| inbound_row(&k_dir).is_none(), "L no longer listed");
        for (dir, daemon) in [&k_dir, &l_dir].into_iter().zip(daemons) {
            assert!(conn_at(dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// K's membership log is behind the dialer's, whose newer entry makes
    /// `L` the hub: K's end stores it through its daemon (`membership.adopt`:
    /// K's `member.toml` hub follows) before announcing anything, then ends
    /// the link so both ends redial saying the new hub.
    #[test]
    fn the_dialed_end_behind_takes_the_dialers_entries() {
        use crate::serve::fleet::member::{self, Hub};
        use crate::serve::fleet::membership::Op;
        let base = scratch("peer-behind");
        let (k_dir, _, _, k_opts) = peer_daemon(&base, "k");
        let k_member = k_opts.member.clone().unwrap();
        let (mut ahead, mut l_id) = fleet(&k_member, &base.join("l-id/member.toml"));
        ahead.log.append(Op::Hub { name: "L".into() }, "K", &test_support::key_of(&k_member)).unwrap();
        l_id.stored = ahead.clone();
        let daemon = spawn_daemon(&k_dir, k_opts);
        drop(conn_at(&k_dir));
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let dialer = fake_dialer(dialer_io, update::hello_of(false, None), l_id, base.join("nowhere"), Arc::default());
        let err = peer_link(k_io, k_dir.clone(), k_member.clone(), Arc::new(update::tests::Fake::default())).join().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("the fleet's hub is now `L`"), "{err:#}");
        assert_eq!(test_support::read(&k_member).unwrap(), ahead, "K took L's entries");
        assert_eq!(member::load(&k_member).unwrap().unwrap().hub(), Hub::Other("L".into()));
        assert!(matches!(dialer.join().unwrap(), link::End::Closed));
        assert!(inbound_row(&k_dir).is_none(), "nothing announced");
        assert!(conn_at(&k_dir).call("shutdown", json!({})).unwrap().error.is_none());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A dialer whose key isn't the one K's log has for `L`: refused by the
    /// proof, before anything is announced.
    #[test]
    fn a_dialer_proving_another_key_is_refused() {
        let base = scratch("peer-wrong-key");
        std::fs::create_dir_all(&base).unwrap();
        let (k_member, mut l_id) = ids(&base);
        l_id.key = crate::serve::fleet::identity::SigningKey::from_seed([7; 32]);
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let dialer = fake_dialer(dialer_io, update::hello_of(false, None), l_id, base.join("nowhere"), Arc::default());
        let err = peer_link(k_io, base.clone(), k_member, Arc::new(update::tests::Fake::default())).join().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("`L` proved key"), "{err:#}");
        assert!(matches!(dialer.join().unwrap(), link::End::Refused(why) if why.contains("`K` refused the link")));
        assert!(!endpoint::fleet_socket(&base, "L").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A dialer naming another hub than K's: refused before anything is
    /// announced, naming both.
    #[test]
    fn a_dialer_in_another_fleet_is_refused() {
        let base = scratch("peer-two-fleets");
        std::fs::create_dir_all(&base).unwrap();
        let (k_member, l_id) = ids(&base);
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let dialer = fake_dialer(dialer_io, update::hello_of(true, Some("lap")), l_id, base.join("nowhere"), Arc::default());
        let k_end = peer_link_saying(k_io.try_clone().unwrap(), base.clone(), k_member, Arc::new(update::tests::Fake::default()), update::hello_of(false, Some("box")));
        let err = k_end.join().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("`L` is in `lap`'s fleet, this host in `box`'s"), "{err:#}");
        k_io.shutdown(Shutdown::Both).unwrap();
        let _ = dialer.join().unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_non_hub_follows_a_hub() {
        let ours = update::hello_of(false, None);
        let semver = env!("CARGO_PKG_VERSION");
        let platform = update::PLATFORM;
        let published = format!("9.9.9 abc published {platform} hub");
        assert_eq!(follows(ours, &published, false), Decision::Download { version: "9.9.9".into() });
        assert_eq!(follows(ours, update::hello_of(true, None), false), Decision::Same, "the same build");
        let dev = format!("{semver} zzz dev {platform} hub");
        assert_eq!(follows(ours, &dev, true), Decision::Receive);
        assert!(matches!(follows(ours, &dev, false), Decision::Stop(_)));
        let elsewhere = format!("{semver} zzz dev other/os hub");
        assert!(matches!(follows(ours, &elsewhere, true), Decision::Stop(why) if why.contains("can't run on")));
        // No hub, or this end is it: nothing to follow.
        assert_eq!(follows(ours, &published.replace(" hub", " member"), false), Decision::Same);
        assert_eq!(follows(update::hello_of(true, None), &published, false), Decision::Same);
        assert_eq!(follows(ours, "9.9.9 abc published linux/x86_64", false), Decision::Same, "a hello without the flag says no hub");
    }

    /// A hub dialer on a published build: K's end drops the link, installs
    /// that version and hands off, and says so.
    #[test]
    fn a_member_follows_a_published_hub_dialer() {
        let base = scratch("peer-follow");
        std::fs::create_dir_all(&base).unwrap();
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let hub: &'static str = Box::leak(format!("9.9.9 abc published {} hub", update::PLATFORM).into_boxed_str());
        let (k_member, l_id) = ids(&base);
        let dialer = fake_dialer(dialer_io, hub, l_id, base.join("nowhere"), Arc::default());
        let fake = Arc::new(update::tests::Fake::reporting("9.9.9"));
        let err = peer_link(k_io, base.clone(), k_member, fake.clone()).join().unwrap().unwrap_err();
        assert!(format!("{err:#}").starts_with("updated to devsandbox 9.9.9, handing off"), "{err:#}");
        assert!(fake.calls().first().is_some_and(|c| c.starts_with("fetch https://") && c.contains("v9.9.9")), "{:?}", fake.calls());
        assert_eq!(fake.calls().last().map(String::as_str), Some("handoff "));
        assert!(!endpoint::fleet_socket(&base, "L").exists());
        assert!(matches!(dialer.join().unwrap(), link::End::Closed));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A hub dialer on another dev build for this platform: K's end serves
    /// only the binary it pushes, then ends the link and installs it.
    #[test]
    fn a_member_takes_a_dev_hub_dialers_pushed_build() {
        use crate::serve::fleet::transfer::{self, Header};
        let base = scratch("peer-dev");
        std::fs::create_dir_all(&base).unwrap();
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let hub: &'static str = Box::leak(format!("{} zzz dev {} hub", env!("CARGO_PKG_VERSION"), update::PLATFORM).into_boxed_str());
        let outlet = Arc::new(link::Outlet::default());
        let (k_member, l_id) = ids(&base);
        let dialer = fake_dialer(dialer_io, hub, l_id, base.join("nowhere"), Arc::clone(&outlet));
        let fake = Arc::new(update::tests::Fake::building(hub.trim_end_matches(" hub")));
        let k_end = peer_link(k_io.try_clone().unwrap(), base.clone(), k_member, fake.clone());
        wait_for(|| outlet.is_up(), "the link is up");
        // Waiting for the binary: nothing else is served.
        let api = outlet.open(channel::API).unwrap();
        let crate::devsbd::mux::Conn::Unix(mut api) = api else { panic!("a socket pair") };
        let mut got = Vec::new();
        api.read_to_end(&mut got).unwrap();
        assert!(got.is_empty(), "refused");
        let (size, sha256) = transfer::digest(&b"new build"[..]).unwrap();
        let header = Header { kind: update::BINARY_KIND.into(), name: "devsandbox".into(), size, sha256, to: None };
        transfer::send(outlet.open(channel::TRANSFER).unwrap(), &header, &b"new build"[..]).unwrap();
        let err = k_end.join().unwrap().unwrap_err();
        assert!(format!("{err:#}").starts_with("updated to devsandbox "), "{err:#}");
        assert_eq!(fake.calls(), ["build received", "install /dest/devsandbox", "build dest", "handoff "]);
        k_io.shutdown(Shutdown::Both).unwrap();
        assert!(matches!(dialer.join().unwrap(), link::End::Closed));
        let _ = std::fs::remove_dir_all(&base);
    }
}
