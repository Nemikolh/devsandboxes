//! `devsandbox fleet-link --worker <name> --root <path>` (hidden): the hub's end of a fleet
//! worker's link (docs/architecture/daemon/fleet.md, *Hub side*). A worker's
//! daemon runs it on its hub over its own ssh (`ssh -T -o BatchMode=yes <hub>
//! devsandbox fleet-link --worker <name> --root <fleet root>`, `serve::fleet::worker`) and answers the
//! streams it opens with connections to its own daemon.
//!
//! It is the opener of a `fleet::link` of `fleet::LINK`: it binds
//! `endpoint::fleet_socket` (`fleet-<name>.sock`, 0600 in the 0700 socket
//! dir; the newest link for a worker wins), and each connection there (the
//! hub's link to the offered machine, its forwarded verbs: `fleet::dialer::dial`)
//! becomes one `API` stream to the worker's daemon. Once the `Hello`s are
//! through it sends `fleet.offer {worker, version, root, draining}` to this host's daemon
//! (started like any client's, if it isn't running), which registers the
//! worker as an offered machine whose one root is `root` (the worker's
//! canonical fleet root, a path on the worker), and holds that connection for the link's
//! life, so a linked worker keeps the hub's daemon from idling out. A refused
//! offer (not a hub, a name an ssh machine has) ends the link with why.
//!
//! It also binds `endpoint::fleet_transfer_socket`
//! (`fleet-<name>.transfer.sock`): each connection there is one `TRANSFER`
//! stream to the worker (`fleet::transfer::send` on it). `TRANSFER` streams
//! the worker opens are refused for now: this end receives no kind yet.
//! And `endpoint::fleet_pty_socket` (`fleet-<name>.pty.sock`): each
//! connection there is one `PTY` stream to the worker, a terminal into one
//! of its instances (`fleet::pty_client`, what a tty `exec` on the offered
//! machine dials). And `endpoint::fleet_tunnel_socket`
//! (`fleet-<name>.tunnel.sock`): each connection there is one `TUNNEL`
//! stream, a port forward's bridge (`tunnel::remote_opener`, what the
//! daemon's forwards and `devsandbox port` on the offered machine dial).
//!
//! It serves `AGENT` streams the worker opens (`fleet::agent`): each one
//! starts with a status byte, then (serving) a connection to the agent this
//! host's daemon answers `fleet.agent` with: its own `bridge::host_agent`
//! (the agents clients reported included) while its entry for the worker
//! has `agent = true`. That answer is asked on a short connection of its own
//! every `agent::REFRESH`, on a thread of its own (`agent::refresh`), so
//! `machine set <w> agent on|off` counts within that without a new link.
//!
//! When this is a dev build the worker isn't on, for the worker's platform
//! (`update::hub_pushes`, over both `Hello`s), it pushes its own binary
//! through that socket once the offer is made ([`push_build`]): the one
//! that said this link's `Hello`, so the build the worker decided on.
//! The worker takes it only while it waits for it (`dev-builds` on),
//! refusing at the header otherwise.
//!
//! `devsandbox fleet-link --peer <dialer>` is the other mode
//! (docs/architecture/daemon/fleet.md, *Peer links*): the dialed end of a
//! link `machine add <this host>` made on `<dialer>` ([`fleet_link_peer`],
//! [`run_peer`]). Still the opener (it writes the sentinel), saying
//! `update::hello_of` (its build and whether this host is the hub). IN:
//! each stream the dialer opens on `API`, `PTY`, `TUNNEL` or `TRANSFER` is
//! handed to this host's daemon as a connection whose first line is
//! `link.attach {peer: <dialer>, channel}` ([`attach_stream`]), so this
//! host's trust table for the dialer applies to every one. `AGENT` isn't
//! served: it only opens member → dialer. OUT: `fleet-<dialer>.sock` and
//! its `.pty`, `.tunnel`, `.transfer` siblings, each accept a stream to the
//! dialer; `agent-link-<dialer>.sock` (`endpoint::link_agent_socket`,
//! `fleet::agent::serve`) while the dialer serves `AGENT`. Once the
//! `Hello`s are through it announces the dialer as an inbound neighbor
//! (`links.inbound`, a refusal ends the link with why) and holds that
//! connection for the link's life, re-announcing on a new one when it drops
//! (a version handoff of this host's daemon) ([`Announce`]). Versions
//! follow the hub flag ([`follows`]): this end, not the hub, follows a hub
//! dialer's build, then ends the link (the dialer redials the new binary);
//! this end, the hub, pushes its dev build to a dialer that would take it.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
use crate::serve::fleet::worker::Follow;
use crate::serve::fleet::{self, agent, link, update};
use crate::serve::machines::check_name;

