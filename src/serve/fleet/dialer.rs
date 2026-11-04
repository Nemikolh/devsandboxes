//! The dialer's half of a peer link (docs/architecture/daemon/fleet.md,
//! *Peer links*; docs/architecture/daemon/machines.md, *ssh*): how this
//! host's daemon carries the link to one of its ssh machines
//! ([`Dialer::hold`], the `machines::service::Hold` of every ssh entry).
//!
//! It dials `devsandbox fleet-link --peer <this host's member name>`
//! ([`link_args`]; the entry's `command` in place of `devsandbox`) on the
//! entry's target through the [`Transport`] and runs the answerer of
//! `fleet::LINK` on it (it reads the sentinel the far end writes). OUT: it
//! binds `fleet-<entry>.sock` and its `.pty`, `.tunnel`, `.transfer`
//! siblings in the socket dir, each accept one stream to the machine, so
//! the entry's link ([`dial`]) and every other local client reach the
//! machine as they reach an inbound one. IN: each stream the machine
//! opens (`API`, `PTY`, `TUNNEL`, `TRANSFER`, `AGENT`, `ROUTE`) goes to this daemon
//! in-process ([`Open`]: `Shared::open_lent` with the entry's name as the
//! peer, the key and introducer it proved, its fleet root
//! `fleet/<entry>/`), under this host's trust table for it; `AGENT` is answered with this host's agent while
//! `member.toml` grants it to the key the machine proved (`daemon::agent_for`).
//!
//! Right after the `Hello`s both ends prove their member keys
//! (`super::proof`): this host's identity (`proof::Identity`: its name,
//! `member.key`, `membership.json`) is read at each dial, the machine must
//! prove the key its entry's name has in the membership log (or a held
//! introduction), and a log behind the machine's takes its newer entries
//! through the daemon's `membership.adopt` path ([`Dialer::adopt`]), the
//! link then redialed if they moved the hub (`proof::hub_moved`). No
//! identity, no dial.
//!
//! This host's hello (`update::hello_of`: its role and its fleet's hub) is
//! read at each dial, so a hub change counts from the next one
//! (`service::Handle::cycle` ends every link then); a machine naming
//! another hub is refused (`update::same_fleet`, fleet plan decision 34).
//!
//! Versions follow the hello's hub flag (fleet plan decision 25,
//! `commands::fleet_link::follows`): a machine that is the hub while this
//! host isn't is followed before anything is served. Another published
//! build is downloaded, installed and handed off to (`update::update`),
//! the link refused meanwhile; another dev build for this platform, with
//! `member.toml`'s `dev-builds` on or trust `full`, keeps the link up
//! serving only the `binary` transfer the hub pushes, then installs it and
//! hands off (`update::install_received`). This host, the hub, pushes its
//! dev build to a machine that would take it (`fleet_link::push_build`
//! through `fleet-<entry>.sock`). Two non-hubs follow nothing.
//!
//! The link's socket is bound as `link::Socket` binds one (the newest
//! binder wins), so after a version handoff the next daemon's links take
//! it over: the old daemon's ssh dies with it, the far end's `fleet-link`
//! exits, the new daemon redials.

use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};

use super::follow::{self, Incoming, PUSH_WAIT};
use super::membership::Stored;
use super::proof::{self, Identity, Proven};
use super::lent::Channel;
use super::transfer::Receivers;
use super::transport::Transport;
use super::update::{self, Build, Decision};
use super::follow::Follow;
use super::{LINK, link};
use crate::commands::fleet_link;
use crate::devsbd::mux::Conn;
use crate::devsbd::proto::channel;
use crate::serve::endpoint;
use crate::serve::machines::service::Held;
use crate::serve::machines::ssh::{Dial, Duplex, RouteDial};
use crate::serve::machines::{Entry, check_name};

/// How the daemon answers one stream a machine opens on `Channel`, the
/// machine being who its link proved (`Proven`: its entry's name, its key,
/// its introducer; `newer` always `None`): the machine's end of a new
/// connection, `None` to refuse it. Runs on the link's read thread: it
/// must return fast.
pub type Open = Arc<dyn Fn(&Proven, Channel) -> Option<Conn> + Send + Sync>;

/// The far end's devsandbox args: `fleet-link --peer <member>`, `member`
/// being this host's member name (`fleet::member`), which the machine
/// knows this host by.
pub fn link_args(member: &str) -> Result<Vec<String>> {
    check_name(member)?;
    Ok(["fleet-link", "--peer", member].map(String::from).to_vec())
}

/// The daemon's [`Dial`] of a machine, ssh or inbound: a
/// connection to its `fleet-<name>.sock` in `socket_dir` (one `API` stream
/// on its link, `link::connect`, the daemon from its first byte:
/// `machines::link::handshake` with `Over::Link`). Never its route socket
/// (`fleet::route::connect`): this connection is the machine's link state,
/// which this host's link report says as a direct link.
pub fn dial(socket_dir: PathBuf) -> Dial {
    Arc::new(move |entry: &Entry| {
        let path = endpoint::fleet_socket(&socket_dir, &entry.name);
        let stream =
            link::connect(&path, channel::API).map_err(|e| anyhow::anyhow!("its link isn't up ({}: {e})", path.display()))?;
        Ok(Duplex::socket(stream))
    })
}

/// The daemon's [`RouteDial`] of a routed machine: one `API` stream over
/// `route::connect` (its `fleet-<name>.sock` if a link came up meanwhile,
/// else its `route-<name>.sock`, opened over the graph's paths), the daemon
/// from its first byte like [`dial`]'s. A routed machine's forwarded calls
/// and verbs go this way; nothing holds it.
pub fn routed(socket_dir: PathBuf) -> RouteDial {
    Arc::new(move |name: &str| {
        let stream = super::route::connect(&socket_dir, name, channel::API).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Duplex::socket(stream))
    })
}

/// What the daemon's ssh machines' links need.
pub struct Dialer {
    pub transport: Transport,
    /// The socket dir: where the link's socket is bound.
    pub dir: PathBuf,
    /// This host's member name, read at each dial (a `member.toml` change
    /// counts from the next one).
    pub member: Arc<dyn Fn() -> Result<String> + Send + Sync>,
    /// IN: the daemon's answer to each of the machine's streams.
    pub open: Open,
    /// What this host says on a peer link (`update::hello_for` of its
    /// `hub`, as the daemon keeps it), read at each dial.
    pub hello: Arc<dyn Fn() -> &'static str + Send + Sync>,
    /// How this host follows a hub machine's build.
    pub follow: Follow,
    /// The sentinel's, the `Hello`s' and the key proof's wait
    /// (`service::Timing::handshake`).
    pub handshake: Duration,
    pub log: fn(&str),
    /// This host's identity, read at each dial ([`Identity::load`]).
    pub identity: Arc<dyn Fn() -> Result<Identity> + Send + Sync>,
    /// Newer membership entries (and introductions) a machine carried,
    /// stored as `membership.adopt` stores them (`hub` written from the
    /// log, the daemon following it); `Err` says why not.
    pub adopt: Arc<dyn Fn(&Stored) -> Result<(), String> + Send + Sync>,
}

impl Dialer {
    /// Hold `entry`'s link until it ends (module doc), telling `held` when
    /// it's up; returns why it ended, with ssh's stderr tail (where the far
    /// end's refusal is).
    pub fn hold(&self, entry: &Entry, held: &Held) -> String {
        let Some(target) = entry.target() else {
            return format!("{} has no ssh target", entry.name);
        };
        let args = match (self.member)().and_then(|m| link_args(&m)) {
            Ok(args) => args,
            Err(e) => return format!("cannot name this host for the link: {e:#}"),
        };
        let identity = match (self.identity)() {
            Ok(identity) => identity,
            Err(e) => return format!("{e:#}"),
        };
        let mut duplex = match self.transport.dial(target, entry.command(), &args) {
            Ok(duplex) => duplex,
            Err(e) => return format!("{e:#}"),
        };
        let clones = (duplex.stream.try_clone(), duplex.stream.try_clone(), duplex.stream.try_clone());
        let (Ok(reader), Ok(writer), Ok(killer)) = clones else {
            return "cannot clone the link's socket".into();
        };
        if !held.hold(duplex.stream.try_clone().ok()) {
            return "stopping".into();
        }
        // Cleared once the `Hello`s are through: the mux reads unbounded.
        let _ = duplex.stream.set_read_timeout(Some(self.handshake));
        let end = self.answer(entry, identity, &duplex, reader, writer, killer, held);
        let why = match end {
            Ended::Link(end) => end_why(end, &duplex),
            Ended::Follow(follow) => {
                // The ssh goes first: nothing is served while this binary changes.
                duplex.peer.close();
                if held.stopped() {
                    return "stopping".into();
                }
                self.follow(&entry.name, follow, &duplex, held)
            }
        };
        duplex.peer.close();
        why
    }

    /// The answerer over `duplex` until the link ends, proving `identity`.
    #[allow(clippy::too_many_arguments)]
    fn answer(
        &self,
        entry: &Entry,
        identity: Identity,
        duplex: &Duplex,
        reader: endpoint::Stream,
        writer: endpoint::Stream,
        killer: endpoint::Stream,
        held: &Held,
    ) -> Ended {
        let name = entry.name.as_str();
        let dir = &self.dir;
        let socket = endpoint::fleet_socket(dir, name);
        // While the hub's binary is on its way nothing but it is served.
        let receiving = AtomicBool::new(false);
        let incoming = Arc::new(Incoming::default());
        let binary = Arc::new(incoming.receiver(Receivers::new(self.log)));
        // Who the machine proved it is, set once the `Hello`s and the proof
        // are through (before any stream is served).
        let proven: OnceLock<Proven> = OnceLock::new();
        let served = |ch: Channel| {
            let (receiving, proven) = (&receiving, &proven);
            move || if receiving.load(Ordering::Relaxed) { None } else { (self.open)(proven.get()?, ch) }
        };
        let transfers = {
            let (attach, receiving, binary) = (served(Channel::Transfer), &receiving, &binary);
            move || if receiving.load(Ordering::Relaxed) { binary.accept() } else { attach() }
        };
        let serves = link::Serves::default()
            .with(channel::API, served(Channel::Api))
            .with(channel::PTY, served(Channel::Pty))
            .with(channel::TUNNEL, served(Channel::Tunnel))
            .with(channel::AGENT, served(Channel::Agent))
            .with(channel::ROUTE, served(Channel::Route))
            .with(channel::TRANSFER, transfers);
        let ours = (self.hello)();
        let spec = link::Spec { hello: ours, ..LINK };
        let wanted: Mutex<Option<String>> = Mutex::new(None);
        let mut end_link = duplex.stream.try_clone().ok().map(follow::shutting);
        let auth = proof::Auth { identity, expect: name.to_string(), log: self.log };
        let endpoint = link::Endpoint { socket: Some(&socket), serves, outlet: None, auth: Some(auth) };
        let end = link::answerer_with(
            &spec,
            reader,
            writer,
            endpoint,
            |peer| {
                let _ = duplex.stream.set_read_timeout(None);
                let Some(p) = &peer.proven else {
                    return Err(format!("{name} proved no member key"));
                };
                let _ = proven.set(Proven { newer: None, ..p.clone() });
                if let Some(newer) = peer.proven.as_ref().and_then(|p| p.newer.as_ref()) {
                    (self.adopt)(newer).map_err(|e| format!("cannot take {name}'s newer membership entries: {e}"))?;
                    (self.log)(&format!("machine {name}: took its newer membership log ({})", newer.log.head()));
                    if let Some(why) = proof::hub_moved(ours, newer) {
                        return Err(why);
                    }
                }
                update::same_fleet(name, ours, &peer.hash)?;
                let label = Build::parse(&peer.hash).label();
                match fleet_link::follows(ours, &peer.hash, (self.follow.dev_builds)()) {
                    Decision::Same => {}
                    Decision::Download { version } => {
                        *wanted.lock().unwrap() = Some(version);
                        return Err(format!("{name}, the hub, runs devsandbox {label}: updating this one"));
                    }
                    Decision::Stop(why) => return Err(why),
                    Decision::Receive => {
                        let scratch = update::scratch_dir(&std::env::temp_dir()).map_err(|e| format!("cannot take the hub's build: {e:#}"))?;
                        receiving.store(true, Ordering::Relaxed);
                        let seq = incoming.expect(peer.hash.clone(), scratch, end_link.take());
                        incoming.watch(seq, PUSH_WAIT);
                        let why = format!("{name}, the hub, runs devsandbox {label}: receiving its binary");
                        (self.log)(&format!("machine {why}"));
                        held.note(why);
                        return Ok(());
                    }
                }
                if update::is_hub(ours) {
                    fleet_link::push_build(ours, &peer.hash, socket.clone(), fleet_link::own_binary);
                }
                held.up();
                Ok(())
            },
            move || {
                let _ = killer.shutdown(Shutdown::Both);
            },
        );
        if let Some(want) = incoming.take() {
            return Ended::Follow(Following::Dev(want, end));
        }
        match wanted.into_inner().unwrap() {
            Some(version) => Ended::Follow(Following::Published(version)),
            None => Ended::Link(end),
        }
    }

