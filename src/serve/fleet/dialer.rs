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
//! machine as they reach an offered worker. IN: each stream the machine
//! opens (`API`, `PTY`, `TUNNEL`, `TRANSFER`, `AGENT`) goes to this daemon
//! in-process ([`Open`]: `Shared::open_lent` with the entry's name as the
//! peer, its fleet root `fleet/<entry>/`), under this host's trust table
//! for it; `AGENT` is answered with this host's agent while the entry has
//! `agent` on (`daemon::agent_for`).
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
//! through `fleet-<entry>.transfer.sock`). Two non-hubs follow nothing.
//!
//! The link's sockets are bound as `link::Socket` binds them (the newest
//! binder wins), so after a version handoff the next daemon's links take
//! them over: the old daemon's ssh dies with it, the far end's `fleet-link`
//! exits, the new daemon redials.

use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use super::follow::{self, Incoming, PUSH_WAIT};
use super::lent::Channel;
use super::transfer::Receivers;
use super::transport::Transport;
use super::update::{self, Build, Decision};
use super::worker::Follow;
use super::{LINK, link};
use crate::commands::fleet_link;
use crate::devsbd::mux::Conn;
use crate::devsbd::proto::channel;
use crate::serve::endpoint;
use crate::serve::machines::service::Held;
use crate::serve::machines::ssh::{Dial, Duplex};
use crate::serve::machines::{Entry, check_name};

/// How the daemon answers one stream machine `<name>` opens on `Channel`:
/// the machine's end of a new connection, `None` to refuse it. Runs on the
/// link's read thread: it must return fast.
pub type Open = Arc<dyn Fn(&str, Channel) -> Option<Conn> + Send + Sync>;

/// The far end's devsandbox args: `fleet-link --peer <member>`, `member`
/// being this host's member name (`fleet::member`), which the machine
/// knows this host by.
pub fn link_args(member: &str) -> Result<Vec<String>> {
    check_name(member)?;
    Ok(["fleet-link", "--peer", member].map(String::from).to_vec())
}

/// The daemon's [`Dial`] of a registered machine, ssh or offered: a
/// connection to its `fleet-<name>.sock` in `socket_dir` (one `API` stream
/// on its link, the daemon from its first byte:
/// `machines::link::handshake` with `Over::Link`).
pub fn dial(socket_dir: PathBuf) -> Dial {
    Arc::new(move |entry: &Entry| {
        let path = endpoint::fleet_socket(&socket_dir, &entry.name);
        let stream =
            endpoint::Stream::connect(&path).map_err(|e| anyhow::anyhow!("its link isn't up ({}: {e})", path.display()))?;
        Ok(Duplex::socket(stream))
    })
}