/// The CLI entry: the socket dir (created 0700 if missing), then [`run`] on
/// stdin/stdout, offering through this host's daemon (lazy start) and
/// asking it for the agent ([`agent_of`]) on a fresh connection each time.
/// `root`: the worker's fleet root (`--root`); `draining`: the worker's
/// `lend` is `drain` (`--drain`).
pub fn fleet_link(worker: &str, root: &Path, draining: bool) -> Result<()> {
    check_name(worker)?;
    let dir = endpoint::socket_dir()?;
    endpoint::ensure_private_dir(&dir)?;
    let connect = |dir: PathBuf| move || client::connect(&dir, "fleet-link").context("cannot reach this host's devsandbox serve");
    let (ask, name) = (connect(dir.clone()), worker.to_string());
    let agent = move || agent_of(&mut ask()?, &name);
    run(io::stdin(), io::stdout(), &dir, worker, root, draining, connect(dir.clone()), agent)
}

/// The link over `reader`/`writer` with its socket in `dir`: bind, sentinel
/// and `Hello`s (the peer must serve `API`), then the offer on the
/// connection `daemon` opens, held until the link ends; then accept and
/// relay (`link::opener_with`). From the offer on, `agent()` (the daemon's
/// `fleet.agent` answer) is asked every `agent::REFRESH` on a thread of its
/// own, and `AGENT` streams are answered from the last answer
/// (`agent::Gate`). `Ok` when the worker went away.
#[allow(clippy::too_many_arguments)]
pub fn run(
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    dir: &Path,
    worker: &str,
    root: &Path,
    draining: bool,
    daemon: impl FnOnce() -> Result<Conn>,
    agent: impl FnMut() -> Result<Option<PathBuf>> + Send + 'static,
) -> Result<()> {
    check_name(worker)?;
    if !root.is_absolute() {
        bail!("--root {} isn't an absolute path", root.display());
    }
    let mut held: Option<Conn> = None;
    let (api, socket) = (endpoint::fleet_socket(dir, worker), endpoint::fleet_transfer_socket(dir, worker));
    let (pty, tunnel) = (endpoint::fleet_pty_socket(dir, worker), endpoint::fleet_tunnel_socket(dir, worker));
    let sockets = [
        (fleet::LINK.channel, api.as_path()),
        (channel::TRANSFER, socket.as_path()),
        (channel::PTY, pty.as_path()),
        (channel::TUNNEL, tunnel.as_path()),
    ];
    let gate = Arc::new(agent::Gate::default());
    let serves = link::Serves::default().transfers(Arc::new(Receivers::new(|m| eprintln!("{m}")))).with(channel::AGENT, {
        let gate = Arc::clone(&gate);
        move || gate.open(Instant::now())
    });
    let push_to = socket.clone();
    let mut refreshing = None;
    let endpoint = link::Endpoint { sockets: &sockets, serves, outlet: None };
    let end = link::opener_with(&fleet::LINK, reader, writer, endpoint, |peer| {
        let mut conn = daemon()?;
        offer(&mut conn, worker, &peer.hash, root, draining)?;
        held = Some(conn);
        // Past the offer: the daemon knows the worker.
        refreshing = Some(agent::refresh(gate, agent::REFRESH, agent, |e| {
            eprintln!("fleet-link: cannot ask for the agent (not served meanwhile): {e:#}")
        }));
        push_build(fleet::LINK.hello, &peer.hash, push_to, own_binary);
        Ok(())
    });
    drop((held, refreshing));
    end
}

/// `fleet.agent {worker}` on `conn`: the agent socket this host's daemon
/// serves `worker` (`None`: not served).
pub fn agent_of(conn: &mut Conn, worker: &str) -> Result<Option<PathBuf>> {
    let r = conn.call("fleet.agent", json!({ "worker": worker }))?;
    if let Some(e) = r.error {
        bail!("fleet.agent: {}", e.message);
    }
    let agent = r.result.as_ref().and_then(|v| v.get("agent")).context("bad fleet.agent result")?;
    serde_json::from_value(agent.clone()).context("bad fleet.agent result")
}

/// This process's own binary: on Linux `/proc/self/exe`, the inode it runs
/// even when the path was replaced since; elsewhere `current_exe()`. The
/// worker checks what it got reports this link's `Hello` anyway.
pub(crate) fn own_binary() -> io::Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    return Ok("/proc/self/exe".into());
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe()
}