    /// Follow the hub machine `name`'s build (module doc); the outcome is
    /// why the link ended (the machine's `error` until the handoff, or the
    /// next attempt).
    fn follow(&self, name: &str, following: Following, duplex: &Duplex, held: &Held) -> String {
        let steps = self.follow.steps.as_ref();
        let why = match following {
            Following::Dev(want, end) => {
                let label = Build::parse(&want.hello).label();
                let why = match &want.received {
                    Some(binary) => {
                        held.note(format!("updating to {label}"));
                        match update::install_received(steps, binary, &want.hello) {
                            Ok(()) => format!("updated to {label}: handing off"),
                            Err(e) => format!("update to {label} failed: {e:#}"),
                        }
                    }
                    None => {
                        let why = want.why.clone().unwrap_or_else(|| format!("{} before the hub's binary arrived", end_why(end, duplex)));
                        format!("update to {label} failed: {why}")
                    }
                };
                let _ = std::fs::remove_dir_all(&want.dir);
                why
            }
            Following::Published(version) => {
                (self.log)(&format!("machine {name}, the hub, runs devsandbox {version}: updating this one"));
                held.note(format!("updating to {version}"));
                let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
                match update::update(steps, &version, os, arch, &std::env::temp_dir()) {
                    Ok(()) => format!("updated to {version}: handing off"),
                    Err(e) => format!("update to {version} failed: {e:#}"),
                }
            }
        };
        (self.log)(&format!("machine {name}: {why}"));
        why
    }
}

/// How [`Dialer::answer`] ended.
enum Ended {
    Link(link::End),
    /// The machine is the hub, on another build: follow it.
    Follow(Following),
}

enum Following {
    /// A dev build pushed (or not, by the link's end) over `TRANSFER`.
    Dev(follow::Want, link::End),
    /// A published version to download.
    Published(String),
}

/// Why the link ended, with ssh's stderr tail.
fn end_why(end: link::End, duplex: &Duplex) -> String {
    let why = match end {
        link::End::Handshake(why) | link::End::Refused(why) => why,
        link::End::Dead => "no frame from the machine: link dead".into(),
        link::End::Closed => "link closed".into(),
    };
    let tail = duplex.peer.stderr_tail(Duration::from_millis(500));
    if tail.is_empty() { why } else { format!("{why} (ssh: {tail})") }
}