/// What the daemon's ssh machines' links need.
pub struct Dialer {
    pub transport: Transport,
    /// The socket dir: where the link's sockets are bound.
    pub dir: PathBuf,
    /// This host's member name, read at each dial (a `member.toml` change
    /// counts from the next one).
    pub member: Arc<dyn Fn() -> Result<String> + Send + Sync>,
    /// IN: the daemon's answer to each of the machine's streams.
    pub open: Open,
    /// This host is the fleet's hub (`[serve] hub`): its `Hello` says so.
    pub hub: bool,
    /// How this host follows a hub machine's build.
    pub follow: Follow,
    /// The sentinel's and the `Hello`s' wait (`service::Timing::handshake`).
    pub handshake: Duration,
    pub log: fn(&str),
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
        let mut duplex = match self.transport.dial_with(target, entry.command(), &args) {
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
        let end = self.answer(entry, &duplex, reader, writer, killer, held);
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

    /// The answerer over `duplex` until the link ends.
    fn answer(
        &self,
        entry: &Entry,
        duplex: &Duplex,
        reader: endpoint::Stream,
        writer: endpoint::Stream,
        killer: endpoint::Stream,
        held: &Held,
    ) -> Ended {
        let name = entry.name.as_str();
        let dir = &self.dir;
        let (api, pty) = (endpoint::fleet_socket(dir, name), endpoint::fleet_pty_socket(dir, name));
        let (tunnel, transfer) = (endpoint::fleet_tunnel_socket(dir, name), endpoint::fleet_transfer_socket(dir, name));
        let sockets = [
            (channel::API, api.as_path()),
            (channel::PTY, pty.as_path()),
            (channel::TUNNEL, tunnel.as_path()),
            (channel::TRANSFER, transfer.as_path()),
        ];
        // While the hub's binary is on its way nothing but it is served.
        let receiving = AtomicBool::new(false);
        let incoming = Arc::new(Incoming::default());
        let binary = Arc::new(incoming.receiver(Receivers::new(self.log)));
        let served = |ch: Channel| {
            let receiving = &receiving;
            move || if receiving.load(Ordering::Relaxed) { None } else { (self.open)(name, ch) }
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
            .with(channel::TRANSFER, transfers);
        let ours = update::hello_of(self.hub);
        let spec = link::Spec { hello: ours, ..LINK };
        let wanted: Mutex<Option<String>> = Mutex::new(None);
        let mut end_link = duplex.stream.try_clone().ok().map(follow::shutting);
        let endpoint = link::Endpoint { sockets: &sockets, serves, outlet: None };
        let end = link::answerer_with(
            &spec,
            reader,
            writer,
            endpoint,
            |peer| {
                let _ = duplex.stream.set_read_timeout(None);
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
                if self.hub {
                    fleet_link::push_build(ours, &peer.hash, transfer.clone(), fleet_link::own_binary);
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
    use crate::serve::daemon::{Exit, Options};
    use crate::serve::machines::ssh;
    use crate::serve::proto::Version;
    use crate::serve::snapshots::Join;

    fn connect(dir: &Path) -> Conn {
        let v = Version { semver: "0.6.0".into(), build: 1 };
        client::connect_with(dir, "test", &v, client::START_TIMEOUT, &|_, _| Ok(())).unwrap()
    }

    fn wait_for(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
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
    /// `k_dir`, a member taking no dev builds (`Fake` steps). Its error, as
    /// `main` would print it, goes to the stand-in ssh's stderr.
    pub(crate) fn peer_transport(k_dir: &Path) -> (Transport, Arc<Dialed>) {
        let dialed = Arc::new(Dialed::default());
        let (k_dir, seen) = (k_dir.to_path_buf(), Arc::clone(&dialed));
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
                hub: false,
                daemon: Arc::new(move || Ok(connect(&daemon_dir))),
                follow: Follow { steps: Arc::new(update::tests::Fake::default()), dev_builds: Arc::new(|| false) },
                reannounce: Duration::from_millis(20),
                push_wait: Duration::from_secs(5),
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
    /// `box` (K, its root K's own), reached through [`peer_transport`].
    struct Pair {
        base: PathBuf,
        k_dir: PathBuf,
        l_dir: PathBuf,
        k_own: PathBuf,
        l_registry: PathBuf,
        dialed: Arc<Dialed>,
        daemons: Vec<std::thread::JoinHandle<Result<Exit>>>,
    }

    fn daemon_opts(base: &Path, name: &str) -> (Options, PathBuf) {
        let data = base.join(format!("{name}-data"));
        std::fs::create_dir_all(&data).unwrap();
        let mut o = opts(1, 60_000);
        o.lend = Some(data.join("lend.toml"));
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
            let (transport, dialed) = peer_transport(&k_dir);
            l_opts.link_transport = Some(transport);
            l(&mut l_opts);
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

        fn wait_box(&self, what: &str, cond: impl Fn(&Value) -> bool) -> Value {
            wait_for(what, Duration::from_secs(10), || cond(&self.box_row()));
            self.box_row()
        }

        fn finish(self) {
            for (dir, daemon) in [&self.l_dir, &self.k_dir].into_iter().zip(self.daemons.into_iter().rev()) {
                assert!(connect(dir).call("shutdown", json!({})).unwrap().error.is_none());
                assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
            }
            self.dialed.cut();
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn connected(r: &Value) -> bool {
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
        for socket in [endpoint::fleet_socket(&pair.l_dir, "box"), endpoint::fleet_transfer_socket(&pair.l_dir, "box")] {
            assert!(socket.exists(), "{}", socket.display());
        }
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
    /// `exec box/web-1 -- …` one `PTY` dial through `fleet-box.pty.sock`
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
        let stream = endpoint::Stream::connect(endpoint::fleet_pty_socket(&pair.l_dir, "box")).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let term = pty_client::Terminal { input: None, output: &mut out, errors: &mut err, size: &|| None };
        let end = pty_client::attach(stream, &pty_client::header(&request), term);
        assert_eq!((pty_client::exit_of(&end, false).0, out, err), (3, b"out\n".to_vec(), b"err\n".to_vec()));
        drop(l);
        pair.finish();
    }

    /// K's `AGENT` streams are answered with this host's agent while L's
    /// entry has `agent` on: K binds `agent-link-lap.sock` then, and it
    /// leads to L's agent; off, it goes. The link stays up throughout.
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
        set(false);
        wait_for("K unbinds it", Duration::from_secs(10), || !socket.exists());
        assert_eq!((pair.dialed.count.load(Ordering::SeqCst), connected(&pair.box_row())), (1, true), "the same link");
        assert!(pair.l_registry.exists());
        pair.finish();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// K already has a machine named `lap` (the two hosts added each
    /// other): its refusal is box's error here.
    #[test]
    fn a_clash_refused_there_is_the_machines_error() {
        let pair = Pair::new("dial-clash", vec![Entry::ssh("lap", "me@lap")], |_| {});
        let row = pair.wait_box("box refused", |r| r["error"].as_str().is_some_and(|e| e.contains("already a machine here")));
        assert_eq!(row["state"], "unreachable");
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
        std::fs::write(base.join("member.toml"), "name = \"lap\"\n").unwrap();
        let hello: &'static str = Box::leak(format!("9.9.9 abc published {} hub", update::PLATFORM).into_boxed_str());
        let transport = Transport::new(move |_, _| {
            let (ours, theirs) = UnixStream::pair()?;
            let (r, w) = (theirs.try_clone()?, theirs);
            std::thread::spawn(move || {
                let spec = link::Spec { hello, ..LINK };
                link::opener_with(&spec, r, w, link::Endpoint::default(), |_| Ok(()))
            });
            Ok(Duplex::socket(ours))
        });
        let fake = Arc::new(update::tests::Fake::reporting("9.9.9"));
        let mut o = opts(1, 60_000);
        o.machines = Some(registry);
        o.lend = Some(base.join("lend.toml"));
        o.link_transport = Some(transport);
        o.lend_update = Some(fake.clone());
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
}