/// When a hub saying `ours` pushes to a worker saying `theirs`
/// (`update::hub_pushes`), `binary()`'s file to it on a thread of its own
/// through the link's transfer socket `socket` (`update::push`; its
/// connection waits in the backlog until the opener's accept loop runs).
/// A failure is on stderr (which the worker sees as ssh's); the worker's
/// wait ends on its own.
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
            eprintln!("fleet-link: cannot send this build to the worker: {e:#}");
        }
        pushed
    });
    pushed.ok()
}

/// `fleet.offer` on `conn`; a refusal is an error with the daemon's reason.
fn offer(conn: &mut Conn, worker: &str, version: &str, root: &Path, draining: bool) -> Result<()> {
    let r = conn.call("fleet.offer", json!({ "worker": worker, "version": version, "root": root, "draining": draining }))?;
    if let Some(e) = r.error {
        bail!("offer refused: {}", e.message);
    }
    Ok(())
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
    /// This host is the fleet's hub (`[serve] hub` in daemon.config.toml).
    pub hub: bool,
    /// A fresh connection to this host's daemon (lazy start), for
    /// `links.inbound`.
    pub daemon: Arc<dyn Fn() -> Result<Conn> + Send + Sync>,
    /// How this end follows a hub dialer's build.
    pub follow: Follow,
    /// The re-announce backoff's first wait ([`REANNOUNCE`]).
    pub reannounce: Duration,
    /// How long a wait for the hub's binary waits ([`PUSH_WAIT`]).
    pub push_wait: Duration,
}