/// This host's member name: `member.toml`'s `name` at `member`, else
/// `hostname`'s (`fleet::member::Member::name_or`).
pub fn member_name(member: &std::path::Path, hostname: &str) -> Result<String> {
    super::member::load(member)?.unwrap_or_default().name_or(hostname).context("no member name")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use serde_json::{Value, json};

    use super::*;
    use crate::commands::fleet_link::{PeerOptions, run_peer};
    use crate::serve::client::{self, Conn};
    use crate::serve::daemon::tests::{fake_join, opts, scratch, spawn_daemon};
    use crate::serve::fleet::membership_api::test_support;
    use crate::serve::fleet::route;
    use crate::serve::daemon::{Exit, Options};
    use crate::serve::machines::ssh;
    use crate::serve::proto::Version;
    use crate::serve::snapshots::Join;

    pub(crate) fn connect(dir: &Path) -> Conn {
        let v = Version { semver: "0.6.0".into(), build: 1 };
        client::connect_with(dir, "test", &v, client::START_TIMEOUT, &|_, _| Ok(())).unwrap()
    }

    pub(crate) fn wait_for(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + within;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What [`peer_transport`] dialed: each far end's socket (shut it to
    /// lose the link as a dead ssh would) and the `--peer` it was given.
    #[derive(Default)]
    pub(crate) struct Dialed {
        pub ends: Mutex<Vec<UnixStream>>,
        pub peers: Mutex<Vec<String>>,
        pub count: AtomicUsize,
    }

    impl Dialed {
        /// Shut every far end: the links end.
        pub(crate) fn cut(&self) {
            for end in self.ends.lock().unwrap().drain(..) {
                let _ = end.shutdown(Shutdown::Both);
            }
        }
    }

    /// A transport whose far end is `fleet-link --peer <member>` run
    /// in-process (`commands::fleet_link::run_peer`) against the daemon in
    /// `k_dir`, a member taking no dev builds (`Fake` steps), proving with
    /// the identity next to `k_member` (its `member.toml`). Its error, as
    /// `main` would print it, goes to the stand-in ssh's stderr.
    pub(crate) fn peer_transport(k_dir: &Path, k_member: &Path) -> (Transport, Arc<Dialed>) {
        let dialed = Arc::new(Dialed::default());
        let (k_dir, seen, k_member) = (k_dir.to_path_buf(), Arc::clone(&dialed), k_member.to_path_buf());
        let transport = Transport::new(move |_, args| {
            let ["fleet-link", "--peer", peer] = args.iter().map(String::as_str).collect::<Vec<_>>()[..] else {
                anyhow::bail!("not a peer link: {args:?}");
            };
            seen.count.fetch_add(1, Ordering::SeqCst);
            seen.peers.lock().unwrap().push(peer.to_string());
            let (ours, theirs) = UnixStream::pair()?;
            let (mut err_w, err_r) = UnixStream::pair()?;
            seen.ends.lock().unwrap().push(theirs.try_clone()?);
            // Shut when it returns, as ssh's exit closes the pipe.
            let exited = theirs.try_clone()?;
            let (r, w) = (theirs.try_clone()?, theirs);
            let (daemon_dir, peer) = (k_dir.clone(), peer.to_string());
            let opts = PeerOptions {
                dir: k_dir.clone(),
                hello: update::hello_of(false, None),
                daemon: Arc::new(move || Ok(connect(&daemon_dir))),
                follow: Follow { steps: Arc::new(update::tests::Fake::default()), dev_builds: Arc::new(|| false) },
                reannounce: Duration::from_millis(20),
                push_wait: Duration::from_secs(5),
                member: k_member.clone(),
            };
            std::thread::spawn(move || {
                if let Err(e) = run_peer(r, w, &peer, opts) {
                    let _ = writeln!(err_w, "Error: {e:#}");
                }
                drop(err_w);
                let _ = exited.shutdown(Shutdown::Both);
            });
            ssh::test_support::with_stderr(ours, err_r)
        });
        (transport, dialed)
    }

    /// K's join: one instance `web-1` in the watched root.
    fn k_join() -> Join {
        Arc::new(|dir, view| {
            let mut list = fake_join()(dir, view);
            let mut row = crate::serve::snapshots::tests::row("web-1", true, None);
            row.root = Some(dir.display().to_string());
            list.snapshot.instances.push(row);
            list
        })
    }

    /// Two daemons: K (`base/k`, its own root `base/k-own` with one
    /// instance, its data in `base/k-data`) and L (`base/l`, member name
    /// `lap`, its data in `base/l-data`) whose registry has ssh machine
    /// `box` (K, its root K's own), reached through [`peer_transport`]. Both
    /// in one membership log (`lap` its authority, `box` its hub), K named
    /// `box` there.
    pub(crate) struct Pair {
        pub(crate) base: PathBuf,
        pub(crate) k_dir: PathBuf,
        pub(crate) l_dir: PathBuf,
        k_own: PathBuf,
        l_registry: PathBuf,
        pub(crate) dialed: Arc<Dialed>,
        daemons: Vec<std::thread::JoinHandle<Result<Exit>>>,
    }

    pub(crate) fn daemon_opts(base: &Path, name: &str) -> (Options, PathBuf) {
        let data = base.join(format!("{name}-data"));
        std::fs::create_dir_all(&data).unwrap();
        let mut o = opts(1, 60_000);
        o.member = Some(data.join("member.toml"));
        o.machines = Some(data.join("machines.toml"));
        o.home = Some(base.join(format!("{name}-home")));
        o.state = Some(data.join("state.toml"));
        (o, data)
    }

    impl Pair {
        fn new(tag: &str, k_entries: Vec<Entry>, l: impl FnOnce(&mut Options)) -> Pair {
            Pair::with(tag, k_entries, |_| {}, l)
        }

        /// [`Pair::new`], K's options set by `k` too.
        fn with(tag: &str, k_entries: Vec<Entry>, k: impl FnOnce(&mut Options), l: impl FnOnce(&mut Options)) -> Pair {
            Pair::build(tag, k_entries, k, l, |k_member, l_member| {
                test_support::fleet_of("box", &[("lap", l_member), ("box", k_member)]);
            })
        }

        /// [`Pair::with`], the membership logs written by `fleet` (given K's
        /// and L's `member.toml`) once both options are set.
        pub(crate) fn build(
            tag: &str,
            k_entries: Vec<Entry>,
            k: impl FnOnce(&mut Options),
            l: impl FnOnce(&mut Options),
            fleet: impl FnOnce(&Path, &Path),
        ) -> Pair {
            let base = scratch(tag);
            let (k_dir, l_dir) = (base.join("k"), base.join("l"));
            let k_own = base.join("k-own");
            std::fs::create_dir_all(&k_own).unwrap();
            let k_own = std::fs::canonicalize(k_own).unwrap();
            let (mut k_opts, k_data) = daemon_opts(&base, "k");
            std::fs::write(
                k_data.join("state.toml"),
                format!(
                    "[instance.web-1]\ninstance_id = \"id-w\"\nsandbox = \"web\"\ncontainer = \"devsandbox-web-1\"\nfolder = \"/w\"\n\
                     workspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                    k_own.to_str().unwrap()
                ),
            )
            .unwrap();
            crate::serve::machines::update(&k_data.join("machines.toml"), |e| e.extend(k_entries)).unwrap();
            k_opts.join = Some(k_join());
            k(&mut k_opts);
            let (mut l_opts, l_data) = daemon_opts(&base, "l");
            std::fs::write(l_data.join("member.toml"), "name = \"lap\"\n").unwrap();
            let l_registry = l_data.join("machines.toml");
            let entry = Entry { roots: vec![k_own.clone()], ..Entry::ssh("box", "me@box") };
            crate::serve::machines::update(&l_registry, |e| e.push(entry)).unwrap();
            let k_member = k_opts.member.clone().unwrap();
            let (transport, dialed) = peer_transport(&k_dir, &k_member);
            l_opts.link_transport = Some(transport);
            l(&mut l_opts);
            fleet(&k_member, l_opts.member.as_ref().unwrap());
            let k = spawn_daemon(&k_dir, k_opts);
            drop(connect(&k_dir));
            let l = spawn_daemon(&l_dir, l_opts);
            drop(connect(&l_dir));
            Pair { base, k_dir, l_dir, k_own, l_registry, dialed, daemons: vec![k, l] }
        }

        fn box_row(&self) -> Value {
            let rows = connect(&self.l_dir).call("machines.list", Value::Null).unwrap().result.unwrap();
            rows.as_array().unwrap().iter().find(|r| r["name"] == "box").cloned().unwrap()
        }

        /// The row `cond` matched, not a later read: box's link loop may
        /// retry in between (`Connecting` clears the error).
        pub(crate) fn wait_box(&self, what: &str, cond: impl Fn(&Value) -> bool) -> Value {
            let mut matched = None;
            wait_for(what, Duration::from_secs(10), || {
                let row = self.box_row();
                let hit = cond(&row);
                matched = hit.then_some(row);
                hit
            });
            matched.unwrap()
        }

        pub(crate) fn finish(self) {
            for (dir, daemon) in [&self.l_dir, &self.k_dir].into_iter().zip(self.daemons.into_iter().rev()) {
                assert!(connect(dir).call("shutdown", json!({})).unwrap().error.is_none());
                assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
            }
            self.dialed.cut();
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    pub(crate) fn connected(r: &Value) -> bool {
        r["state"] == "connected"
    }

    /// An ssh machine's link comes up with nobody wanting it, over
    /// `fleet-link --peer lap` there: `machines.list` says connected with
    /// its version, K lists `lap` as inbound, a quick call and a watch's
    /// rows go over `fleet-box.sock`; K's side dropping is redialed;
    /// `machines.hold` is gone.
    #[test]
    fn an_ssh_machine_is_held_over_its_peer_link() {
        let pair = Pair::new("dial-held", Vec::new(), |_| {});
        let row = pair.wait_box("box connected", connected);
        assert_eq!((&row["version"], &row["error"]), (&json!("0.6.0"), &Value::Null));
        assert_eq!(*pair.dialed.peers.lock().unwrap(), ["lap"]);
        let socket = endpoint::fleet_socket(&pair.l_dir, "box");
        assert!(socket.exists(), "{}", socket.display());
        let mut k = connect(&pair.k_dir);
        let inbound = || {
            let rows = connect(&pair.k_dir).call("machines.list", Value::Null).unwrap().result.unwrap();
            rows.as_array().unwrap().iter().find(|r| r["kind"] == "inbound").cloned()
        };
        wait_for("lap inbound on K", Duration::from_secs(5), || inbound().is_some_and(|r| r["name"] == "lap"));

        // A quick call and a watch's rows: K's answer, under K's trust for lap.
        let mut l = connect(&pair.l_dir);
        let r = l.call("instances.list", json!({"dir": pair.k_own, "machine": "box"})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let r = l.call("instances.list", json!({"dir": pair.base, "machine": "box"})).unwrap();
        assert_eq!(r.error.unwrap().code, "denied", "outside what K's trust table lets lap reach");
        let l_root = pair.base.join("l-root");
        std::fs::create_dir_all(&l_root).unwrap();
        l.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        l.call("instances.watch", json!({"dir": l_root, "machines": true})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let n = l.next_notification(left).unwrap().expect("no snapshot with box's rows");
            if n.method == "instances.snapshot" && n.params["instances"].as_array().is_some_and(|a| !a.is_empty()) {
                assert_eq!((&n.params["instances"][0]["name"], &n.params["instances"][0]["machine"]), (&json!("web-1"), &json!("box")));
                break;
            }
        }
        let r = connect(&pair.l_dir).call("machines.hold", json!({"name": "box"})).unwrap();
        assert_eq!(r.error.unwrap().code, "unknown-method");

        // K's side goes (its `fleet-link` exits): redialed.
        pair.dialed.cut();
        pair.wait_box("box redialed", |r| connected(r) && pair.dialed.count.load(Ordering::SeqCst) == 2);
        drop((k.call("machines.list", Value::Null), l));
        pair.finish();
    }

    /// K's `PTY` exec: the instance argv's first word run by `sh -c`.
    fn sh(t: &crate::serve::fleet::pty::Target) -> Vec<String> {
        let crate::serve::fleet::pty::Target::Instance { argv, .. } = t else { panic!("not an instance exec") };
        vec!["sh".into(), "-c".into(), argv[0].clone()]
    }

    /// The CLI's re-runs on ssh machine `box` go over its link, under K's
    /// trust for `lap` (K's own root admitted, nothing else): `--machine
    /// box ls` and `done box/web-1` as API calls (`cli::calls`), a piped
    /// `exec box/web-1 -- …` one `PTY` dial through `fleet-box.sock`
    /// ending in the command's exit code; a root K doesn't admit is K's
    /// refusal.
    #[test]
    fn cli_re_runs_on_an_ssh_machine_go_over_its_link() {
        use clap::Parser;

        use crate::serve::fleet::pty_client;
        use crate::serve::machines::cli::{self, calls};
        let pair = Pair::with(
            "dial-cli",
            Vec::new(),
            |k| {
                k.helper = Some(crate::serve::helper::HelperClient::new(crate::serve::helper::tests::Fake::default(), |_| {}));
                k.pty_exec = Some(sh);
            },
            |_| {},
        );
        pair.wait_box("box connected", connected);
        std::fs::write(pair.k_own.join(crate::config::CONFIG_FILE), "[sandbox.web]\nimage = \"node\"\n").unwrap();
        let mut l = connect(&pair.l_dir);
        let mut call = |m: &str, p: Value| l.call(m, p);

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = calls::per_root(&mut call, &calls::Use::Ls { json: false }, "box", &[pair.k_own.clone()], &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(err).unwrap(), format!("{}\n", cli::banner("box", &pair.k_own)));
        let line = crate::commands::ls::Line { name: "web".into(), source: "image node".into(), folder: None, services: None, extends: None };
        assert_eq!(String::from_utf8(out).unwrap(), crate::commands::ls::table(&[line]));
        let mut out = Vec::new();
        calls::named(&mut call, &calls::Use::Done(true), "box", "web-1", &pair.k_own, false, &mut out, &mut Vec::new()).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n", crate::commands::done::status_line("web-1", true, false)));
        let k_state = std::fs::read_to_string(pair.base.join("k-data/state.toml")).unwrap();
        assert!(k_state.contains("done"), "{k_state}");
        let e = calls::per_root(&mut call, &calls::Use::Ls { json: false }, "box", &[pair.base.clone()], &mut Vec::new(), &mut Vec::new());
        let e = e.unwrap_err().to_string();
        assert!(e.starts_with("machine `box`: ") && e.contains("isn't one of this member's roots"), "K's trust refuses what isn't its own: {e}");

        // A piped exec, as `cli::pipe_mode` makes it of the typed command.
        let typed = crate::Cli::try_parse_from(["devsandbox", "exec", "box/web-1", "--", "echo out; echo err >&2; exit 3"]).unwrap();
        let mode = cli::pipe_mode(typed.command.as_ref().unwrap(), false).unwrap().unwrap();
        let request = pty_client::Request { root: pair.k_own.clone(), instance: "web-1".into(), mode };
        let stream = link::connect(&endpoint::fleet_socket(&pair.l_dir, "box"), channel::PTY).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let term = pty_client::Terminal { input: None, output: &mut out, errors: &mut err, size: &|| None };
        let end = pty_client::attach(stream, &pty_client::header(&request), term);
        assert_eq!((pty_client::exit_of(&end, false).0, out, err), (3, b"out\n".to_vec(), b"err\n".to_vec()));

        // A forward's bridge: one `TUNNEL` stream through
        // `fleet-box.sock`, answered by K's daemon under its trust for
        // lap.
        use crate::commands::tunnel::{self, Request, Via};
        let bridge = |root: &Path| {
            let request = Request { root: root.to_path_buf(), instance: Some("web-1".into()), service: None, port: 3000 };
            let via = Via::Fleet { dir: pair.l_dir.clone(), request };
            let open = tunnel::remote_opener("box".into(), Box::new(move || Ok(via.clone())));
            open(&std::sync::atomic::AtomicBool::new(false)).err()
        };
        let e = bridge(&pair.k_own).unwrap();
        assert_eq!(e, "instance `web-1` is not running (devsandbox start web-1)", "K's own answer: found, not running");
        let e = bridge(&pair.base).unwrap();
        assert!(e.contains("isn't one of this member's roots"), "K's trust refuses what isn't its own: {e}");
        drop(l);
        pair.finish();
    }

    /// K's `AGENT` streams are answered with this host's agent while L
    /// grants it to K's key (`machines.set {agent}`, L's `member.toml`): K
    /// binds `agent-link-lap.sock` then, and it leads to L's agent;
    /// withdrawn, it goes. The link stays up throughout.
    #[test]
    fn the_machines_agent_streams_follow_the_entrys_agent() {
        let base = scratch("dial-agent-sock");
        std::fs::create_dir_all(&base).unwrap();
        let agent_path = base.join("agent.sock");
        let listener = UnixListener::bind(&agent_path).unwrap();
        std::thread::spawn(move || {
            for mut c in listener.incoming().flatten() {
                let mut ping = [0u8; 4];
                if c.read_exact(&mut ping).is_ok() {
                    let _ = c.write_all(b"pong");
                }
            }
        });
        let pair = Pair::new("dial-agent", Vec::new(), |o| {
            let path = agent_path.clone();
            o.host_agent = Some(Arc::new(move || Some(path.clone())));
        });
        pair.wait_box("box connected", connected);
        let socket = endpoint::link_agent_socket(&pair.k_dir, "lap");
        std::thread::sleep(crate::serve::fleet::agent::PROBE + Duration::from_millis(500));
        assert!(!socket.exists(), "agent off: not served");

        let set = |on: bool| {
            let r = connect(&pair.l_dir).call("machines.set", json!({"name": "box", "agent": on})).unwrap();
            assert!(r.error.is_none(), "{r:?}");
        };
        set(true);
        wait_for("K binds the agent socket", Duration::from_secs(10), || socket.exists());
        let mut c = UnixStream::connect(&socket).unwrap();
        c.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        c.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"pong");
        assert_eq!(endpoint::agent_sockets(&pair.k_dir), [socket.clone()], "no agent-route-lap.sock: lap dials K");
        set(false);
        wait_for("K unbinds it", Duration::from_secs(10), || !socket.exists());
        assert_eq!((pair.dialed.count.load(Ordering::SeqCst), connected(&pair.box_row())), (1, true), "the same link");
        assert!(pair.l_registry.exists());
        pair.finish();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A verb on `conn`: its answer, its streamed stdout and stderr.
    fn verb(conn: &mut Conn, method: &str, params: Value) -> (crate::serve::proto::Response, String, String) {
        let (mut out, mut err) = (String::new(), String::new());
        let streamed = conn.call_streaming(method, params, Duration::from_millis(50), &mut |n| {
            if let Some(n) = n.filter(|n| n.method == "output") {
                let data = n.params["data"].as_str().unwrap_or_default();
                if n.params["stream"] == "stdout" { out.push_str(data) } else { err.push_str(data) }
            }
            true
        });
        let Ok(client::Streamed::Answer(r)) = streamed else { panic!("{method}: no answer") };
        (r, out, err)
    }

    /// `rows`' entry named `name`, if any.
    pub(crate) fn named(rows: &Value, name: &str) -> Option<Value> {
        rows.as_array()?.iter().find(|r| r["name"] == name).cloned()
    }

    /// From K's side, the dialed host, L is a machine (kind `inbound`) like
    /// any other: `machines.list` says connected with its link's version
    /// and its roots (its fleet root for K among them), `machines.get`
    /// answers it, a quick call and a forwarded verb run on L under L's
    /// trust for K (`box` there), K's federated snapshot has L's rows; its
    /// `unlink` is refused here, an agent grant by its key written. L dropping its entry for K takes
    /// the machine and its rows away on K.
    #[test]
    fn the_dialer_is_an_inbound_machine_on_the_dialed_host() {
        let (l_runner, l_verbs) = crate::serve::verbs::tests::script_runner("dial-inbound-l", r#"echo "lap $*""#);
        let pair = Pair::new("dial-inbound", Vec::new(), |l| {
            l.verbs = Some(l_runner);
            l.peer_env = Some(std::collections::BTreeMap::from([("PATH".into(), std::env::var("PATH").unwrap_or_default())]));
            // L's join: one instance in the watched root.
            l.join = Some(Arc::new(|dir, view| {
                let mut list = fake_join()(dir, view);
                let mut row = crate::serve::snapshots::tests::row("lap-1", true, None);
                row.root = Some(dir.display().to_string());
                list.snapshot.instances.push(row);
                list
            }));
        });
        pair.wait_box("box connected", connected);
        // L's fleet root for K: `fleet/<its name for K>/`.
        let fleet_root = std::fs::canonicalize(pair.base.join("l-data/fleet/box")).unwrap();
        let k_rows = || connect(&pair.k_dir).call("machines.list", Value::Null).unwrap().result.unwrap();
        wait_for("lap connected on K, its roots listed", Duration::from_secs(10), || {
            named(&k_rows(), "lap").is_some_and(|r| connected(&r) && r["roots"].as_array().is_some_and(|a| !a.is_empty()))
        });
        let lap = named(&k_rows(), "lap").unwrap();
        assert_eq!((&lap["kind"], &lap["target"], &lap["version"], &lap["agent"]), (&json!("inbound"), &Value::Null, &json!("0.6.0"), &json!(false)));
        assert_eq!(lap["roots"], json!([fleet_root]), "L's own roots (none) and its fleet root for K");
        crate::serve::dts::assert_matches("MachineRow", &lap);
        let mut k = connect(&pair.k_dir);
        let info = k.call("machines.get", json!({"name": "lap"})).unwrap().result.unwrap();
        assert_eq!(info, json!({"name": "lap", "kind": "inbound", "target": null, "command": null, "roots": [fleet_root], "agent": false}));
        let e = k.call("machines.unlink", json!({"name": "lap"})).unwrap().error.unwrap();
        assert!(e.code == "denied" && e.message.contains("links to this host from its end"), "{e:?}");
        // An agent grant by lap's key (fleet plan decision 67), in K's member.toml.
        let r = k.call("machines.set", json!({"name": "lap", "agent": true})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let k_member = pair.base.join("k-data/member.toml");
        let lap_key = crate::serve::fleet::identity::Signer::public_key(&test_support::key_of(&pair.base.join("l-data/member.toml")));
        let grants = crate::serve::fleet::member::load(&k_member).unwrap().unwrap().agent;
        assert_eq!(grants, [crate::serve::fleet::member::Grant { name: "lap".into(), key: lap_key }]);
        assert_eq!(k.call("machines.get", json!({"name": "lap"})).unwrap().result.unwrap()["agent"], true);
        wait_for("lap's row shows the grant", Duration::from_secs(10), || named(&k_rows(), "lap").is_some_and(|r| r["agent"] == true));
        let r = k.call("machines.set", json!({"name": "lap", "agent": false})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        assert!(crate::serve::fleet::member::load(&k_member).unwrap().unwrap().agent.is_empty(), "withdrawn");

        // A quick call: L's answer, within L's trust for K.
        let r = k.call("instances.list", json!({"dir": fleet_root, "machine": "lap"})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let r = k.call("instances.list", json!({"dir": pair.base, "machine": "lap"})).unwrap();
        assert_eq!(r.error.unwrap().code, "denied", "outside what L's trust table lets K reach");
        // A forwarded verb runs on L, in L's fleet root for K.
        let stop = |root: &Path| json!({"root": root, "env": {}, "instance": "lap-1", "machine": "lap"});
        let (r, out, _) = verb(&mut k, "stop", stop(&fleet_root));
        assert!(r.error.is_none(), "{r:?}");
        assert_eq!(out, format!("lap --in-process -C {} stop -- lap-1\n", fleet_root.display()));
        let (r, ..) = verb(&mut k, "stop", stop(&pair.k_own));
        assert_eq!(r.error.unwrap().code, "denied", "L's trust: K's own root isn't K's to reach on L");

        // K's federated snapshot: L's rows, tagged `lap`, and the machine.
        k.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        k.call("instances.watch", json!({"dir": pair.k_own, "machines": true})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let n = k.next_notification(left).unwrap().expect("no snapshot with lap's rows");
            let rows = n.params["instances"].as_array().cloned().unwrap_or_default();
            if n.method == "instances.snapshot" && rows.iter().any(|r| r["machine"] == "lap") {
                let lap_row = rows.iter().find(|r| r["machine"] == "lap").unwrap();
                assert_eq!(lap_row["name"], "lap-1");
                assert_eq!(named(&n.params["machines"], "lap").unwrap()["kind"], "inbound");
                break;
            }
        }

        // L drops its entry for K: the link goes, and with it the machine
        // and its rows on K.
        assert!(connect(&pair.l_dir).call("machines.unlink", json!({"name": "box"})).unwrap().error.is_none());
        wait_for("lap gone on K", Duration::from_secs(10), || named(&k_rows(), "lap").is_none());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let n = k.next_notification(left).unwrap().expect("no snapshot without lap");
            if n.method == "instances.snapshot" && named(&n.params["machines"], "lap").is_none() {
                assert!(!n.params["instances"].as_array().unwrap().iter().any(|r| r["machine"] == "lap"), "its rows went with it");
                break;
            }
        }
        let e = connect(&pair.k_dir).call("instances.list", json!({"dir": fleet_root, "machine": "lap"})).unwrap().error.unwrap();
        assert_eq!(e.code, "not-found");
        drop(k);
        pair.finish();
        let _ = std::fs::remove_dir_all(&l_verbs);
    }

    /// A `run` on K (the dialed host) is placed on L, the inbound machine
    /// whose fresh `capacity.get` has room: the config root's tar and the
    /// branch's bundle land in L's fleet root for K (as L's `roots.list`
    /// flags it), and the run is forwarded there.
    #[test]
    fn a_run_is_placed_on_an_inbound_machine() {
        use crate::serve::capacity::Capacity;
        use crate::serve::fleet::code::repo::tests::{git, origin_and_checkout};
        const G: u64 = 1 << 30;

        let (l_runner, l_verbs) =
            crate::serve::verbs::tests::script_runner("dial-place-l", r#"echo "lap $*"; cat "$3/dockerfiles/web.Dockerfile" "$3/../web/F""#);
        let (k_runner, k_verbs) = crate::serve::verbs::tests::script_runner("dial-place-k", r#"echo "local $*""#);
        let path = std::env::var("PATH").unwrap_or_default();
        let src = scratch("dial-place-src");
        std::fs::create_dir_all(&src).unwrap();
        origin_and_checkout(&src, "web");
        let cfg = src.join("cfg");
        std::fs::create_dir_all(cfg.join("dockerfiles")).unwrap();
        std::fs::write(cfg.join(crate::config::CONFIG_FILE), "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\n").unwrap();
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM x\n").unwrap();
        let cfg = std::fs::canonicalize(&cfg).unwrap();
        let pair = Pair::with(
            "dial-place",
            Vec::new(),
            |k| {
                // Placement runs on the hub.
                let hub = crate::serve::fleet::member::Member { hub: "self".into(), ..Default::default() };
                crate::serve::fleet::member::write_for_test(k.member.as_ref().unwrap(), &hub);
                k.verbs = Some(k_runner);
                k.repos = Some(src.join("k-repos"));
            },
            |l| {
                l.verbs = Some(l_runner);
                l.peer_env = Some(std::collections::BTreeMap::from([("PATH".into(), path.clone())]));
                let roomy = Capacity {
                    cpus: Some(8),
                    mem_total: Some(32 * G),
                    mem_available: Some(28 * G),
                    disk_free: Some(400 * G),
                    disk_total: Some(500 * G),
                    ..Capacity::default()
                };
                l.local_capacity = Some(Arc::new(move || roomy.clone()));
            },
        );
        pair.wait_box("box connected", connected);
        wait_for("lap connected on K", Duration::from_secs(10), || {
            let rows = connect(&pair.k_dir).call("machines.list", Value::Null).unwrap().result.unwrap();
            named(&rows, "lap").is_some_and(|r| connected(&r))
        });
        let fleet_root = std::fs::canonicalize(pair.base.join("l-data/fleet/box")).unwrap();
        let mut k = connect(&pair.k_dir);
        let (r, out, err) = verb(&mut k, "run", json!({"root": cfg, "env": {"PATH": path}, "sandbox": "web", "branch": "feature", "place": true}));
        assert!(r.error.is_none(), "{r:?} {err}");
        assert_eq!(r.result.as_ref().unwrap()["machine"], "lap", "{r:?}");
        assert!(err.starts_with("placed on lap ("), "{err}");
        let roots: Vec<_> =
            std::fs::read_dir(fleet_root.join("roots")).unwrap().flatten().map(|e| e.file_name()).filter(|n| n != ".in").collect();
        assert_eq!(roots.len(), 1, "{roots:?}");
        let remote = fleet_root.join("roots").join(&roots[0]).join("cfg");
        assert_eq!(out, format!("lap --in-process -C {} run --branch=feature -- web\nFROM x\nfeature\n", remote.display()));
        assert_eq!(git(&remote.join("../web"), &["symbolic-ref", "--short", "HEAD"]), "feature");
        drop(k);
        pair.finish();
        for dir in [src, l_verbs, k_verbs] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A `run --place` on member L (`lap`, hub `box`) is placed by the hub K
    /// (`fleet.place`): L is over a cap, K has room, so L moves the code
    /// into its fleet root on K (as K's `roots.list` flags it) and forwards
    /// the run there, the hub's lines first. Without `place` it runs on L,
    /// whatever the hub would say.
    #[test]
    fn a_members_run_is_placed_by_the_hub() {
        use crate::serve::capacity::{Capacity, Caps};
        use crate::serve::fleet::code::repo::tests::{git, origin_and_checkout};
        use crate::serve::fleet::member::{Member, write_for_test};
        const G: u64 = 1 << 30;

        let (k_runner, k_verbs) =
            crate::serve::verbs::tests::script_runner("dial-hub-place-k", r#"echo "box $*"; cat "$3/dockerfiles/web.Dockerfile" "$3/../web/F""#);
        let (l_runner, l_verbs) = crate::serve::verbs::tests::script_runner("dial-hub-place-l", r#"echo "local $*""#);
        let path = std::env::var("PATH").unwrap_or_default();
        let src = scratch("dial-hub-place-src");
        std::fs::create_dir_all(&src).unwrap();
        origin_and_checkout(&src, "web");
        let cfg = src.join("cfg");
        std::fs::create_dir_all(cfg.join("dockerfiles")).unwrap();
        std::fs::write(cfg.join(crate::config::CONFIG_FILE), "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\n").unwrap();
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM x\n").unwrap();
        let cfg = std::fs::canonicalize(&cfg).unwrap();
        let capacity = |avail: u64, running: usize, caps: Option<Caps>| Capacity {
            cpus: Some(8),
            mem_total: Some(32 * G),
            mem_available: Some(avail * G),
            disk_free: Some(400 * G),
            disk_total: Some(500 * G),
            running,
            caps,
        };
        let roomy = capacity(28, 0, None);
        let busy = capacity(4, 3, Some(Caps { instances: Some(1), ..Caps::default() }));
        let pair = Pair::with(
            "dial-hub-place",
            Vec::new(),
            |k| {
                write_for_test(k.member.as_ref().unwrap(), &Member { name: "box".into(), hub: "self".into(), ..Default::default() });
                k.verbs = Some(k_runner);
                k.peer_env = Some(std::collections::BTreeMap::from([("PATH".into(), path.clone())]));
                k.local_capacity = Some(Arc::new(move || roomy.clone()));
            },
            |l| {
                write_for_test(l.member.as_ref().unwrap(), &Member { name: "lap".into(), hub: "box".into(), ..Default::default() });
                l.verbs = Some(l_runner);
                l.repos = Some(src.join("l-repos"));
                l.local_capacity = Some(Arc::new(move || busy.clone()));
            },
        );
        pair.wait_box("box connected", connected);
        wait_for("lap connected on K", Duration::from_secs(10), || lap_on_k(&pair).is_some_and(|r| connected(&r)));
        let fleet_root = std::fs::canonicalize(pair.base.join("k-data/fleet/lap")).unwrap();
        let mut l = connect(&pair.l_dir);
        let (r, out, err) = verb(&mut l, "run", json!({"root": cfg, "env": {"PATH": path}, "sandbox": "web", "branch": "feature"}));
        assert!(r.error.is_none(), "{r:?} {err}");
        assert_eq!(r.result.as_ref().unwrap().get("machine"), None, "{r:?}");
        assert_eq!((out, err.as_str()), (format!("local --in-process -C {} run --branch=feature -- web\n", cfg.display()), ""));
        assert!(!fleet_root.join("roots").exists() || std::fs::read_dir(fleet_root.join("roots")).unwrap().flatten().all(|e| e.file_name() == ".in"));
        let (r, out, err) = verb(&mut l, "run", json!({"root": cfg, "env": {"PATH": path}, "sandbox": "web", "branch": "feature", "place": true}));
        assert!(r.error.is_none(), "{r:?} {err}");
        assert_eq!(r.result.as_ref().unwrap()["machine"], "box", "{r:?}");
        assert!(err.starts_with("placed on box ("), "{err}");
        let roots: Vec<_> =
            std::fs::read_dir(fleet_root.join("roots")).unwrap().flatten().map(|e| e.file_name()).filter(|n| n != ".in").collect();
        assert_eq!(roots.len(), 1, "{roots:?}");
        let remote = fleet_root.join("roots").join(&roots[0]).join("cfg");
        assert_eq!(out, format!("box --in-process -C {} run --branch=feature -- web\nFROM x\nfeature\n", remote.display()));
        assert_eq!(git(&remote.join("../web"), &["symbolic-ref", "--short", "HEAD"]), "feature");
        drop(l);
        pair.finish();
        for dir in [src, l_verbs, k_verbs] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Member L's hub `box` can't be reached (its ssh fails): a `run
    /// --place` on L runs on L, its output starting with a note naming the
    /// hub.
    #[test]
    fn a_members_run_stays_here_when_its_hub_is_unreachable() {
        use crate::serve::fleet::member::{Member, write_for_test};
        let (l_runner, l_verbs) = crate::serve::verbs::tests::script_runner("dial-hub-down-l", r#"echo "local $*""#);
        let pair = Pair::with(
            "dial-hub-down",
            Vec::new(),
            |k| write_for_test(k.member.as_ref().unwrap(), &Member { name: "box".into(), hub: "self".into(), ..Default::default() }),
            |l| {
                write_for_test(l.member.as_ref().unwrap(), &Member { name: "lap".into(), hub: "box".into(), ..Default::default() });
                l.verbs = Some(l_runner);
                l.link_transport = Some(Transport::new(|_, _| anyhow::bail!("ssh: connect to host box port 22: Connection refused")));
            },
        );
        pair.wait_box("box unreachable", |r| r["state"] == "unreachable");
        let cfg = pair.base.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let (r, out, err) = verb(&mut connect(&pair.l_dir), "run", json!({"root": cfg, "env": {}, "sandbox": "web", "place": true}));
        assert!(r.error.is_none(), "{r:?} {err}");
        assert_eq!(r.result.as_ref().unwrap().get("machine"), None, "{r:?}");
        assert!(err.starts_with("note: the hub `box` is unreachable (") && err.contains("): running here\n"), "{err}");
        assert_eq!(out, format!("local --in-process -C {} run -- web\n", cfg.display()));
        pair.finish();
        let _ = std::fs::remove_dir_all(&l_verbs);
    }

    /// L's membership log is behind K's, whose last entry makes `lap` the
    /// hub: the link's proof carries the entry to L, L's daemon adopts it
    /// (its `member.toml` hub follows: this host), the link ends since L's
    /// `Hello` said no hub, and the redial comes up.
    #[test]
    fn the_end_behind_takes_the_newer_entries_and_follows_their_hub() {
        use crate::serve::fleet::member::Hub;
        use crate::serve::fleet::membership::Op;
        let newer = Arc::new(Mutex::new(None));
        let kept = Arc::clone(&newer);
        let pair = Pair::build("dial-behind", Vec::new(), |_| {}, |_| {}, move |k_member, l_member| {
            let mut ahead = test_support::fleet_of("box", &[("lap", l_member), ("box", k_member)]);
            ahead.log.append(Op::Hub { name: "lap".into() }, "lap", &test_support::key_of(l_member)).unwrap();
            test_support::write(k_member, &ahead);
            *kept.lock().unwrap() = Some(ahead);
        });
        let ahead = newer.lock().unwrap().take().unwrap();
        pair.wait_box("box connected after a redial", |r| connected(r) && pair.dialed.count.load(Ordering::SeqCst) >= 2);
        let l_member = pair.base.join("l-data/member.toml");
        assert_eq!(test_support::read(&l_member).unwrap(), ahead, "L took K's entries");
        assert_eq!(crate::serve::fleet::member::load(&l_member).unwrap().unwrap().hub(), Hub::This, "L's hub follows its log");
        assert_eq!(connect(&pair.l_dir).call("member.get", json!({})).unwrap().result.unwrap()["hub"], "self");
        pair.finish();
    }

    /// K has no membership log (a fleet made before keys): its `fleet-link`
    /// refuses to link, saying how to make the fleet again; box's error
    /// here.
    #[test]
    fn a_machine_with_no_membership_log_is_refused() {
        let pair = Pair::build("dial-no-log", Vec::new(), |_| {}, |_| {}, |k_member, l_member| {
            test_support::fleet_of("box", &[("lap", l_member), ("box", k_member)]);
            std::fs::remove_file(crate::serve::fleet::membership::path(k_member)).unwrap();
        });
        let row = pair.wait_box("box refused", |r| r["error"].as_str().is_some_and(|e| e.contains("membership log")));
        let e = row["error"].as_str().unwrap();
        assert!(e.contains("in no fleet's membership log") && e.contains("machine add"), "{e}");
        assert_ne!(row["state"], "connected");
        pair.finish();
    }

    /// K already has a machine named `lap` (the two hosts added each
    /// other): its refusal is box's error here.
    #[test]
    fn a_clash_refused_there_is_the_machines_error() {
        let pair = Pair::new("dial-clash", vec![Entry::ssh("lap", "me@lap")], |_| {});
        let row = pair.wait_box("box refused", |r| {
            r["state"] == "unreachable" && r["error"].as_str().is_some_and(|e| e.contains("already a machine here"))
        });
        assert!(row["error"].as_str().unwrap().contains("refused: `lap` is already a machine here (me@lap)"), "{row}");
        pair.finish();
    }

    /// A hub machine on another published build: this host refuses the
    /// link, installs that version and hands off, and says so as box's
    /// error.
    #[test]
    fn this_host_follows_a_published_hub_machine() {
        let base = scratch("dial-follow");
        std::fs::create_dir_all(&base).unwrap();
        let registry = base.join("machines.toml");
        crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("box", "me@box"))).unwrap();
        let box_member = base.join("box-data/member.toml");
        std::fs::create_dir_all(box_member.parent().unwrap()).unwrap();
        test_support::fleet_of("box", &[("lap", &base.join("member.toml")), ("box", &box_member)]);
        let hello: &'static str = Box::leak(format!("9.9.9 abc published {} hub", update::PLATFORM).into_boxed_str());
        let transport = Transport::new(move |_, _| {
            let (ours, theirs) = UnixStream::pair()?;
            let (r, w) = (theirs.try_clone()?, theirs);
            let identity = Identity::load(&box_member, "box")?;
            std::thread::spawn(move || {
                let spec = link::Spec { hello, ..LINK };
                let auth = proof::Auth { identity, expect: "lap".into(), log: |_| {} };
                link::opener_with(&spec, r, w, link::Endpoint { auth: Some(auth), ..link::Endpoint::default() }, |_| Ok(()))
            });
            Ok(Duplex::socket(ours))
        });
        let fake = Arc::new(update::tests::Fake::reporting("9.9.9"));
        let mut o = opts(1, 60_000);
        o.machines = Some(registry);
        o.member = Some(base.join("member.toml"));
        o.link_transport = Some(transport);
        o.update_steps = Some(fake.clone());
        let dir = base.join("l");
        let daemon = spawn_daemon(&dir, o);
        let row = || {
            let rows = connect(&dir).call("machines.list", Value::Null).unwrap().result.unwrap();
            rows[0].clone()
        };
        wait_for("box says it updated", Duration::from_secs(10), || row()["error"] == "updated to 9.9.9: handing off");
        assert!(fake.calls().first().is_some_and(|c| c.starts_with("fetch https://") && c.contains("v9.9.9")), "{:?}", fake.calls());
        assert!(fake.calls().iter().any(|c| c == "handoff "), "{:?}", fake.calls());
        assert!(connect(&dir).call("shutdown", json!({})).unwrap().error.is_none());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The next `instances.snapshot` on `conn` that `pick` takes, within
    /// 10 s.
    fn snapshot_where(conn: &mut Conn, what: &str, pick: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let n = conn.next_notification(left).unwrap().unwrap_or_else(|| panic!("no snapshot: {what}"));
            if n.method == "instances.snapshot" && pick(&n.params) {
                return n.params;
            }
        }
    }

    fn tagged(params: &Value, machine: &str) -> Vec<String> {
        let rows = params["instances"].as_array().cloned().unwrap_or_default();
        rows.iter().filter(|r| r["machine"] == machine).map(|r| r["name"].as_str().unwrap_or_default().to_string()).collect()
    }

    /// A fleet: K (member `box`) is the hub, with a third machine `x` it
    /// knows rows of (restored from its `remotes.json`; x itself is never
    /// reached); L (`lap`) is a member whose hub is `box`. L's federated
    /// snapshot shows K's rows and, through K, x's (`via: box`, kind
    /// `routed`), never its own as K sees them; x is a routed machine on L
    /// (`machines.list`/`get`), acted on over a route (none reaches it: the
    /// route's error), its agent refused. K serves a federated watch only
    /// as the hub.
    #[test]
    fn a_member_sees_the_fleet_through_its_hub() {
        let base_remotes = scratch("dial-via-files");
        std::fs::create_dir_all(&base_remotes).unwrap();
        let (k_remotes, l_remotes) = (base_remotes.join("k-remotes.json"), base_remotes.join("l-remotes.json"));
        let x_root = PathBuf::from("/x/root");
        let x_rows = json!({"dir": x_root, "instances": [{"name": "x-1", "root": x_root}], "sandboxes": [], "services": []});
        let x = crate::serve::snapshots::Machine {
            name: "x".into(),
            rows: [(x_root.clone(), Arc::new(x_rows))].into(),
            seen: Some(1_761_566_400),
            ..Default::default()
        };
        crate::serve::machines::saved::save(&k_remotes, &crate::serve::machines::saved::of(&[x], 0)).unwrap();
        let pair = Pair::with(
            "dial-via",
            vec![Entry { roots: vec![x_root.clone()], ..Entry::ssh("x", "me@x") }],
            |k| {
                let hub = crate::serve::fleet::member::Member { name: "box".into(), hub: "self".into(), ..Default::default() };
                crate::serve::fleet::member::write_for_test(k.member.as_ref().unwrap(), &hub);
                k.remotes = Some(k_remotes.clone());
            },
            |l| {
                let member = crate::serve::fleet::member::Member { name: "lap".into(), hub: "box".into(), ..Default::default() };
                crate::serve::fleet::member::write_for_test(l.member.as_ref().unwrap(), &member);
                l.remotes = Some(l_remotes.clone());
                // L's join: one instance in the watched root, which K sees.
                l.join = Some(Arc::new(|dir, view| {
                    let mut list = fake_join()(dir, view);
                    let mut row = crate::serve::snapshots::tests::row("lap-1", true, None);
                    row.root = Some(dir.display().to_string());
                    list.snapshot.instances.push(row);
                    list
                }));
            },
        );
        pair.wait_box("box connected", connected);

        // L's dashboard: K's rows, x's through K, nothing of lap's own as K has them.
        let l_root = pair.base.join("l-root");
        std::fs::create_dir_all(&l_root).unwrap();
        let mut l = connect(&pair.l_dir);
        l.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        l.call("instances.watch", json!({"dir": l_root, "machines": true})).unwrap();
        let seen = snapshot_where(&mut l, "x's rows through box", |p| !tagged(p, "x").is_empty() && !tagged(p, "box").is_empty());
        assert_eq!(tagged(&seen, "x"), ["x-1"]);
        assert_eq!(tagged(&seen, "box"), ["web-1"]);
        let x = named(&seen["machines"], "x").unwrap();
        assert_eq!((&x["via"], &x["target"], &x["state"]), (&json!("box"), &Value::Null, &json!("unreachable")));
        assert_eq!(named(&seen["machines"], "box").unwrap()["via"], Value::Null);
        crate::serve::dts::assert_matches("SnapshotMachine", &x);

        // K has lap's rows by now (its own federated watch shows them),
        // yet L's next pushes still carry none of them.
        let mut k = connect(&pair.k_dir);
        k.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        k.call("instances.watch", json!({"dir": pair.k_own, "machines": true})).unwrap();
        snapshot_where(&mut k, "lap's rows on K", |p| !tagged(p, "lap").is_empty());
        // A re-watch pushes the merge as it is now.
        l.call("instances.watch", json!({"dir": l_root, "machines": true})).unwrap();
        let until = Instant::now() + Duration::from_millis(1500);
        let mut pushes = 0;
        while Instant::now() < until {
            let Some(n) = l.next_notification(Duration::from_millis(200)).unwrap() else { continue };
            if n.method == "instances.snapshot" {
                pushes += 1;
                assert!(tagged(&n.params, "lap").is_empty(), "lap's own rows came back: {}", n.params);
                assert!(named(&n.params["machines"], "lap").is_none(), "{}", n.params);
                assert_eq!(tagged(&n.params, "x"), ["x-1"]);
            }
        }
        assert!(pushes > 0, "no push after the re-watch");
        // K itself leaves lap out (not only L's guard): a federated watch
        // on lap's own API stream to K.
        let api_to = |socket: &Path, name: &str| {
            let stream = link::connect(socket, channel::API).unwrap();
            let duplex = crate::serve::machines::ssh::Duplex::socket(stream);
            crate::serve::machines::link::handshake(name, duplex, Duration::from_secs(10), crate::serve::machines::link::Over::Link).unwrap()
        };
        let mut lap_on_k = api_to(&endpoint::fleet_socket(&pair.l_dir, "box"), "box");
        let rows = lap_on_k.conn.call("roots.list", json!({})).unwrap().result.unwrap();
        let k_fleet = rows.as_array().unwrap().iter().find(|r| r["fleet"] == true).unwrap()["root"].clone();
        lap_on_k.conn.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        let r = lap_on_k.conn.call("instances.watch", json!({"dir": k_fleet, "machines": true})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let pushed = snapshot_where(&mut lap_on_k.conn, "K's push to lap", |_| true);
        assert_eq!(tagged(&pushed, "x"), ["x-1"], "{pushed}");
        assert!(tagged(&pushed, "lap").is_empty() && named(&pushed["machines"], "lap").is_none(), "{pushed}");

        // Routed (fleet plan decision 60): x is a machine of its own here,
        // kind `routed`, acted on over a route, which x (no daemon, not in
        // the log, so in no graph) never has.
        let rows = connect(&pair.l_dir).call("machines.list", Value::Null).unwrap().result.unwrap();
        let row = named(&rows, "x").unwrap();
        assert_eq!((&row["kind"], &row["via"], &row["state"], &row["target"]), (&json!("routed"), &json!("box"), &json!("unreachable"), &Value::Null));
        assert_eq!(row["roots"], json!([]), "x's view has no sandboxes rows");
        crate::serve::dts::assert_matches("MachineRow", &row);
        assert_eq!(x["kind"], "routed");
        let info = connect(&pair.l_dir).call("machines.get", json!({"name": "x"})).unwrap().result.unwrap();
        assert_eq!(info, json!({"name": "x", "kind": "routed", "target": null, "command": null, "roots": [], "agent": false}));
        // A machine, but not in the log: no key to grant agent by.
        let e = connect(&pair.l_dir).call("machines.set", json!({"name": "x", "agent": true})).unwrap().error.unwrap();
        assert!(e.code == "failed" && e.message.contains("no key to grant agent by"), "{e:?}");
        let r = connect(&pair.l_dir).call("machines.set", json!({"name": "x", "agent": false})).unwrap();
        assert!(r.error.is_none(), "nothing to withdraw: {r:?}");
        let e = connect(&pair.l_dir).call("instances.list", json!({"dir": x_root, "machine": "x"})).unwrap().error.unwrap();
        assert!(e.code == "failed" && e.message.starts_with("x: unreachable: no link to `x` is up"), "{e:?}");
        let add = json!({"dir": x_root, "instance": "x-1", "spec": "3000", "machine": "x"});
        let e = connect(&pair.l_dir).call("forwards.add", add).unwrap().error.unwrap();
        assert_eq!(e.message, "this daemon runs no forwards", "served here, not refused");
        let (r, ..) = verb(&mut connect(&pair.l_dir), "stop", json!({"root": x_root, "env": {}, "instance": "x-1", "machine": "x"}));
        let e = r.error.unwrap();
        assert!(e.code == "failed" && e.message.starts_with("x: unreachable: no link to `x` is up"), "{e:?}");
        // A name nobody sees is still unknown.
        assert_eq!(connect(&pair.l_dir).call("machines.get", json!({"name": "nope"})).unwrap().error.unwrap().code, "not-found");

        // Only the hub federates for a peer: L refuses K's federated watch.
        let fleet_root = std::fs::canonicalize(pair.base.join("l-data/fleet/box")).unwrap();
        let mut k_on_l = api_to(&endpoint::fleet_socket(&pair.k_dir, "lap"), "lap");
        k_on_l.conn.call("subscribe", json!({"topics": ["instances"]})).unwrap();
        let e = k_on_l.conn.call("instances.watch", json!({"dir": fleet_root, "machines": true})).unwrap().error.unwrap();
        assert_eq!(e.code, "denied", "{e:?}");
        assert!(!l_remotes.exists(), "a member saves no remotes");
        drop((l, k, k_on_l, lap_on_k));
        pair.finish();
        let _ = std::fs::remove_dir_all(&base_remotes);
    }

    /// A [`Pair`] in one fleet: K (`box`, with a registry entry `x` it never
    /// reaches) and L (`lap`), `k_hub` / `l_hub` their `hub`s, the log's hub
    /// the one they name (`lap` when L says `self`, else `box`).
    fn fleet_pair(tag: &str, k_hub: &str, l_hub: &str) -> Pair {
        use crate::serve::fleet::member::{Member, write_for_test};
        let hub = if l_hub == "self" { "lap" } else { "box" };
        let (k_hub, l_hub) = (k_hub.to_string(), l_hub.to_string());
        Pair::build(
            tag,
            vec![Entry::ssh("x", "me@x")],
            move |k| write_for_test(k.member.as_ref().unwrap(), &Member { name: "box".into(), hub: k_hub, ..Default::default() }),
            move |l| write_for_test(l.member.as_ref().unwrap(), &Member { name: "lap".into(), hub: l_hub, ..Default::default() }),
            |k_member, l_member| {
                test_support::fleet_of(hub, &[("lap", l_member), ("box", k_member)]);
            },
        )
    }

    /// `who`'s (`k` or `l`) `member.toml` hub, and its registry's names.
    pub(crate) fn member_and_registry(pair: &Pair, who: &str) -> (crate::serve::fleet::member::Hub, Vec<String>) {
        let data = pair.base.join(format!("{who}-data"));
        let hub = crate::serve::fleet::member::load(&data.join("member.toml")).unwrap().unwrap().hub();
        let names = crate::serve::machines::load(&data.join("machines.toml")).unwrap().into_iter().map(|e| e.name).collect();
        (hub, names)
    }

    pub(crate) fn lap_on_k(pair: &Pair) -> Option<Value> {
        named(&connect(&pair.k_dir).call("machines.list", Value::Null).unwrap().result.unwrap(), "lap")
    }

    /// L, the authority and the hub, removes member K: it signs K's
    /// revoke, pushes it to K (`fleet.sync`), then K hears `fleet.leave`
    /// over the link, which it follows since its log revokes its key: its
    /// `hub` is cleared, its log and its own entries dropped; L unlinks it,
    /// and the link ends.
    #[test]
    fn the_authoritys_rm_of_a_member_revokes_and_removes_it() {
        use crate::serve::fleet::member::Hub;
        let pair = fleet_pair("dial-rm", "lap", "self");
        pair.wait_box("box connected", connected);
        let r = connect(&pair.l_dir).call("machines.rm", json!({"name": "box"})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let answer = r.result.unwrap();
        let removed = json!({"name": "box", "hub": "lap", "leave": "told", "error": null, "forgot": [], "missed": []});
        assert_eq!(
            answer,
            json!({"type": "revoked", "name": "box", "change": "revoke `box`", "pushed": ["box"], "unpushed": [], "removed": removed, "error": null})
        );
        crate::serve::dts::assert_variant("MachineRmRevoked", &answer);
        assert_eq!(member_and_registry(&pair, "k"), (Hub::None, vec![]), "K left: no hub, no entries");
        assert!(test_support::read(&pair.base.join("k-data/member.toml")).is_none(), "K dropped its log");
        assert_eq!(member_and_registry(&pair, "l"), (Hub::This, vec![]), "L unlinked it");
        let l_log = test_support::read(&pair.base.join("l-data/member.toml")).unwrap();
        assert!(l_log.log.revoked_name("box"), "L's log revokes box");
        wait_for("lap gone on K", Duration::from_secs(10), || lap_on_k(&pair).is_none());
        // Again: the revoke stands, nothing more to reach.
        let r = connect(&pair.l_dir).call("machines.rm", json!({"name": "box"})).unwrap().result.unwrap();
        assert_eq!((&r["pushed"], &r["removed"]["leave"]), (&json!([]), &json!("unreachable")));
        assert_eq!(test_support::read(&pair.base.join("l-data/member.toml")).unwrap(), l_log, "signed once");
        pair.finish();
    }

    /// `fleet.*` goes by who asks: from a peer that isn't this host's hub
    /// (or from a local client) `fleet.forget` / `fleet.leave` are refused
    /// and nothing changes; from the hub they still follow only a revoke.
    /// The hub removes no member its log hasn't revoked, and the hub itself
    /// isn't removed.
    #[test]
    fn fleet_forget_from_a_non_hub_peer_is_refused() {
        use crate::serve::fleet::member::Hub;
        let pair = fleet_pair("dial-forget", "self", "box");
        pair.wait_box("box connected", connected);
        wait_for("lap connected on K", Duration::from_secs(10), || lap_on_k(&pair).is_some_and(|r| connected(&r)));
        let api_to = |socket: &Path, name: &str| {
            let stream = link::connect(socket, channel::API).unwrap();
            let duplex = crate::serve::machines::ssh::Duplex::socket(stream);
            crate::serve::machines::link::handshake(name, duplex, Duration::from_secs(10), crate::serve::machines::link::Over::Link).unwrap()
        };
        // lap on K: not K's hub (K is the hub).
        let mut lap = api_to(&endpoint::fleet_socket(&pair.l_dir, "box"), "box");
        for (method, params) in [("fleet.forget", json!({"name": "x"})), ("fleet.leave", json!({}))] {
            let e = lap.conn.call(method, params.clone()).unwrap().error.unwrap();
            assert_eq!(e.code, "denied", "{method}: {e:?}");
            let e = connect(&pair.k_dir).call(method, params).unwrap().error.unwrap();
            assert_eq!(e.code, "denied", "{method} from a local client: {e:?}");
        }
        let e = lap.conn.call("fleet.rm", json!({"name": "box"})).unwrap().error.unwrap();
        assert_eq!(e.code, "denied");
        assert!(e.message.contains("`box`: its key isn't revoked in the hub's membership log"), "{}", e.message);
        assert_eq!(member_and_registry(&pair, "k"), (Hub::This, vec!["x".to_string()]), "nothing changed on K");
        // From the member's own `machine rm`, too.
        let e = connect(&pair.l_dir).call("machines.rm", json!({"name": "box"})).unwrap().error.unwrap();
        assert_eq!(e.code, "denied");
        // K on L: L's hub, but `fleet.forget` follows a revoke L's log hasn't.
        let mut k = api_to(&endpoint::fleet_socket(&pair.k_dir, "lap"), "lap");
        let e = k.conn.call("fleet.forget", json!({"name": "nobody"})).unwrap().error.unwrap();
        assert!(e.code == "denied" && e.message.contains("follows a revoke"), "{e:?}");
        drop((lap, k));
        pair.finish();
    }

    /// Member L leaves: it tells the hub K (`fleet.rm` of itself: K forgets
    /// it, `x` unreached), then clears its `hub` and drops its entries; the
    /// link ends.
    #[test]
    fn a_member_leaves_its_fleet() {
        use crate::serve::fleet::member::Hub;
        let pair = fleet_pair("dial-leave", "self", "box");
        pair.wait_box("box connected", connected);
        wait_for("lap connected on K", Duration::from_secs(10), || lap_on_k(&pair).is_some_and(|r| connected(&r)));
        let r = connect(&pair.l_dir).call("machines.rm", json!({"name": "lap"})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let mut answer = r.result.unwrap();
        crate::serve::dts::assert_variant("MachineRmLeft", &answer);
        let missed = answer["removed"]["missed"].take();
        assert_eq!(missed.as_array().map(|m| m.iter().map(|m| m["name"].clone()).collect::<Vec<_>>()), Some(vec![json!("x")]));
        assert_eq!(
            answer,
            json!({
                "type": "left", "name": "lap", "hub": "box", "error": null, "dropped": ["box"], "proposed": null,
                "removed": {"name": "lap", "hub": "box", "leave": "leaving", "error": null, "forgot": [], "missed": null},
            })
        );
        assert_eq!(member_and_registry(&pair, "l"), (Hub::None, vec![]), "L left");
        assert_eq!(member_and_registry(&pair, "k"), (Hub::This, vec!["x".to_string()]), "K stays the hub");
        wait_for("lap gone on K", Duration::from_secs(10), || lap_on_k(&pair).is_none());
        assert_eq!(connect(&pair.l_dir).call("member.get", json!({})).unwrap().result.unwrap()["hub"], Value::Null);
        // Out of every fleet: nothing more to leave.
        let e = connect(&pair.l_dir).call("machines.rm", json!({"name": "lap"})).unwrap().error.unwrap();
        assert_eq!(e.code, "failed");
        pair.finish();
    }

    /// Three daemons in one fleet, A and B not linked: A (`base/a`, member
    /// `lap`) dials K (`base/k`, `box`), K dials B (`base/b`, `b`), each
    /// through [`peer_transport`], so each of K's links has one end of each
    /// kind (A → K reaches K's `fleet-link`, B → K K's dialer). The log
    /// (authority `lap`, hub `box`) also has `m`, who has no daemon. With
    /// `introduced`, the log is `b`'s (hub `box`) without `lap`, which every
    /// host holds as `box`'s introduction (capped).
    struct Trio {
        base: PathBuf,
        a_dir: PathBuf,
        k_dir: PathBuf,
        b_dir: PathBuf,
        a_member: PathBuf,
        k_member: PathBuf,
        b_member: PathBuf,
        dialed: Vec<Arc<Dialed>>,
        daemons: Vec<std::thread::JoinHandle<Result<Exit>>>,
    }

    impl Trio {
        fn new(tag: &str, introduced: bool) -> Trio {
            Trio::build(tag, introduced, None, |_, _, _| {})
        }

        /// [`Trio::new`] (not introduced) whose `member.toml`s also name
        /// the hub (`box`): the members report their links to it and it
        /// pushes the graph (`fleet::reports`).
        fn hubbed(tag: &str) -> Trio {
            Trio::build(tag, false, Some("box"), |_, _, _| {})
        }

        /// A's, K's and B's options set by `set` too; with `hub`, the log's
        /// hub (not introduced) and every `member.toml`'s.
        fn build(tag: &str, introduced: bool, hub: Option<&str>, set: impl FnOnce(&mut Options, &mut Options, &mut Options)) -> Trio {
            use crate::serve::fleet::member::{Member, write_for_test};
            use crate::serve::fleet::identity::Signer;
            use crate::serve::fleet::membership::Introduction;
            let base = scratch(tag);
            let (a_dir, k_dir, b_dir) = (base.join("a"), base.join("k"), base.join("b"));
            let (mut a_opts, _) = daemon_opts(&base, "a");
            let (mut k_opts, _) = daemon_opts(&base, "k");
            let (mut b_opts, b_data) = daemon_opts(&base, "b");
            let (_, m_data) = daemon_opts(&base, "m");
            let (a_member, k_member, b_member) = (a_opts.member.clone().unwrap(), k_opts.member.clone().unwrap(), b_opts.member.clone().unwrap());
            let m_member = m_data.join("member.toml");
            // B's fleet roots for both peers, so a refusal of one is the trust table's.
            for peer in ["lap", "box"] {
                std::fs::create_dir_all(b_data.join("fleet").join(peer)).unwrap();
            }
            set(&mut a_opts, &mut k_opts, &mut b_opts);
            let members: [(&str, &Path); 4] = [("lap", &a_member), ("box", &k_member), ("b", &b_member), ("m", &m_member)];
            if introduced {
                for (name, toml) in members {
                    write_for_test(toml, &Member { name: name.into(), ..Default::default() });
                }
                let pk = |t: &Path| test_support::key_of(t).public_key();
                let mut s = test_support::fleet("b", &test_support::key_of(&b_member), "box", &[("box", pk(&k_member)), ("m", pk(&m_member))]);
                let genesis = s.log.membership().unwrap().genesis;
                s.introduce(Introduction::sign(&genesis, "lap", pk(&a_member), "box", &test_support::key_of(&k_member))).unwrap();
                for (_, toml) in members {
                    test_support::write(toml, &s);
                }
            } else {
                test_support::fleet_of(hub.unwrap_or("box"), &members);
            }
            if let Some(hub) = hub {
                for (name, toml) in members {
                    let mut m = crate::serve::fleet::member::load(toml).unwrap().unwrap();
                    m.hub = if name == hub { "self" } else { hub }.into();
                    write_for_test(toml, &m);
                }
            }
            crate::serve::machines::update(a_opts.machines.as_ref().unwrap(), |e| e.push(Entry::ssh("box", "me@box"))).unwrap();
            crate::serve::machines::update(k_opts.machines.as_ref().unwrap(), |e| e.push(Entry::ssh("b", "me@b"))).unwrap();
            let (to_k, a_dialed) = peer_transport(&k_dir, &k_member);
            let (to_b, k_dialed) = peer_transport(&b_dir, &b_member);
            a_opts.link_transport = Some(to_k);
            k_opts.link_transport = Some(to_b);
            let mut daemons = Vec::new();
            for (dir, o) in [(&b_dir, b_opts), (&k_dir, k_opts), (&a_dir, a_opts)] {
                daemons.push(spawn_daemon(dir, o));
                drop(connect(dir));
            }
            let trio = Trio { base, a_dir, k_dir, b_dir, a_member, k_member, b_member, dialed: vec![a_dialed, k_dialed], daemons };
            for (dir, name) in [(&trio.a_dir, "box"), (&trio.k_dir, "b"), (&trio.k_dir, "lap"), (&trio.b_dir, "box")] {
                wait_for(&format!("{name} connected on {}", dir.display()), Duration::from_secs(10), || {
                    named(&connect(dir).call("machines.list", Value::Null).unwrap().result.unwrap(), name).is_some_and(|r| connected(&r))
                });
            }
            trio
        }

        /// The fleet root `who` (`a` or `b`) has for `peer`, canonical.
        fn fleet_root(&self, who: &str, peer: &str) -> PathBuf {
            let root = self.base.join(format!("{who}-data/fleet/{peer}"));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::canonicalize(root).unwrap()
        }

        fn finish(self) {
            for dir in [&self.a_dir, &self.k_dir, &self.b_dir] {
                assert!(connect(dir).call("shutdown", json!({})).unwrap().error.is_none());
            }
            for daemon in self.daemons {
                assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
            }
            for d in &self.dialed {
                d.cut();
            }
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// `member_toml`'s member opening `header` over its link in `dir`
    /// (`route::open`, its own key and log): the plaintext end of its
    /// session, pumped on a thread (`session::pump`) as the daemon's
    /// `serve_routed` does.
    fn open_routed(dir: &Path, member_toml: &Path, header: &route::Header) -> Result<UnixStream> {
        let (sealed, keys) = route::open(dir, header, &test_support::key_of(member_toml), &test_support::read(member_toml).unwrap(), route::OPENING)?;
        let (plain, pumped) = UnixStream::pair()?;
        std::thread::spawn(move || crate::serve::fleet::session::pump(pumped, sealed, keys));
        Ok(plain)
    }

    /// An API connection over a routed `stream` to `dest` (the hello as
    /// `0.0.0`, as over a link).
    fn routed_api(stream: UnixStream, dest: &str) -> crate::serve::machines::link::Link {
        use crate::serve::machines::link;
        link::handshake(dest, Duplex::socket(stream), Duration::from_secs(10), link::Over::Link).unwrap()
    }

    fn path(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    /// A's `API` stream to B through K is served by B under its trust for
    /// A: A's fleet root there is reached, B's for K isn't; the other way
    /// too (K relaying from its dialer's end, A answering on its own). K
    /// relays under `worker`, its default.
    #[test]
    fn a_routed_api_stream_is_served_as_its_origins() {
        let trio = Trio::new("route-api", false);
        // (origin's socket dir, its member.toml, origin, destination, the destination's data prefix)
        for (dir, member, origin, dest, data) in [
            (&trio.a_dir, &trio.a_member, "lap", "b", "b"),
            (&trio.b_dir, &trio.b_member, "b", "lap", "a"),
        ] {
            let header = route::Header::new(origin, Channel::Api, path(&["box", dest]));
            let mut api = routed_api(open_routed(dir, member, &header).unwrap(), dest);
            let own = trio.fleet_root(data, origin);
            let r = api.conn.call("instances.list", json!({"dir": own})).unwrap();
            assert!(r.error.is_none(), "{origin} → {dest}: {r:?}");
            let e = api.conn.call("instances.list", json!({"dir": trio.fleet_root(data, "box")})).unwrap().error.unwrap();
            assert_eq!(e.code, "denied", "{origin} → {dest}: the relay's fleet root isn't the origin's: {e:?}");
            let r = api.conn.call("roots.list", json!({})).unwrap().result.unwrap();
            let fleet = r.as_array().unwrap().iter().find(|r| r["fleet"] == true).unwrap()["root"].clone();
            assert_eq!(fleet, json!(own), "{origin} → {dest}");
        }
        trio.finish();
    }

    /// K holds no key of A's: a stream claiming to come from A, sent on K's
    /// own link to B, is refused, as through K (the hop before B) or
    /// straight (not from A, its first hop).
    #[test]
    fn a_relay_cannot_open_a_stream_as_another_member() {
        let trio = Trio::new("route-forged", false);
        let k_key = test_support::key_of(&trio.k_member);
        let k_log = test_support::read(&trio.k_member).unwrap();
        for (p, want) in [
            (&["box", "b"][..], "is refused: `b`: `lap` proved key"),
            (&["b"][..], "is refused: `b`: the routed stream from `lap` to `b`: it came from `box`, not from `lap`"),
        ] {
            let header = route::Header::new("lap", Channel::Api, path(p));
            let stream = link::connect(&endpoint::fleet_socket(&trio.k_dir, "b"), channel::ROUTE).unwrap();
            let (mut r, mut w) = (&stream, &stream);
            route::write_header(&mut w, &header).unwrap();
            let e = format!("{:#}", route::origin_side(&mut r, &mut w, &header, &k_key, &k_log).err().expect("refused"));
            assert!(e.contains(want), "{p:?}: {e}");
        }
        trio.finish();
    }

    /// A relay refuses, naming itself, a next hop with no link to it (`m`
    /// has no daemon); a routed `AGENT` stream isn't refused as a channel
    /// (decision 58): B, granting nobody, answers it `NOT_SERVING`.
    #[test]
    fn a_relay_refuses_a_hop_that_isnt_up_not_agent_streams() {
        let trio = Trio::new("route-refused", false);
        let header = route::Header::new("lap", Channel::Api, path(&["box", "m"]));
        let e = format!("{:#}", open_routed(&trio.a_dir, &trio.a_member, &header).unwrap_err());
        assert_eq!(e, "the routed stream with `m` is refused: `box`: the routed stream from `lap` to `m`: `m` has no link to this host");
        let agent = route::Header::new("lap", Channel::Agent, path(&["box", "b"]));
        let stream = open_routed(&trio.a_dir, &trio.a_member, &agent).unwrap();
        assert_eq!(status_byte(&stream), Some(crate::serve::fleet::agent::NOT_SERVING));
        trio.finish();
    }

    /// The first byte on `stream` (an `AGENT` stream's status), `None` at
    /// its end or after 10 s.
    fn status_byte(stream: &UnixStream) -> Option<u8> {
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut b = [0u8; 1];
        (&mut &*stream).read_exact(&mut b).ok().map(|()| b[0])
    }

    /// A fake host agent at `dir/agent.sock`: answers `pong` to each
    /// connection's `ping`.
    fn ping_agent(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for mut c in listener.incoming().flatten() {
                let mut ping = [0u8; 4];
                if c.read_exact(&mut ping).is_ok() {
                    let _ = c.write_all(b"pong");
                }
            }
        });
        path
    }

    /// `ping` to the agent socket at `socket`: its answer.
    fn ping(socket: &Path) -> [u8; 4] {
        let mut c = UnixStream::connect(socket).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        c.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        c.read_exact(&mut got).unwrap();
        got
    }

    /// Whether `dir`'s daemon holds `member`'s report granting agent to
    /// exactly `grantees`.
    fn grants_in_graph(dir: &Path, member: &str, grantees: &[&str]) -> bool {
        let g = connect(dir).call("graph.get", json!({})).unwrap().result.unwrap();
        g["reports"].as_array().unwrap().iter().any(|r| r["report"]["member"] == member && r["report"]["agent"] == json!(grantees))
    }

    /// Whether `dir`'s daemon holds `member`'s report with a connected link
    /// to each of `peers`.
    fn links_in_graph(dir: &Path, member: &str, peers: &[&str]) -> bool {
        let g = connect(dir).call("graph.get", json!({})).unwrap().result.unwrap();
        g["reports"].as_array().unwrap().iter().filter(|r| r["report"]["member"] == member).any(|r| {
            let links = r["report"]["links"].as_array().cloned().unwrap_or_default();
            peers.iter().all(|p| links.iter().any(|l| l["peer"] == *p && l["state"] == "connected"))
        })
    }

    /// `lap` grants `b`, which it has no link to, agent (decision 66): B
    /// sees it in lap's report (through the hub), binds
    /// `agent-route-lap.sock`, and a client there reaches lap's agent over
    /// a routed `AGENT` stream through K; K, not granted, binds nothing.
    /// Withdrawn, B's socket goes.
    #[test]
    fn a_routed_member_reaches_the_agent_of_a_member_granting_it() {
        let base = scratch("route-agent-fake");
        let agent = ping_agent(&base);
        let trio = Trio::build("route-agent", false, Some("box"), |a, _, _| {
            a.host_agent = Some(Arc::new(move || Some(agent.clone())));
        });
        let socket = endpoint::route_agent_socket(&trio.b_dir, "lap");
        let set = |on: bool| {
            let r = connect(&trio.a_dir).call("machines.set", json!({"name": "b", "agent": on})).unwrap();
            assert!(r.error.is_none(), "{r:?}");
        };
        set(true);
        wait_for("B binds agent-route-lap.sock", Duration::from_secs(15), || socket.exists());
        assert_eq!(&ping(&socket), b"pong", "lap's agent, through K");
        assert_eq!(endpoint::agent_sockets(&trio.b_dir), [socket.clone()]);
        assert!(!endpoint::route_agent_socket(&trio.k_dir, "lap").exists() && !endpoint::link_agent_socket(&trio.k_dir, "lap").exists());
        set(false);
        wait_for("B unbinds it", Duration::from_secs(15), || !socket.exists());
        trio.finish();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `lap` grants `box` (K, the relay between it and B) but not `b`: K
    /// has its agent-link from lap's dialing, B binds nothing, and a routed
    /// `AGENT` stream from B to lap is answered `NOT_SERVING`: the grant is
    /// checked for the origin's key, never the relay's.
    #[test]
    fn a_relays_agent_grant_does_not_reach_past_it() {
        let base = scratch("route-agent-relay-fake");
        let agent = ping_agent(&base);
        let trio = Trio::build("route-agent-relay", false, Some("box"), |a, _, _| {
            a.host_agent = Some(Arc::new(move || Some(agent.clone())));
        });
        let r = connect(&trio.a_dir).call("machines.set", json!({"name": "box", "agent": true})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let k_agent = endpoint::link_agent_socket(&trio.k_dir, "lap");
        wait_for("K binds agent-link-lap.sock", Duration::from_secs(15), || k_agent.exists());
        assert_eq!(&ping(&k_agent), b"pong");
        wait_for("B holds lap's report granting box", Duration::from_secs(15), || grants_in_graph(&trio.b_dir, "lap", &["box"]));
        let header = route::Header::new("b", Channel::Agent, path(&["box", "lap"]));
        let stream = open_routed(&trio.b_dir, &trio.b_member, &header).unwrap();
        assert_eq!(status_byte(&stream), Some(crate::serve::fleet::agent::NOT_SERVING));
        assert!(!endpoint::route_agent_socket(&trio.b_dir, "lap").exists(), "lap's report doesn't list b");
        assert!(!endpoint::route_agent_socket(&trio.k_dir, "lap").exists(), "covered by K's agent-link");
        trio.finish();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// K (`box`) grants `lap`, its inbound neighbor, agent: L, the dialer,
    /// binds `agent-route-box.sock` over its own link socket (no route:
    /// K's `fleet-link --peer lap` serves `AGENT`), and a client there
    /// reaches K's agent. K binds no socket for lap.
    #[test]
    fn the_dialer_reaches_the_agent_of_the_machine_it_dialed_when_granted() {
        let base = scratch("dial-agent-inbound-fake");
        let agent = ping_agent(&base);
        let pair = Pair::with(
            "dial-agent-inbound",
            Vec::new(),
            |k| k.host_agent = Some(Arc::new(move || Some(agent.clone()))),
            |_| {},
        );
        pair.wait_box("box connected", connected);
        let r = connect(&pair.k_dir).call("machines.set", json!({"name": "lap", "agent": true})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let socket = endpoint::route_agent_socket(&pair.l_dir, "box");
        wait_for("L binds agent-route-box.sock", Duration::from_secs(15), || socket.exists());
        assert!(!endpoint::route_socket(&pair.l_dir, "box").exists(), "box is linked: no route");
        assert_eq!(&ping(&socket), b"pong", "K's agent, over L's link");
        assert!(endpoint::agent_sockets(&pair.k_dir).is_empty(), "K binds none: lap grants it nothing");
        let r = connect(&pair.k_dir).call("machines.set", json!({"name": "lap", "agent": false})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        wait_for("L unbinds it", Duration::from_secs(15), || !socket.exists());
        pair.finish();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A reaches B, which it has no link to, through `route-b.sock`: its
    /// daemon opens the stream over the graph K pushed (`lap` → `box` →
    /// `b`), and B serves it as A's. `m` (no daemon) signs a report saying
    /// it's linked to everyone since long ago and is always on, so its
    /// paths sort first: `m → b` is skipped (A has no link to `m`), and to
    /// `m` itself every path fails over to the next, the API client reading
    /// why as its hello's answer. A has no route socket for `box`, its link.
    #[test]
    fn a_member_without_a_link_is_reached_through_its_route_socket() {
        use crate::serve::fleet::graph::{LinkReport, Report, sign_report};
        use crate::serve::fleet::reports::{Reports, Signed};
        use crate::serve::machines::link::{Over, handshake};
        use crate::serve::snapshots::LinkState;
        let trio = Trio::hubbed("route-socket");
        let m_member = trio.base.join("m-data/member.toml");
        let genesis = test_support::read(&trio.a_member).unwrap().log.membership().unwrap().genesis;
        let links = ["lap", "box", "b"].map(|p| LinkReport { peer: p.into(), dials: false, state: LinkState::Connected, since_ms: 1 }).to_vec();
        let report = Report { member: "m".into(), trust: Default::default(), always_on: true, links, agent: vec![] };
        let later = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 3_600_000;
        let (statement, report) = sign_report(&test_support::key_of(&m_member), &genesis, &report, 0, later);
        // Carried to K, the hub, as `b` over B's link: K pushes it on to
        // `lap`. Not over A's: K's flood skips the peer a change came from
        // (it holds it), and A never would.
        let mut k = routed_api(link::connect(&endpoint::fleet_socket(&trio.b_dir, "box"), channel::API).unwrap(), "box");
        let params = serde_json::to_value(Reports { reports: vec![Signed { statement, report }] }).unwrap();
        let r = k.conn.call("fleet.graph", params).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let (to_b, to_m) = (endpoint::route_socket(&trio.a_dir, "b"), endpoint::route_socket(&trio.a_dir, "m"));
        wait_for("A's route sockets for b and m", Duration::from_secs(15), || to_b.exists() && to_m.exists());
        // m's report alone binds both: A's `box → b` comes from box's.
        wait_for("A holds box's report linking lap and b", Duration::from_secs(15), || {
            links_in_graph(&trio.a_dir, "box", &["lap", "b"])
        });
        assert!(!endpoint::route_socket(&trio.a_dir, "box").exists(), "box is linked: its fleet socket serves");

        let mut api = routed_api(route::connect(&trio.a_dir, "b", channel::API).unwrap(), "b");
        let r = api.conn.call("instances.list", json!({"dir": trio.fleet_root("b", "lap")})).unwrap();
        assert!(r.error.is_none(), "served as lap's at b: {r:?}");

        let stream = route::connect(&trio.a_dir, "m", channel::API).unwrap();
        let e = match handshake("m", Duplex::socket(stream), Duration::from_secs(20), Over::Link) {
            Ok(_) => panic!("no path to `m` opens"),
            Err(e) => e.to_string(),
        };
        for want in [
            "no path to `m` opened: via m: no link to `m` is up here",
            "via box → m: the routed stream with `m` is refused: `box`: the routed stream from `lap` to `m`: `m` has no link to this host",
            "via box → b → m: the routed stream with `m` is refused: `b`:",
            "(failed)",
        ] {
            assert!(e.contains(want), "{want:?} in {e}");
        }
        trio.finish();
    }

    /// A reaches B, which it has no link to, as a routed machine (fleet
    /// plan decision 60): `machines.list` and `machines.get` say `routed`,
    /// via `box`, connected (a path starts with A's link up); a quick call,
    /// a forwarded verb and a federated resolve go to it by name over
    /// `route-b.sock`, served under B's trust for A (not for the relay);
    /// an agent grant for it is written by B's key (decision 67), and shows.
    #[test]
    fn a_routed_machine_takes_quick_calls_and_verbs() {
        let (b_runner, b_verbs) = crate::serve::verbs::tests::script_runner("route-machine-b", r#"echo "b $*""#);
        let trio = Trio::build("route-machine", false, Some("box"), |_, _, b| {
            b.verbs = Some(b_runner);
            b.peer_env = Some(std::collections::BTreeMap::from([("PATH".into(), std::env::var("PATH").unwrap_or_default())]));
            // One instance in B's fleet root for lap.
            let state = b.state.clone().unwrap();
            let root = std::fs::canonicalize(state.parent().unwrap().join("fleet/lap")).unwrap();
            let toml = format!(
                "[instance.b-1]\ninstance_id = \"id-b\"\nsandbox = \"web\"\ncontainer = \"devsandbox-b-1\"\nfolder = \"/w\"\n\
                 workspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                root.to_str().unwrap()
            );
            std::fs::write(state, toml).unwrap();
        });
        let rows = || connect(&trio.a_dir).call("machines.list", Value::Null).unwrap().result.unwrap();
        wait_for("b routed and connected on A", Duration::from_secs(15), || named(&rows(), "b").is_some_and(|r| connected(&r)));
        let b = named(&rows(), "b").unwrap();
        assert_eq!((&b["kind"], &b["via"], &b["target"], &b["agent"]), (&json!("routed"), &json!("box"), &Value::Null, &json!(false)));
        crate::serve::dts::assert_matches("MachineRow", &b);
        let mut a = connect(&trio.a_dir);
        let info = a.call("machines.get", json!({"name": "b"})).unwrap().result.unwrap();
        assert_eq!((&info["kind"], &info["target"]), (&json!("routed"), &Value::Null));

        let own = trio.fleet_root("b", "lap");
        let r = a.call("instances.list", json!({"dir": own, "machine": "b"})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let e = a.call("instances.list", json!({"dir": trio.fleet_root("b", "box"), "machine": "b"})).unwrap().error.unwrap();
        assert_eq!(e.code, "denied", "B's trust for lap, not for the relay: {e:?}");
        let (r, out, _) = verb(&mut a, "stop", json!({"root": own, "env": {}, "instance": "b-1", "machine": "b"}));
        assert!(r.error.is_none(), "{r:?}");
        assert_eq!(out, format!("b --in-process -C {} stop -- b-1\n", own.display()));
        let r = a.call("instances.resolve", json!({"name": "b-1", "machines": true})).unwrap().result.unwrap();
        assert_eq!(r["matches"], json!(["b/b-1"]), "{r}");

        let r = a.call("machines.set", json!({"name": "b", "agent": true})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let grants = crate::serve::fleet::member::load(&trio.a_member).unwrap().unwrap().agent;
        let b_key = crate::serve::fleet::identity::Signer::public_key(&test_support::key_of(&trio.b_member));
        assert_eq!(grants, [crate::serve::fleet::member::Grant { name: "b".into(), key: b_key }]);
        assert_eq!(named(&rows(), "b").unwrap()["agent"], true);
        assert_eq!(a.call("machines.get", json!({"name": "b"})).unwrap().result.unwrap()["agent"], true);
        drop(a);
        trio.finish();
        let _ = std::fs::remove_dir_all(&b_verbs);
    }

    /// An introduced origin (capped) is served capped at its destination:
    /// B, the authority, refuses its `fleet.propose` as a capped member's,
    /// while its fleet root is reached.
    #[test]
    fn an_introduced_origin_is_served_capped() {
        let trio = Trio::new("route-capped", true);
        let header = route::Header::new("lap", Channel::Api, path(&["box", "b"]));
        let mut api = routed_api(open_routed(&trio.a_dir, &trio.a_member, &header).unwrap(), "b");
        let r = api.conn.call("instances.list", json!({"dir": trio.fleet_root("b", "lap")})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let e = api.conn.call("fleet.propose", json!({"op": {"kind": "revoke", "name": "m"}})).unwrap().error.unwrap();
        assert!(e.code == "denied" && e.message.contains("capped"), "{e:?}");
        trio.finish();
    }

    /// `method` on the daemon in `dir`: its result, or its error's code and message.
    fn call(dir: &Path, method: &str, params: Value) -> std::result::Result<Value, (String, String)> {
        let r = connect(dir).call(method, params).unwrap();
        match r.error {
            Some(e) => Err((e.code, e.message)),
            None => Ok(r.result.unwrap_or(Value::Null)),
        }
    }

    /// Whether `dir`'s daemon lists `name` as a routed machine, connected.
    fn routed_up(dir: &Path, name: &str) -> bool {
        named(&call(dir, "machines.list", Value::Null).unwrap(), name).is_some_and(|r| r["kind"] == "routed" && connected(&r))
    }

    /// B has no link to the fleet's authority `lap` (A): its `machine rm m`
    /// is proposed over a route (`b` → `box` → `lap`), admitted at A as
    /// B's (the routed origin, not the relay `box`), and waits in A's
    /// pending list; the same again collapses with it.
    #[test]
    fn a_proposal_reaches_the_authority_over_a_route() {
        let trio = Trio::hubbed("route-propose");
        wait_for("lap routed and connected on B", Duration::from_secs(15), || routed_up(&trio.b_dir, "lap"));
        let r = call(&trio.b_dir, "machines.rm", json!({"name": "m"})).unwrap();
        assert_eq!(r, json!({"type": "proposed", "name": "m", "authority": "lap", "id": 1, "error": null}));
        assert_eq!(call(&trio.b_dir, "machines.rm", json!({"name": "m"})).unwrap()["id"], 1, "pending: the same id");
        let pending = call(&trio.a_dir, "membership.pending", json!({})).unwrap();
        assert_eq!(pending.as_array().unwrap().len(), 1, "{pending}");
        assert_eq!((&pending[0]["from"], &pending[0]["op"]), (&json!("b"), &json!({"kind": "revoke", "name": "m"})));
        assert_eq!(call(&trio.k_dir, "membership.pending", json!({})).unwrap(), json!([]), "nothing queued on the relay");
        trio.finish();
    }

    /// `b` is the hub, A (`lap`, the authority) not linked to it: A's
    /// `machine rm m` signs the revoke, pushes it to K (which floods it to
    /// B) and asks B's `fleet.rm` over a route; B's removal tells `box`
    /// over its link and `lap`, a routed member, over a route
    /// (`fleet.forget`, admitted there as from the hub `b`).
    #[test]
    fn a_removal_forgets_on_a_routed_member() {
        let trio = Trio::build("route-forget", false, Some("b"), |_, _, _| {});
        wait_for("b routed and connected on A, lap on B", Duration::from_secs(15), || {
            routed_up(&trio.a_dir, "b") && routed_up(&trio.b_dir, "lap")
        });
        let mut r = call(&trio.a_dir, "machines.rm", json!({"name": "m"})).unwrap();
        assert_eq!((&r["type"], &r["pushed"]), (&json!("revoked"), &json!(["box"])), "{r}");
        wait_for("b holds the revoke", Duration::from_secs(10), || test_support::read(&trio.b_member).unwrap().log.revoked_name("m"));
        if r["removed"].is_null() {
            // K sends the revoke on to B off its `fleet.sync`'s request
            // thread, so A's `fleet.rm` can reach B first, refused there
            // (no revoke of `m` yet): the retry removes it.
            r = call(&trio.a_dir, "machines.rm", json!({"name": "m"})).unwrap();
        }
        let removed = &r["removed"];
        assert_eq!((&removed["hub"], &removed["leave"], &removed["forgot"]), (&json!("b"), &json!("unreachable"), &json!(["box", "lap"])), "{r}");
        assert_eq!(removed["missed"], json!([]), "{r}");
        trio.finish();
    }

    /// `b` is the hub, A linked only to K (not the hub): B's link reports
    /// reach A through K's flood, with no exchange of A's own. B starts
    /// with a registry entry `m` it can't reach (its report lists the link
    /// down); `machine unlink m` there changes its report, and the newer
    /// one, without `m`, reaches A the same way.
    #[test]
    fn a_report_change_on_the_hub_reaches_a_member_it_has_no_link_to() {
        let trio = Trio::build("graph-flood", false, Some("b"), |_, _, b| {
            crate::serve::machines::update(b.machines.as_ref().unwrap(), |e| e.push(Entry::ssh("m", "me@m"))).unwrap();
            b.link_transport = Some(Transport::new(|_, _| anyhow::bail!("ssh: connect to host m port 22: Connection refused")));
        });
        let b_report = || {
            let g = call(&trio.a_dir, "graph.get", json!({})).unwrap();
            g["reports"].as_array().unwrap().iter().find(|r| r["report"]["member"] == "b").cloned()
        };
        let peers = |r: &Value| -> Vec<String> { r["report"]["links"].as_array().unwrap().iter().map(|l| l["peer"].as_str().unwrap().to_string()).collect() };
        wait_for("b's report, linked to box and listing m, on A", Duration::from_secs(20), || {
            b_report().is_some_and(|r| {
                let p = peers(&r);
                p.contains(&"m".to_string()) && r["report"]["links"].as_array().unwrap().iter().any(|l| l["peer"] == "box" && l["state"] == "connected")
            })
        });
        let before = b_report().unwrap()["statement"]["version"].as_u64().unwrap();
        assert_eq!(call(&trio.b_dir, "machines.unlink", json!({"name": "m"})).unwrap(), json!({"ok": true}));
        wait_for("b's newer report, without m, on A", Duration::from_secs(20), || {
            b_report().is_some_and(|r| r["statement"]["version"].as_u64().unwrap() > before && !peers(&r).contains(&"m".to_string()))
        });
        trio.finish();
    }

    /// A `run --place` on member A (`lap`, hub `box`), A and K busy, B
    /// roomy: A tells the hub it reaches `b` over a route, the hub places it
    /// there (its fresh `capacity.get` of B), and A moves the code into its
    /// fleet root on B and forwards the run, both over the route.
    #[test]
    fn a_members_run_is_placed_on_a_routed_machine() {
        use crate::serve::capacity::{Capacity, Caps};
        use crate::serve::fleet::code::repo::tests::{git, origin_and_checkout};
        const G: u64 = 1 << 30;
        let (b_runner, b_verbs) =
            crate::serve::verbs::tests::script_runner("route-place-b", r#"echo "b $*"; cat "$3/dockerfiles/web.Dockerfile" "$3/../web/F""#);
        let (a_runner, a_verbs) = crate::serve::verbs::tests::script_runner("route-place-a", r#"echo "local $*""#);
        let path = std::env::var("PATH").unwrap_or_default();
        let src = scratch("route-place-src");
        std::fs::create_dir_all(&src).unwrap();
        origin_and_checkout(&src, "web");
        let cfg = src.join("cfg");
        std::fs::create_dir_all(cfg.join("dockerfiles")).unwrap();
        std::fs::write(cfg.join(crate::config::CONFIG_FILE), "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\n").unwrap();
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM x\n").unwrap();
        let cfg = std::fs::canonicalize(&cfg).unwrap();
        let capacity = |avail: u64, running: usize, caps: Option<Caps>| Capacity {
            cpus: Some(8),
            mem_total: Some(32 * G),
            mem_available: Some(avail * G),
            disk_free: Some(400 * G),
            disk_total: Some(500 * G),
            running,
            caps,
        };
        let busy = capacity(4, 3, Some(Caps { instances: Some(1), ..Caps::default() }));
        let roomy = capacity(28, 0, None);
        let repos = src.join("a-repos");
        let trio = Trio::build("route-place", false, Some("box"), |a, k, b| {
            a.verbs = Some(a_runner);
            a.repos = Some(repos);
            let (busy_a, busy_k) = (busy.clone(), busy.clone());
            a.local_capacity = Some(Arc::new(move || busy_a.clone()));
            k.local_capacity = Some(Arc::new(move || busy_k.clone()));
            b.verbs = Some(b_runner);
            b.peer_env = Some(std::collections::BTreeMap::from([("PATH".into(), path.clone())]));
            b.local_capacity = Some(Arc::new(move || roomy.clone()));
        });
        wait_for("b routed and connected on A", Duration::from_secs(15), || routed_up(&trio.a_dir, "b"));
        let fleet_root = trio.fleet_root("b", "lap");
        let mut a = connect(&trio.a_dir);
        let (r, out, err) = verb(&mut a, "run", json!({"root": cfg, "env": {"PATH": std::env::var("PATH").unwrap_or_default()}, "sandbox": "web", "branch": "feature", "place": true}));
        assert!(r.error.is_none(), "{r:?} {err}");
        assert_eq!(r.result.as_ref().unwrap()["machine"], "b", "{r:?} {err}");
        assert!(err.starts_with("placed on b ("), "{err}");
        let roots: Vec<_> =
            std::fs::read_dir(fleet_root.join("roots")).unwrap().flatten().map(|e| e.file_name()).filter(|n| n != ".in").collect();
        assert_eq!(roots.len(), 1, "{roots:?}");
        let remote = fleet_root.join("roots").join(&roots[0]).join("cfg");
        assert_eq!(out, format!("b --in-process -C {} run --branch=feature -- web\nFROM x\nfeature\n", remote.display()));
        assert_eq!(git(&remote.join("../web"), &["symbolic-ref", "--short", "HEAD"]), "feature");
        drop(a);
        trio.finish();
        for dir in [src, a_verbs, b_verbs] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
