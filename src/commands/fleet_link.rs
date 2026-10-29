//! `devsandbox fleet-link --peer <dialer>` (hidden)
//! (docs/architecture/daemon/fleet.md, *Peer links*): the dialed end of a
//! link `machine add <this host>` made on `<dialer>` ([`fleet_link_peer`],
//! [`run_peer`]). The opener (it writes the sentinel), saying
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
/// through the link's transfer socket `socket` (`update::push`; its
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
        let again = crate::serve::daemon::Options { machines: k_opts.machines.clone(), state: k_opts.state.clone(), member: k_opts.member.clone(), ..opts(2, 60_000) };
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