/// The `--peer` CLI entry: the socket dir (created 0700 if missing), the hub
/// flag read as the daemon reads it (`daemon_config::load`), then
/// [`run_peer`] on stdin/stdout. Following the hub uses the release steps
/// (`update::Release`) and `member.toml`'s `dev-builds` / trust `full`,
/// read at the `Hello`s.
pub fn fleet_link_peer(peer: &str) -> Result<()> {
    check_name(peer)?;
    let dir = endpoint::socket_dir()?;
    endpoint::ensure_private_dir(&dir)?;
    let hub = crate::daemon_config::load()?.serve.hub;
    let member = fleet::member::path()?;
    let daemon_dir = dir.clone();
    let opts = PeerOptions {
        dir: dir.clone(),
        hub,
        daemon: Arc::new(move || client::connect(&daemon_dir, "fleet-link").context("cannot reach this host's devsandbox serve")),
        follow: Follow {
            steps: Arc::new(update::Release { socket_dir: dir }),
            dev_builds: Arc::new(move || fleet::member::load(&member).ok().flatten().is_some_and(|m| m.takes_dev_builds())),
        },
        reannounce: REANNOUNCE,
        push_wait: PUSH_WAIT,
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
    let dir = opts.dir.clone();
    let (api, transfer) = (endpoint::fleet_socket(&dir, peer), endpoint::fleet_transfer_socket(&dir, peer));
    let (pty, tunnel) = (endpoint::fleet_pty_socket(&dir, peer), endpoint::fleet_tunnel_socket(&dir, peer));
    let sockets = [
        (channel::API, api.as_path()),
        (channel::TRANSFER, transfer.as_path()),
        (channel::PTY, pty.as_path()),
        (channel::TUNNEL, tunnel.as_path()),
    ];
    // While the hub's binary is on its way nothing but it is served.
    let receiving = Arc::new(AtomicBool::new(false));
    let incoming = Arc::new(Incoming::default());
    let binary = Arc::new(incoming.receiver(Receivers::new(|m| eprintln!("{m}"))));
    let attached = |ch: Channel| {
        let (dir, peer, receiving) = (dir.clone(), peer.to_string(), Arc::clone(&receiving));
        move || if receiving.load(Ordering::Relaxed) { None } else { attach_stream(&dir, ch, &peer) }
    };
    let transfers = {
        let (attach, receiving) = (attached(Channel::Transfer), Arc::clone(&receiving));
        move || if receiving.load(Ordering::Relaxed) { binary.accept() } else { attach() }
    };
    let serves = link::Serves::default()
        .with(channel::API, attached(Channel::Api))
        .with(channel::PTY, attached(Channel::Pty))
        .with(channel::TUNNEL, attached(Channel::Tunnel))
        .with(channel::TRANSFER, transfers);
    let outlet = Arc::new(link::Outlet::default());
    let over = Arc::new(AtomicBool::new(false));
    let spec = link::Spec { hello: update::hello_of(opts.hub), ..fleet::LINK };
    let refused: Arc<Mutex<Option<String>>> = Arc::default();
    let mut wanted = None;
    let mut held = None;
    let mut agent_thread = None;
    let endpoint = link::Endpoint { sockets: &sockets, serves, outlet: Some(&outlet) };
    let end = link::opener_with(&spec, reader, writer, endpoint, |theirs| {
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
        let on_refused = {
            let (refused, outlet) = (Arc::clone(&refused), Arc::clone(&outlet));
            move |why: String| {
                eprintln!("fleet-link: refused: {why}");
                *refused.lock().unwrap() = Some(why);
                outlet.hang_up();
            }
        };
        held = Some(Announce::hold(conn, params, Arc::clone(&opts.daemon), opts.reannounce, on_refused));
        if theirs.caps & caps::AGENT != 0 {
            let (path, outlet, over) = (endpoint::link_agent_socket(&dir, peer), Arc::clone(&outlet), Arc::clone(&over));
            agent_thread = std::thread::Builder::new()
                .name("fleet-link-agent".into())
                .spawn(move || {
                    if let Err(e) = agent::serve(&path, &outlet, &over, agent::PROBE) {
                        eprintln!("fleet-link: {e:#}");
                    }
                })
                .ok();
        }
        if opts.hub && !update::is_hub(&theirs.hash) {
            push_build(spec.hello, &theirs.hash, transfer.clone(), own_binary);
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
        bail!("refused: {why}");
    }
    end
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
/// [`REANNOUNCE_MAX`] while the daemon can't be reached. A refusal then
/// goes to `on_refused` (which ends the link) and the thread ends. Dropping
/// it stops the thread and closes the connection.
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
        on_refused: impl FnOnce(String) + Send + 'static,
    ) -> Announce {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("fleet-link-announce".into())
            .spawn(move || {
                let mut conn = conn;
                loop {
                    // Nothing comes on it: a notification is ignored, an error is the drop.
                    while !stopping.load(Ordering::Relaxed) {
                        if conn.next_notification(Duration::from_millis(100)).is_err() {
                            break;
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
                            Ok((Err(why), _)) => return on_refused(why),
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

/// One stream peer `peer` opens on `channel`, handed to this host's daemon
/// in `dir`: a socket pair whose far end a thread of its own takes (this
/// runs on the link's read thread). The thread connects the daemon, sends
/// `link.attach {peer, channel}` as the connection's first line, reads the
/// answer (nothing past it), and splices the stream to the connection (an
/// `API` stream's first byte is the daemon's, fleet plan decision 27). A
/// refused attach closes the stream, with why on stderr. `None` when no
/// pair or thread could be made.
pub fn attach_stream(dir: &Path, channel: Channel, peer: &str) -> Option<mux::Conn> {
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().ok()?;
    let (dir, peer) = (dir.to_path_buf(), peer.to_string());
    let spawned = std::thread::Builder::new().name("fleet-link-attach".into()).spawn(move || {
        let daemon = match attach(&dir, channel, &peer) {
            Ok(daemon) => daemon,
            Err(e) => return eprintln!("fleet-link: cannot hand a {} stream to this host's devsandbox: {e:#}", channel.as_str()),
        };
        link::splice(ours, daemon.into());
    });
    spawned.ok()?;
    Some(theirs.into())
}

/// A connection to the daemon in `dir` past `link.attach {peer, channel}`'s
/// `ok`, its answer read byte by byte so nothing past it is taken.
fn attach(dir: &Path, channel: Channel, peer: &str) -> Result<endpoint::Stream> {
    let mut stream = endpoint::connect(dir).context("cannot connect")?;
    stream.set_read_timeout(Some(ATTACH_TIMEOUT)).context("cannot set a read timeout")?;
    let line = json!({ "id": 0, "method": "link.attach", "params": { "peer": peer, "channel": channel.as_str() } });
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
    use std::io::{BufRead, BufReader};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::*;
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::daemon::Exit;
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

    /// The worker's end, faked: the answerer of `fleet::LINK` over `io`,
    /// each `API` stream a connection to the daemon in `daemon`, as the
    /// worker's (`daemon::Shared::open_lent`) is.
    fn fake_worker(io: UnixStream, daemon: PathBuf) -> std::thread::JoinHandle<link::End> {
        let (r, w) = (io.try_clone().unwrap(), io);
        std::thread::spawn(move || {
            let open = move || Some(endpoint::connect(&daemon).ok()?.into());
            let serves = link::Serves::default()
                .with(fleet::LINK.channel, open)
                .with(channel::PTY, fake_pty)
                .with(channel::TUNNEL, fake_tunnel)
                .transfers(Arc::new(Receivers::new(|_| {})));
            link::answerer_with(&fleet::LINK, r, w, link::Endpoint { serves, ..link::Endpoint::default() }, |_| Ok(()), || {})
        })
    }

    /// A `PTY` stream's worker end, faked: past the header, `stdout` `hi
    /// <session>`, then `exit 4`.
    fn fake_pty() -> Option<crate::devsbd::mux::Conn> {
        use crate::devsbd::rpc::{self, Exit, Record};
        let (ours, theirs) = UnixStream::pair().ok()?;
        std::thread::spawn(move || {
            let Ok(Some(Record::Call(call))) = rpc::read_record(&mut &ours) else { return };
            let hi = format!("hi {}", call.params.str("session").unwrap_or_default());
            let _ = rpc::write_record(&mut &ours, &Record::Stdout(hi.into_bytes()));
            let _ = rpc::write_record(&mut &ours, &Record::Exit(Exit::Code(4)));
        });
        Some(theirs.into())
    }

    /// A `TUNNEL` stream's worker end, faked: past the request, an error
    /// header naming its instance and port.
    fn fake_tunnel() -> Option<crate::devsbd::mux::Conn> {
        use crate::commands::tunnel::Header;
        let (mut ours, theirs) = UnixStream::pair().ok()?;
        std::thread::spawn(move || {
            let Ok(req) = fleet::tunnel::read_request(&mut ours) else { return };
            let error = format!("no {} on {}", req.instance.unwrap_or_default(), req.port);
            let _ = ours.write_all(Header::Error { error }.line().as_bytes());
        });
        Some(theirs.into())
    }

    /// The fleet root `lap` offers in these tests (a path on the worker;
    /// the hub never reads it).
    const WORKER_ROOT: &str = "/worker/fleet/hub";

    /// The hub's `fleet-link` for worker `lap` over `io`, its socket and
    /// daemon in `hub`.
    fn hub_link(io: UnixStream, hub: PathBuf) -> std::thread::JoinHandle<Result<()>> {
        let (r, w) = (io.try_clone().unwrap(), io);
        std::thread::spawn(move || run(r, w, &hub.clone(), "lap", Path::new(WORKER_ROOT), false, move || Ok(conn_at(&hub)), || Ok(None)))
    }

    /// A worker linked to its hub: the hub's daemon registers it on offer,
    /// and quick calls and verbs reach the worker's daemon over the link
    /// through the offered dial, `link::handshake` and `forward::verb` as
    /// for an ssh machine.
    #[test]
    fn an_offered_worker_answers_quick_calls_and_verbs_over_its_link() {
        let base = scratch("fleet-e2e");
        let (worker_dir, hub_dir) = (base.join("w"), base.join("hub"));
        let worker_root = base.join("worker-root");
        std::fs::create_dir_all(&worker_root).unwrap();
        std::fs::write(worker_root.join(crate::config::CONFIG_FILE), "[sandbox.web]\nimage = \"a\"\n").unwrap();
        let worker_root = std::fs::canonicalize(&worker_root).unwrap();
        let worker_state = base.join("worker-data/state.toml");
        std::fs::create_dir_all(worker_state.parent().unwrap()).unwrap();
        std::fs::write(
            &worker_state,
            format!(
                "[instance.web-w]\nsandbox = \"web\"\ncontainer = \"devsandbox-web-w\"\nfolder = \"/w\"\n\
                 workspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                worker_root.to_str().unwrap()
            ),
        )
        .unwrap();

        let (runner, verb_dir) = crate::serve::verbs::tests::script_runner("fleet-e2e", r#"for a; do n=$a; done; echo "hi $n""#);
        let mut worker = opts(1, 60_000);
        worker.verbs = Some(runner);
        worker.state = Some(worker_state);
        worker.home = Some(base.join("worker-home"));
        worker.machines = Some(base.join("worker-machines.toml"));
        let registry = base.join("machines.toml");
        let mut hub = opts(1, 300);
        hub.hub = true;
        hub.remotes = Some(base.join("hub-data/remotes.json"));
        hub.machines = Some(registry.clone());
        hub.state = Some(base.join("hub-data/state.toml"));
        let daemons = [spawn_daemon(&worker_dir, worker), spawn_daemon(&hub_dir, hub)];
        drop(conn_at(&worker_dir));

        let (worker_io, hub_io) = UnixStream::pair().unwrap();
        let worker_end = fake_worker(worker_io, worker_dir.clone());
        let hub_end = hub_link(hub_io.try_clone().unwrap(), hub_dir.clone());
        let lap = Entry { roots: vec![WORKER_ROOT.into()], ..Entry::offered("lap") };
        wait_for(|| machines::load(&registry).is_ok_and(|e| e == [lap.clone()]), "the offer registers `lap` and its root");
        assert!(endpoint::fleet_socket(&hub_dir, "lap").exists());
        assert!(endpoint::fleet_transfer_socket(&hub_dir, "lap").exists());
        assert!(endpoint::fleet_pty_socket(&hub_dir, "lap").exists());
        assert!(endpoint::fleet_tunnel_socket(&hub_dir, "lap").exists());

        // A terminal: one `PTY` stream through the link's pty socket.
        use crate::serve::fleet::pty_client;
        let mode = pty_client::Mode::Session { id: "1700000000-ab12".into(), create: None };
        let request = pty_client::Request { root: WORKER_ROOT.into(), instance: "web-w".into(), mode };
        let (_keys, input) = UnixStream::pair().unwrap();
        let mut out = Vec::new();
        let stream = endpoint::Stream::connect(endpoint::fleet_pty_socket(&hub_dir, "lap")).unwrap();
        let mut err = Vec::new();
        let term = pty_client::Terminal { input: Some(std::os::fd::AsRawFd::as_raw_fd(&input)), output: &mut out, errors: &mut err, size: &|| None };
        let end = pty_client::attach(stream, &pty_client::header(&request), term);
        assert_eq!((end, out), (pty_client::End::Exit(crate::devsbd::rpc::Exit::Code(4)), b"hi 1700000000-ab12".to_vec()));

        // A forward's bridge: one `TUNNEL` stream through the tunnel socket.
        use crate::commands::tunnel::{self, Request, Via};
        let socket = endpoint::fleet_tunnel_socket(&hub_dir, "lap");
        let request = Request { root: WORKER_ROOT.into(), instance: Some("web-w".into()), service: None, port: 3000 };
        let via = Via::Fleet { socket, request };
        let open = tunnel::remote_opener("lap".into(), Box::new(move || Ok(via.clone())));
        assert_eq!(open(&std::sync::atomic::AtomicBool::new(false)).err().as_deref(), Some("no web-w on 3000"));

        let mut c = conn_at(&hub_dir);
        // A quick call: the worker's answer, over the link.
        let r = c.call("instances.resolve", json!({"name": "web-w", "machine": "lap"})).unwrap();
        assert_eq!(r.result, Some(json!({"matches": ["web-w"], "found": [{"name": "web-w", "machine": null, "root": worker_root, "sandbox": "web"}]})), "{r:?}");
        let rows = c.call("machines.list", Value::Null).unwrap().result.unwrap();
        assert_eq!((&rows[0]["name"], &rows[0]["kind"], &rows[0]["target"]), (&json!("lap"), &json!("offered"), &Value::Null));
        assert_eq!(rows[0]["state"], "connected");
        assert_eq!(rows[0]["version"], "0.6.0", "the worker daemon's hello");

        // A verb: its own `API` stream, streamed back under this request's id.
        let stream = stream_at(&hub_dir);
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let params = json!({"root": verb_dir, "env": {}, "instance": "web-w", "machine": "lap"});
        use std::io::Write as _;
        writeln!(&stream, "{}", json!({"id": 5, "method": "stop", "params": params})).unwrap();
        let mut out = String::new();
        let answer = loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "closed before the answer");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id").is_some() {
                break msg;
            }
            if msg["method"] == "output" && msg["params"]["stream"] == "stdout" {
                assert_eq!(msg["params"]["request"], 5);
                out.push_str(msg["params"]["data"].as_str().unwrap());
            }
        };
        assert!(answer["error"].is_null(), "{answer}");
        assert_eq!(out, "hi web-w\n");
        drop((reader, stream, c));

        // The worker going away ends the hub's end, which unlinks its socket.
        hub_io.shutdown(Shutdown::Both).unwrap();
        hub_end.join().unwrap().unwrap();
        assert!(!endpoint::fleet_socket(&hub_dir, "lap").exists());
        assert!(!endpoint::fleet_transfer_socket(&hub_dir, "lap").exists());
        assert!(!endpoint::fleet_pty_socket(&hub_dir, "lap").exists());
        assert!(!endpoint::fleet_tunnel_socket(&hub_dir, "lap").exists());
        assert!(matches!(worker_end.join().unwrap(), link::End::Closed));
        assert!(conn_at(&worker_dir).call("shutdown", json!({})).unwrap().error.is_none());
        let [worker, hub] = daemons;
        assert_eq!(worker.join().unwrap().unwrap(), Exit::Shutdown);
        // A hub never idles out: stopped by hand.
        assert!(conn_at(&hub_dir).call("shutdown", json!({})).unwrap().error.is_none());
        assert_eq!(hub.join().unwrap().unwrap(), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&verb_dir);
    }

    /// A daemon that isn't a hub refuses the offer: the link ends with why,
    /// its socket gone, nothing registered.
    #[test]
    fn a_refused_offer_ends_the_link() {
        let base = scratch("fleet-nohub");
        let hub_dir = base.join("hub");
        let registry = base.join("machines.toml");
        let mut not_hub = opts(1, 300);
        not_hub.machines = Some(registry.clone());
        let daemon = spawn_daemon(&hub_dir, not_hub);
        drop(conn_at(&hub_dir));
        let (worker_io, hub_io) = UnixStream::pair().unwrap();
        let worker_end = fake_worker(worker_io, base.join("nowhere"));
        let err = hub_link(hub_io, hub_dir.clone()).join().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("offer refused: this devsandbox isn't a fleet hub"), "{err:#}");
        assert!(!endpoint::fleet_socket(&hub_dir, "lap").exists());
        assert!(!endpoint::fleet_transfer_socket(&hub_dir, "lap").exists());
        assert!(!endpoint::fleet_pty_socket(&hub_dir, "lap").exists());
        assert!(!registry.exists());
        assert!(matches!(worker_end.join().unwrap(), link::End::Closed));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_bad_worker_name_is_refused_before_binding() {
        let (a, b) = UnixStream::pair().unwrap();
        let dir = std::env::temp_dir();
        let err = run(a, b, &dir, "a/b", Path::new("/r"), false, || unreachable!(), || Ok(None)).unwrap_err();
        assert!(format!("{err:#}").contains("machine name"), "{err:#}");
        let (a, b) = UnixStream::pair().unwrap();
        let err = run(a, b, &dir, "lap", Path::new("rel"), false, || unreachable!(), || Ok(None)).unwrap_err();
        assert!(format!("{err:#}").contains("absolute"), "{err:#}");
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
        o.lend = Some(data.join("lend.toml"));
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

    /// The dialer's end, faked as 7d's daemon answers it: the answerer of a
    /// peer link saying `hello`, each `API` stream K opens handed to the
    /// daemon in `daemon` as peer `K`'s (`attach_stream`, the in-process
    /// path's stand-in), `PTY` streams answered `dialer pty`; its outlet
    /// `outlet`.
    fn fake_dialer(io: UnixStream, hello: &'static str, daemon: PathBuf, outlet: Arc<link::Outlet>) -> std::thread::JoinHandle<link::End> {
        let (r, w) = (io.try_clone().unwrap(), io);
        std::thread::spawn(move || {
            let serves = link::Serves::default()
                .with(channel::API, move || attach_stream(&daemon, Channel::Api, "K"))
                .with(channel::PTY, tagged(b"dialer pty"));
            let spec = link::Spec { hello, ..fleet::LINK };
            link::answerer_with(&spec, r, w, link::Endpoint { serves, outlet: Some(&outlet), ..link::Endpoint::default() }, |_| Ok(()), || {})
        })
    }

    /// K's `fleet-link --peer L` over `io`, its daemon and sockets in `dir`,
    /// a member; following through `steps` (dev builds taken).
    fn peer_link(io: UnixStream, dir: PathBuf, steps: Arc<dyn update::Steps>) -> std::thread::JoinHandle<Result<()>> {
        let (r, w) = (io.try_clone().unwrap(), io);
        let daemon_dir = dir.clone();
        let opts = PeerOptions {
            dir,
            hub: false,
            daemon: Arc::new(move || Ok(conn_at(&daemon_dir))),
            follow: Follow { steps, dev_builds: Arc::new(|| true) },
            reannounce: Duration::from_millis(20),
            push_wait: Duration::from_secs(5),
        };
        std::thread::spawn(move || run_peer(r, w, "L", opts))
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
    /// table through `fleet-L.*.sock`; the neighbor goes with the link.
    #[test]
    fn a_peer_link_announces_its_dialer_and_hands_its_streams_to_the_daemon() {
        let base = scratch("peer-e2e");
        let (k_dir, k_data, k_own, k_opts) = peer_daemon(&base, "k");
        let (l_dir, l_data, l_own, l_opts) = peer_daemon(&base, "l");
        let daemons = [spawn_daemon(&k_dir, k_opts), spawn_daemon(&l_dir, l_opts)];
        drop((conn_at(&k_dir), conn_at(&l_dir)));

        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let outlet = Arc::new(link::Outlet::default());
        let dialer = fake_dialer(dialer_io, update::hello_of(false), l_dir.clone(), Arc::clone(&outlet));
        let k_end = peer_link(k_io.try_clone().unwrap(), k_dir.clone(), Arc::new(update::tests::Fake::default()));
        let l_fleet = l_data.join("fleet/K");
        let want = json!([l_own, l_fleet]);
        // A machine on K: its held link's state and its daemon's semver.
        wait_for(|| inbound_row(&k_dir).is_some_and(|r| r["roots"] == want && r["state"] == "connected"), "L listed with its roots");
        let row = inbound_row(&k_dir).unwrap();
        assert_eq!((&row["name"], &row["target"], &row["state"]), (&json!("L"), &Value::Null, &json!("connected")));
        assert_eq!(row["version"], "0.6.0");
        for socket in [endpoint::fleet_socket(&k_dir, "L"), endpoint::fleet_pty_socket(&k_dir, "L")] {
            assert!(socket.exists(), "{}", socket.display());
        }
        assert!(!endpoint::link_agent_socket(&k_dir, "L").exists(), "the dialer serves no AGENT");

        // A registry entry can't take a live neighbor's name.
        let mut k = conn_at(&k_dir);
        let e = k.call("machines.add", json!({"target": "me@l", "name": "L", "roots": ["/x"]})).unwrap().error.unwrap();
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
        UnixStream::connect(endpoint::fleet_pty_socket(&k_dir, "L")).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, b"dialer pty");

        // The link ends: K's end returns, its sockets and the neighbor go.
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
        let daemon = spawn_daemon(&k_dir, k_opts);
        drop(conn_at(&k_dir));
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let dialer = fake_dialer(dialer_io, update::hello_of(false), base.join("nowhere"), Arc::default());
        let err = peer_link(k_io, k_dir.clone(), Arc::new(update::tests::Fake::default())).join().unwrap().unwrap_err();
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
        let again = crate::serve::daemon::Options { machines: k_opts.machines.clone(), state: k_opts.state.clone(), lend: k_opts.lend.clone(), ..opts(2, 60_000) };
        let first = spawn_daemon(&k_dir, k_opts);
        let l_daemon = spawn_daemon(&l_dir, l_opts);
        drop((conn_at(&k_dir), conn_at(&l_dir)));
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let dialer = fake_dialer(dialer_io, update::hello_of(false), l_dir.clone(), Arc::default());
        let k_end = peer_link(k_io.try_clone().unwrap(), k_dir.clone(), Arc::new(update::tests::Fake::default()));
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

    #[test]
    fn only_a_non_hub_follows_a_hub() {
        let ours = update::hello_of(false);
        let semver = env!("CARGO_PKG_VERSION");
        let platform = update::PLATFORM;
        let published = format!("9.9.9 abc published {platform} hub");
        assert_eq!(follows(ours, &published, false), Decision::Download { version: "9.9.9".into() });
        assert_eq!(follows(ours, update::hello_of(true), false), Decision::Same, "the same build");
        let dev = format!("{semver} zzz dev {platform} hub");
        assert_eq!(follows(ours, &dev, true), Decision::Receive);
        assert!(matches!(follows(ours, &dev, false), Decision::Stop(_)));
        let elsewhere = format!("{semver} zzz dev other/os hub");
        assert!(matches!(follows(ours, &elsewhere, true), Decision::Stop(why) if why.contains("can't run on")));
        // No hub, or this end is it: nothing to follow.
        assert_eq!(follows(ours, &published.replace(" hub", " member"), false), Decision::Same);
        assert_eq!(follows(update::hello_of(true), &published, false), Decision::Same);
        assert_eq!(follows(ours, "9.9.9 abc published linux/x86_64", false), Decision::Same, "a lending hello says no hub");
    }

    /// A hub dialer on a published build: K's end drops the link, installs
    /// that version and hands off, and says so.
    #[test]
    fn a_member_follows_a_published_hub_dialer() {
        let base = scratch("peer-follow");
        std::fs::create_dir_all(&base).unwrap();
        let (dialer_io, k_io) = UnixStream::pair().unwrap();
        let hub: &'static str = Box::leak(format!("9.9.9 abc published {} hub", update::PLATFORM).into_boxed_str());
        let dialer = fake_dialer(dialer_io, hub, base.join("nowhere"), Arc::default());
        let fake = Arc::new(update::tests::Fake::reporting("9.9.9"));
        let err = peer_link(k_io, base.clone(), fake.clone()).join().unwrap().unwrap_err();
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
        let dialer = fake_dialer(dialer_io, hub, base.join("nowhere"), Arc::clone(&outlet));
        let fake = Arc::new(update::tests::Fake::building(hub.trim_end_matches(" hub")));
        let k_end = peer_link(k_io.try_clone().unwrap(), base.clone(), fake.clone());
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
