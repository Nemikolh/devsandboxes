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
//! opens (`API`, `PTY`, `TUNNEL`, `TRANSFER`, `AGENT`) goes to this daemon
//! in-process ([`Open`]: `Shared::open_lent` with the entry's name as the
//! peer, its fleet root `fleet/<entry>/`), under this host's trust table
//! for it; `AGENT` is answered with this host's agent while the entry has
//! `agent` on (`daemon::agent_for`).
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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use super::follow::{self, Incoming, PUSH_WAIT};
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

/// The daemon's [`Dial`] of a machine, ssh or inbound: a
/// connection to its `fleet-<name>.sock` in `socket_dir` (one `API` stream
/// on its link, `link::connect`, the daemon from its first byte:
/// `machines::link::handshake` with `Over::Link`).
pub fn dial(socket_dir: PathBuf) -> Dial {
    Arc::new(move |entry: &Entry| {
        let path = endpoint::fleet_socket(&socket_dir, &entry.name);
        let stream =
            link::connect(&path, channel::API).map_err(|e| anyhow::anyhow!("its link isn't up ({}: {e})", path.display()))?;
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
        let socket = endpoint::fleet_socket(dir, name);
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
        let ours = (self.hello)();
        let spec = link::Spec { hello: ours, ..LINK };
        let wanted: Mutex<Option<String>> = Mutex::new(None);
        let mut end_link = duplex.stream.try_clone().ok().map(follow::shutting);
        let endpoint = link::Endpoint { socket: Some(&socket), serves, outlet: None };
        let end = link::answerer_with(
            &spec,
            reader,
            writer,
            endpoint,
            |peer| {
                let _ = duplex.stream.set_read_timeout(None);
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
                hello: update::hello_of(false, None),
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
        let socket = endpoint::fleet_socket(&pair.l_dir, "box");
        let bridge = |root: &Path| {
            let request = Request { root: root.to_path_buf(), instance: Some("web-1".into()), service: None, port: 3000 };
            let via = Via::Fleet { socket: socket.clone(), request };
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
    fn named(rows: &Value, name: &str) -> Option<Value> {
        rows.as_array()?.iter().find(|r| r["name"] == name).cloned()
    }

    /// From K's side, the dialed host, L is a machine (kind `inbound`) like
    /// any other: `machines.list` says connected with its link's version
    /// and its roots (its fleet root for K among them), `machines.get`
    /// answers it, a quick call and a forwarded verb run on L under L's
    /// trust for K (`box` there), K's federated snapshot has L's rows; its
    /// `agent` and `unlink` are refused here. L dropping its entry for K takes
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
        for (method, params) in [("machines.set", json!({"name": "lap", "agent": true})), ("machines.unlink", json!({"name": "lap"}))] {
            let e = k.call(method, params).unwrap().error.unwrap();
            assert!(e.code == "denied" && e.message.contains("links to this host from its end"), "{method}: {e:?}");
        }

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
        let (r, out, err) = verb(&mut k, "run", json!({"root": cfg, "env": {"PATH": path}, "sandbox": "web", "branch": "feature"}));
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

    /// A `run` typed on member L (`lap`, hub `box`) is placed by the hub K
    /// (`fleet.place`): L is over a cap, K has room, so L moves the code
    /// into its fleet root on K (as K's `roots.list` flags it) and forwards
    /// the run there, the hub's lines first.
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

    /// Member L's hub `box` can't be reached (its ssh fails): a `run` typed
    /// on L runs on L, its output starting with a note naming the hub.
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
        let (r, out, err) = verb(&mut connect(&pair.l_dir), "run", json!({"root": cfg, "env": {}, "sandbox": "web"}));
        assert!(r.error.is_none(), "{r:?} {err}");
        assert_eq!(r.result.as_ref().unwrap().get("machine"), None, "{r:?}");
        assert!(err.starts_with("note: the hub `box` is unreachable (") && err.contains("): running here\n"), "{err}");
        assert_eq!(out, format!("local --in-process -C {} run -- web\n", cfg.display()));
        pair.finish();
        let _ = std::fs::remove_dir_all(&l_verbs);
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
    /// snapshot shows K's rows and, through K, x's (`via: box`), never its
    /// own as K sees them; acting on x from L is refused naming K. K
    /// serves a federated watch only as the hub.
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

        // View-only: anything acting on x is refused naming the hub.
        let refused = crate::serve::snapshots::seen_via("x", "box");
        let calls = [
            ("instances.list", json!({"dir": x_root, "machine": "x"})),
            ("machines.get", json!({"name": "x"})),
            ("machines.set", json!({"name": "x", "agent": true})),
            ("forwards.add", json!({"root": x_root, "instance": "x-1", "port": 3000, "machine": "x"})),
        ];
        for (method, params) in calls {
            let e = connect(&pair.l_dir).call(method, params).unwrap().error.unwrap();
            assert_eq!((e.code.as_str(), e.message.as_str()), ("denied", refused.as_str()), "{method}");
        }
        let (r, ..) = verb(&mut connect(&pair.l_dir), "stop", json!({"root": x_root, "env": {}, "instance": "x-1", "machine": "x"}));
        let e = r.error.unwrap();
        assert_eq!((e.code.as_str(), e.message.as_str()), ("denied", refused.as_str()));
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
    /// reaches) and L (`lap`), `k_hub` / `l_hub` their `hub`s.
    fn fleet_pair(tag: &str, k_hub: &str, l_hub: &str) -> Pair {
        use crate::serve::fleet::member::{Member, write_for_test};
        let (k_hub, l_hub) = (k_hub.to_string(), l_hub.to_string());
        Pair::with(
            tag,
            vec![Entry::ssh("x", "me@x")],
            move |k| write_for_test(k.member.as_ref().unwrap(), &Member { name: "box".into(), hub: k_hub, ..Default::default() }),
            move |l| write_for_test(l.member.as_ref().unwrap(), &Member { name: "lap".into(), hub: l_hub, ..Default::default() }),
        )
    }

    /// `who`'s (`k` or `l`) `member.toml` hub, and its registry's names.
    fn member_and_registry(pair: &Pair, who: &str) -> (crate::serve::fleet::member::Hub, Vec<String>) {
        let data = pair.base.join(format!("{who}-data"));
        let hub = crate::serve::fleet::member::load(&data.join("member.toml")).unwrap().unwrap().hub();
        let names = crate::serve::machines::load(&data.join("machines.toml")).unwrap().into_iter().map(|e| e.name).collect();
        (hub, names)
    }

    fn lap_on_k(pair: &Pair) -> Option<Value> {
        named(&connect(&pair.k_dir).call("machines.list", Value::Null).unwrap().result.unwrap(), "lap")
    }

    /// The hub L removes member K: K hears `fleet.leave` over the link, so
    /// its `hub` is cleared and its own entries dropped; L unlinks it, and
    /// the link ends.
    #[test]
    fn the_hubs_rm_of_a_member_reaches_it() {
        use crate::serve::fleet::member::Hub;
        let pair = fleet_pair("dial-rm", "lap", "self");
        pair.wait_box("box connected", connected);
        let r = connect(&pair.l_dir).call("machines.rm", json!({"name": "box"})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let answer = r.result.unwrap();
        assert_eq!(answer, json!({"type": "removed", "name": "box", "hub": "lap", "leave": "told", "error": null, "forgot": [], "missed": []}));
        crate::serve::dts::assert_variant("MachineRmRemoved", &answer);
        assert_eq!(member_and_registry(&pair, "k"), (Hub::None, vec![]), "K left: no hub, no entries");
        assert_eq!(member_and_registry(&pair, "l"), (Hub::This, vec![]), "L unlinked it");
        wait_for("lap gone on K", Duration::from_secs(10), || lap_on_k(&pair).is_none());
        let e = connect(&pair.l_dir).call("machines.rm", json!({"name": "box"})).unwrap().error.unwrap();
        assert_eq!(e.code, "not-found");
        pair.finish();
    }

    /// `fleet.*` goes by who asks: from a peer that isn't this host's hub
    /// (or from a local client) `fleet.forget` / `fleet.leave` are refused
    /// and nothing changes; from the hub they run. The hub itself can't be
    /// removed by a member.
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
        assert_eq!((e.code.as_str(), e.message.as_str()), ("denied", "`box` is the hub: it leaves on its own host"));
        assert_eq!(member_and_registry(&pair, "k"), (Hub::This, vec!["x".to_string()]), "nothing changed on K");
        // From the member's own `machine rm`, too.
        let e = connect(&pair.l_dir).call("machines.rm", json!({"name": "box"})).unwrap().error.unwrap();
        assert_eq!(e.code, "denied");
        // K on L: L's hub, admitted.
        let mut k = api_to(&endpoint::fleet_socket(&pair.k_dir, "lap"), "lap");
        let r = k.conn.call("fleet.forget", json!({"name": "nobody"})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
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
                "type": "left", "name": "lap", "hub": "box", "error": null, "dropped": ["box"],
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
}
