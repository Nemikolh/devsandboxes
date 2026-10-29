//! The worker side of a fleet link (docs/architecture/daemon/fleet.md,
//! *Worker side*): while `lend.toml` says `on` or `drain`, the daemon's
//! `serve-lend` thread holds `ssh -T -o BatchMode=yes <hub> devsandbox
//! fleet-link --worker <name> --root <fleet root>` ([`link_argv`]) and runs the answerer of
//! `fleet::LINK` on its stdio. Each `API` stream the hub opens is answered
//! by the daemon's `open` (a socket pair: the API relay's sentinel on the
//! hub's end, the other end served as a `Peer` connection, `fleet::lent`).
//! A link that ends is retried with the machine links' backoff
//! (`service::backoff`), reset once a link came up. [`Lending::status`] is
//! the link's state for `lend.get`; [`Lending::set_draining`] changes what
//! the next link offers (`--drain`) without dropping the current one.
//! `PTY` streams (`fleet::pty`) are answered by the daemon's handler for
//! them, like `API` ones ([`Opens`]). `TRANSFER` streams the hub opens go
//! to the daemon's `Receivers`; [`Lending::open_transfer`] opens one toward
//! the hub while a link is up. While a link is up and serving (its hub
//! serves `AGENT`), its agent socket is bound in the daemon's socket dir
//! (`fleet::agent::serve`): the hub's agent for this host's bridges.
//!
//! Before it serves anything, each link compares the hub's build (its
//! `Hello`) with this one's (`update::decide`): another published build is
//! downloaded, installed and handed off to (`update::update`), the link
//! dropped meanwhile. Another dev build for this platform, with
//! `member.toml`'s `dev-builds` on (or trust `full`), keeps the link up serving no `API`
//! stream, only the `binary` transfer the hub pushes (`update::push`); once
//! it is in, the link is dropped and the binary installed and handed off to
//! (`update::install_received`). Any other dev build drops the link, saying
//! why. Either way the next attempt is after the backoff, so a failed
//! update is retried at the backoff's pace, never in a loop.
//!
//! The transport is injectable ([`Transport`]): production spawns ssh
//! (`ssh::spawn`), tests hand back one end of a socket pair whose other end
//! is the hub's `fleet-link` in-process.

use std::ffi::OsString;
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};

use super::agent;
use super::transfer::Receivers;
use super::update::{self, BINARY_KIND, BINARY_LIMIT, Build, Decision, Steps};
use super::{LINK, link};
use crate::devsbd::mux::Conn;
use crate::devsbd::proto::caps;
use crate::serve::endpoint::Stream;
use crate::serve::machines::check_name;
use crate::serve::machines::service::{self, Timing};
use crate::serve::machines::ssh::{self, Duplex};

/// How the worker reaches its hub: the argv in, a connection out (ssh's
/// stdio behind a socket, as `ssh::spawn` returns it).
pub type Transport = Arc<dyn Fn(&[OsString]) -> Result<Duplex> + Send + Sync>;

/// The production [`Transport`]: ssh.
pub fn ssh_transport() -> Transport {
    Arc::new(|argv: &[OsString]| ssh::spawn(argv))
}

/// How the daemon answers one stream the hub opens (`API`, `PTY`): the
/// hub's end of a new connection, `None` to refuse it.
pub type Open = Arc<dyn Fn() -> Option<Conn> + Send + Sync>;

/// The channels the daemon answers on each link, with their [`Open`]s
/// (`TRANSFER` aside: the lending thread's receivers take it). All refused
/// while the link waits for the hub's binary.
pub type Opens = Vec<(u8, Open)>;

/// How this worker follows its hub's build.
#[derive(Clone)]
pub struct Follow {
    /// The update's steps (`update::Release` in production).
    pub steps: Arc<dyn Steps>,
    /// `member.toml`'s `dev-builds` or trust `full`
    /// (`fleet::member::Member::takes_dev_builds`), read at each link's
    /// `Hello`s (a hand edit counts from the next attempt).
    pub dev_builds: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// How long a link waiting for the hub's binary waits for its transfer to
/// start; then it ends, retried after the backoff.
const PUSH_WAIT: Duration = Duration::from_secs(60);

/// `ssh -T -o BatchMode=yes [-p N] -- <hub> <devsandbox fleet-link --worker
/// <name> --root <root>>` through the hub's login shell, as every machine
/// argv is (`ssh::remote_command`). `BatchMode`: the daemon has no TTY.
/// `root` is the canonical fleet root: the offer registers it as the
/// offered entry's one root, which the hub watches. `draining` adds
/// `--drain`: the offer says this worker takes no new work.
pub fn link_argv(hub: &str, name: &str, root: &Path, draining: bool) -> Result<Vec<OsString>> {
    check_name(name)?;
    let root = root.to_str().with_context(|| format!("fleet root {} is not UTF-8", root.display()))?;
    let mut args = vec!["fleet-link", "--worker", name, "--root", root];
    if draining {
        args.push("--drain");
    }
    ssh::target_argv(hub, &["-T", "-o", "BatchMode=yes"], "devsandbox", &args)
}

/// The link's state, for `lend.get`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The first attempt hasn't come up (or failed) yet.
    Connecting,
    /// The `Hello`s are through.
    Connected,
    /// The last attempt failed or the link ended: why. The next one is
    /// after the backoff.
    Retrying(String),
}

/// The lending thread. Dropping it stops it: the link's stream is shut
/// (which ends the answerer) and the thread joined; the ssh dies with its
/// `Duplex`.
pub struct Lending {
    ctl: Arc<Ctl>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Ctl {
    stopped: Mutex<bool>,
    wake: Condvar,
    /// The live link's stream, to shut on stop.
    current: Mutex<Option<Stream>>,
    /// What the next link offers: `--drain`.
    draining: AtomicBool,
    status: Mutex<Option<Status>>,
    /// Who takes the hub's `TRANSFER` streams: the daemon's receivers (and
    /// the `binary` one, [`Incoming`]).
    receivers: Option<Arc<Receivers>>,
    /// The live link's way to open streams toward the hub.
    outlet: link::Outlet,
    /// How this worker follows its hub's version.
    follow: Option<Follow>,
    incoming: Arc<Incoming>,
    /// The socket dir the link's agent socket goes in; `None`: none bound.
    agent_dir: Option<PathBuf>,
}

/// The hub's binary on its way (`Decision::Receive`): what the `binary`
/// receiver takes, for the one link waiting for it.
#[derive(Default)]
struct Incoming {
    want: Mutex<Option<Want>>,
    wake: Condvar,
    seq: std::sync::atomic::AtomicU64,
}

struct Want {
    seq: u64,
    /// The hub's `Hello` hash: what the binary must report.
    hello: String,
    /// The private scratch dir it lands in.
    dir: PathBuf,
    /// The link's stream, shut once the wait is over (received or not).
    link: Option<Stream>,
    /// Its transfer's header was taken: no other is.
    started: bool,
    received: Option<PathBuf>,
    /// Why it didn't come.
    why: Option<String>,
}

impl Want {
    fn waiting(&self) -> bool {
        !self.started && self.received.is_none() && self.why.is_none()
    }
}

impl Incoming {
    /// Wait for the hub's binary on the link `link`; the wait's `seq`.
    fn expect(&self, hello: String, dir: PathBuf, link: Option<Stream>) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        *self.want.lock().unwrap() = Some(Want { seq, hello, dir, link, started: false, received: None, why: None });
        seq
    }

    /// End the current wait with `f`: its link shut (which ends the link).
    fn finish(&self, f: impl FnOnce(&mut Want)) {
        if let Some(w) = self.want.lock().unwrap().as_mut() {
            f(w);
            if let Some(link) = &w.link {
                let _ = link.shutdown(Shutdown::Both);
            }
        }
        self.wake.notify_all();
    }

    /// The wait, over (its link ended).
    fn take(&self) -> Option<Want> {
        let want = self.want.lock().unwrap().take();
        self.wake.notify_all();
        want
    }

    /// After `wait` without the transfer starting, wait `seq` ends with why.
    fn watch(self: &Arc<Self>, seq: u64, wait: Duration) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("serve-lend-push".into()).spawn(move || {
            let current = |w: &Option<Want>| w.as_ref().is_some_and(|w| w.seq == seq && w.waiting());
            let guard = me.want.lock().unwrap();
            let (guard, _) = me.wake.wait_timeout_while(guard, wait, |w| current(w)).unwrap();
            if current(&guard) {
                drop(guard);
                me.finish(|w| w.why = Some(format!("the hub sent no binary within {wait:?}")));
            }
        });
    }

    /// `receivers` taking the hub's binary ([`BINARY_KIND`]) into the
    /// current wait's dir, once; with no wait, refused at the header.
    fn receiver(self: &Arc<Self>, receivers: Receivers) -> Receivers {
        let (take, done, failed) = (Arc::clone(self), Arc::clone(self), Arc::clone(self));
        receivers
            .with(
                BINARY_KIND,
                BINARY_LIMIT,
                move |_| {
                    let mut want = take.want.lock().unwrap();
                    match want.as_mut() {
                        Some(w) if w.waiting() => {
                            w.started = true;
                            take.wake.notify_all();
                            Ok(w.dir.join("devsandbox"))
                        }
                        _ => anyhow::bail!("this worker isn't waiting for a binary"),
                    }
                },
                move |r| done.finish(|w| w.received = Some(r.path)),
            )
            .on_failure(BINARY_KIND, move |why| failed.finish(|w| w.why = Some(format!("the hub's binary didn't arrive: {why}"))))
    }
}

impl Lending {
    /// Start lending to `hub` as `name`, offering fleet root `root`
    /// (`draining`: offering `--drain`); the hub's streams on `opens`'
    /// channels go to their handlers, its `TRANSFER` streams to
    /// `receivers`, plus the hub's binary; a hub on another build is
    /// followed through `follow`. Each serving link binds its agent socket
    /// in `agent_dir` (`fleet::agent::socket`).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        hub: String,
        name: String,
        root: PathBuf,
        draining: bool,
        transport: Transport,
        opens: Opens,
        receivers: Receivers,
        agent_dir: Option<PathBuf>,
        follow: Follow,
        timing: Timing,
        log: fn(&str),
    ) -> Result<Lending> {
        link_argv(&hub, &name, &root, draining)?;
        let incoming = Arc::new(Incoming::default());
        let receivers = Some(Arc::new(incoming.receiver(receivers)));
        let ctl = Arc::new(Ctl { receivers, follow: Some(follow), incoming, agent_dir, ..Ctl::default() });
        ctl.draining.store(draining, Ordering::SeqCst);
        let thread = {
            let ctl = Arc::clone(&ctl);
            std::thread::Builder::new()
                .name("serve-lend".into())
                .spawn(move || lend_loop(&hub, &name, &root, &transport, &opens, &timing, &ctl, log))
                .context("cannot start the lending thread")?
        };
        Ok(Lending { ctl, thread: Some(thread) })
    }

    pub fn status(&self) -> Status {
        self.ctl.status.lock().unwrap().clone().unwrap_or(Status::Connecting)
    }

    /// What the next link offers; the current one stays up (the hub learns
    /// it when the link is next made).
    pub fn set_draining(&self, draining: bool) {
        self.ctl.draining.store(draining, Ordering::SeqCst);
    }

    /// Whether it's draining now (its last [`set_draining`](Self::set_draining),
    /// or how it started).
    pub fn draining(&self) -> bool {
        self.ctl.draining.load(Ordering::SeqCst)
    }

    /// A new `TRANSFER` stream to the hub (for `transfer::send`); an error
    /// while no link is up.
    #[allow(dead_code)] // first caller: fetch-back (fleet step 12)
    pub fn open_transfer(&self) -> Result<Conn> {
        self.ctl.outlet.open(crate::devsbd::proto::channel::TRANSFER)
    }
}

impl Drop for Lending {
    fn drop(&mut self) {
        *self.ctl.stopped.lock().unwrap() = true;
        self.ctl.wake.notify_all();
        if let Some(s) = self.ctl.current.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Ctl {
    fn stopped(&self) -> bool {
        *self.stopped.lock().unwrap()
    }

    fn set_status(&self, status: Status) {
        *self.status.lock().unwrap() = Some(status);
    }

    /// Sleep `d` unless stopped first; `false` once stopped.
    fn wait(&self, d: Duration) -> bool {
        let guard = self.stopped.lock().unwrap();
        let (guard, _) = self.wake.wait_timeout_while(guard, d, |stopped| !*stopped).unwrap();
        !*guard
    }
}

#[allow(clippy::too_many_arguments)]
fn lend_loop(hub: &str, name: &str, root: &Path, transport: &Transport, opens: &Opens, timing: &Timing, ctl: &Ctl, log: fn(&str)) {
    let mut failures = 0;
    while !ctl.stopped() {
        let argv = match link_argv(hub, name, root, ctl.draining.load(Ordering::SeqCst)) {
            Ok(argv) => argv,
            Err(e) => {
                // Checked by `spawn`: unreachable short of a bug.
                log(&format!("fleet: {e:#}"));
                return;
            }
        };
        match transport(&argv) {
            Err(e) => {
                log(&format!("fleet: cannot reach hub {hub}: {e:#}"));
                ctl.set_status(Status::Retrying(format!("{e:#}")));
            }
            Ok(duplex) => {
                if run_link(hub, name, duplex, opens, ctl, log) {
                    failures = 0;
                }
            }
        }
        if !ctl.wait(service::backoff(timing, &mut failures)) {
            break;
        }
    }
}

/// One link over `duplex` until it ends; `true` if it came up.
fn run_link(hub: &str, name: &str, mut duplex: Duplex, opens: &Opens, ctl: &Ctl, log: fn(&str)) -> bool {
    let (Ok(reader), Ok(writer), Ok(killer)) = (duplex.stream.try_clone(), duplex.stream.try_clone(), duplex.stream.try_clone())
    else {
        log("fleet: cannot clone the link's socket");
        return false;
    };
    {
        let mut current = ctl.current.lock().unwrap();
        if ctl.stopped() {
            return false;
        }
        *current = duplex.stream.try_clone().ok();
    }
    let up = AtomicBool::new(false);
    let wanted: Mutex<Option<String>> = Mutex::new(None);
    // While the hub's binary is on its way the link serves no `API` (or
    // `PTY`) stream, nor binds its agent socket: nothing reaches the
    // `Worker` table, nor an instance, until the update is done.
    let receiving = AtomicBool::new(false);
    let mut link_stream = duplex.stream.try_clone().ok();
    let mut serves = link::Serves::default();
    for (channel, open) in opens {
        let receiving = &receiving;
        serves = serves.with(*channel, move || if receiving.load(Ordering::Relaxed) { None } else { open() });
    }
    if let Some(receivers) = &ctl.receivers {
        serves = serves.transfers(Arc::clone(receivers));
    }
    // Set once the link ended: the agent socket's thread unbinds.
    let over = AtomicBool::new(false);
    let end = std::thread::scope(|s| {
        let end = link::answerer_with(
            &LINK,
            reader,
            writer,
            &serves,
            Some(&ctl.outlet),
            |peer| {
                let theirs = Build::parse(&peer.hash);
                let dev_builds = ctl.follow.as_ref().is_some_and(|f| (f.dev_builds)());
                let platform = update::platform_of(&peer.hash);
                match update::decide(&Build::current(), &theirs, update::PLATFORM, platform, dev_builds) {
                    Decision::Same => {}
                    Decision::Download { version } => {
                        *wanted.lock().unwrap() = Some(version);
                        return Err(format!("the hub runs {}: updating", theirs.label()));
                    }
                    Decision::Stop(why) => return Err(why),
                    Decision::Receive => {
                        let dir = update::scratch_dir(&std::env::temp_dir()).map_err(|e| format!("cannot take the hub's build: {e:#}"))?;
                        receiving.store(true, Ordering::Relaxed);
                        let seq = ctl.incoming.expect(peer.hash.clone(), dir, link_stream.take());
                        ctl.incoming.watch(seq, PUSH_WAIT);
                        log(&format!("fleet: hub {hub} runs devsandbox {}: receiving its binary", theirs.label()));
                        ctl.set_status(Status::Retrying(format!("receiving the hub's build {}", theirs.label())));
                        return Ok(());
                    }
                }
                up.store(true, Ordering::Relaxed);
                ctl.set_status(Status::Connected);
                log(&format!("fleet: lending to {hub} as {name}"));
                if let Some(dir) = &ctl.agent_dir
                    && peer.caps & caps::AGENT != 0
                {
                    let (path, outlet, over) = (agent::socket(dir, hub), &ctl.outlet, &over);
                    let spawned = std::thread::Builder::new().name("serve-lend-agent".into()).spawn_scoped(s, move || {
                        if let Err(e) = agent::serve(&path, outlet, over, agent::PROBE) {
                            log(&format!("fleet: {e:#}"));
                        }
                    });
                    if let Err(e) = spawned {
                        log(&format!("fleet: cannot start the agent socket's thread: {e}"));
                    }
                }
                Ok(())
            },
            move || {
                let _ = killer.shutdown(Shutdown::Both);
            },
        );
        over.store(true, Ordering::Relaxed);
        end
    });
    ctl.current.lock().unwrap().take();
    if let Some(want) = ctl.incoming.take() {
        let label = Build::parse(&want.hello).label();
        match (&want.received, ctl.stopped()) {
            (_, true) => {}
            // The ssh goes first: nothing is served while this binary changes.
            (Some(binary), false) => {
                duplex.peer.close();
                follow_dev(&label, &want.hello, binary, ctl, log);
            }
            (None, false) => {
                let why = want.why.clone().unwrap_or_else(|| format!("{} before the hub's binary arrived", end_why(end, &mut duplex)));
                let why = format!("update to {label} failed: {why}");
                log(&format!("fleet: {why}"));
                ctl.set_status(Status::Retrying(why));
            }
        }
        let _ = std::fs::remove_dir_all(&want.dir);
        duplex.peer.close();
        return false;
    }
    let wanted = wanted.into_inner().unwrap();
    if let (Some(version), false) = (wanted, ctl.stopped()) {
        // The ssh goes first: nothing is served while this binary changes.
        duplex.peer.close();
        follow(hub, &version, ctl, log);
        return false;
    }
    if !ctl.stopped() {
        let why = end_why(end, &mut duplex);
        log(&format!("fleet: link to {hub} ended: {why}"));
        ctl.set_status(Status::Retrying(why));
    }
    duplex.peer.close();
    up.load(Ordering::Relaxed)
}

/// Why the link ended, with ssh's stderr tail.
fn end_why(end: link::End, duplex: &mut Duplex) -> String {
    let why = match end {
        link::End::Handshake(why) | link::End::Refused(why) => why,
        link::End::Dead => "no frame from the hub: link dead".into(),
        link::End::Closed => "link closed".into(),
    };
    let tail = duplex.peer.stderr_tail(Duration::from_millis(500));
    format!("{why}{}", if tail.is_empty() { String::new() } else { format!(" (ssh: {tail})") })
}

/// Install the hub's dev build `hello` (`label` for messages), received at
/// `binary`, and hand off to it (`update::install_received`); the outcome
/// is the status `lend.get` shows until the daemon hands off or the next
/// attempt.
fn follow_dev(label: &str, hello: &str, binary: &Path, ctl: &Ctl, log: fn(&str)) {
    let Some(follow) = &ctl.follow else { return };
    ctl.set_status(Status::Retrying(format!("updating to {label}")));
    let why = match update::install_received(follow.steps.as_ref(), binary, hello) {
        Ok(()) => format!("updated to {label}: handing off"),
        Err(e) => format!("update to {label} failed: {e:#}"),
    };
    log(&format!("fleet: {why}"));
    ctl.set_status(Status::Retrying(why));
}

/// Install the hub's published `version` and hand off to it
/// (`update::update`); the outcome is the status `lend.get` shows until the
/// daemon hands off or the next attempt.
fn follow(hub: &str, version: &str, ctl: &Ctl, log: fn(&str)) {
    let Some(Follow { steps, .. }) = &ctl.follow else { return };
    log(&format!("fleet: hub {hub} runs devsandbox {version}: updating this one"));
    ctl.set_status(Status::Retrying(format!("updating to {version}")));
    let consts = (std::env::consts::OS, std::env::consts::ARCH);
    let why = match update::update(steps.as_ref(), version, consts.0, consts.1, &std::env::temp_dir()) {
        Ok(()) => format!("updated to {version}: handing off"),
        Err(e) => format!("update to {version} failed: {e:#}"),
    };
    log(&format!("fleet: {why}"));
    ctl.set_status(Status::Retrying(why));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::fleet::update::tests::Fake;

    /// What a test hub pushes: `None` never gets a binary.
    type Binary = Arc<dyn Fn() -> std::io::Result<PathBuf> + Send + Sync>;

    fn no_binary() -> Binary {
        Arc::new(|| Err(std::io::Error::other("no binary here")))
    }

    /// A transport whose every link is a hub `fleet-link` saying `hello` in
    /// its `Hello` (its opener in-process, its sockets under `dir`), pushing
    /// `binary()`'s file to the worker as `fleet_link::run` does
    /// (`fleet_link::push_build`, when `update::hub_pushes`). Its `AGENT`
    /// streams are echoed, standing in for the hub's agent.
    fn hub_pushing(hello: &'static str, dir: PathBuf, binary: Binary) -> Transport {
        Arc::new(move |_: &[OsString]| {
            let (worker, hub) = std::os::unix::net::UnixStream::pair()?;
            let (socket, xfer, pty) =
                (dir.join("fleet-lap.sock"), dir.join("fleet-lap.transfer.sock"), dir.join("fleet-lap.pty.sock"));
            let binary = Arc::clone(&binary);
            std::thread::spawn(move || {
                let spec = link::Spec { hello, ..LINK };
                use crate::devsbd::proto::channel;
                let echo = || {
                    let (ours, theirs) = std::os::unix::net::UnixStream::pair().ok()?;
                    std::thread::spawn(move || {
                        // Serving (`agent::SERVING`), then the echo.
                        let _ = std::io::Write::write_all(&mut &ours, &[super::agent::SERVING]);
                        let _ = std::io::copy(&mut &ours, &mut &ours);
                    });
                    Some(theirs.into())
                };
                let serves = link::Serves::default().transfers(Arc::new(Receivers::new(|_| {}))).with(channel::AGENT, echo);
                let (r, w) = (hub.try_clone().unwrap(), hub);
                let sockets = [(spec.channel, socket.as_path()), (channel::TRANSFER, xfer.as_path()), (channel::PTY, pty.as_path())];
                let push_to = xfer.clone();
                link::opener_with(&spec, r, w, &sockets, serves, |peer| {
                    crate::commands::fleet_link::push_build(hello, &peer.hash, push_to, move || binary());
                    Ok(())
                })
            });
            Ok(Duplex::socket(worker))
        })
    }

    /// A lending thread to an in-process hub (`hub_pushing`) in a fresh
    /// `dir`; every `API` and `PTY` stream the worker answers is counted
    /// (`opens`).
    struct Lent {
        lending: Lending,
        dir: PathBuf,
        opens: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Drop for Lent {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn lend_to(hello: &'static str, tag: &str, steps: Arc<Fake>, dev_builds: bool, binary: Binary) -> Lent {
        let dir = std::env::temp_dir().join(format!("dsv-worker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let timing = Timing { backoff: Duration::from_millis(5), backoff_cap: Duration::from_millis(10), ..Timing::default() };
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = |opens: &Arc<std::sync::atomic::AtomicUsize>| -> Open {
            let counted = Arc::clone(opens);
            Arc::new(move || -> Option<Conn> {
                counted.fetch_add(1, Ordering::SeqCst);
                None
            })
        };
        let lending = Lending::spawn(
            "hub".into(),
            "lap".into(),
            "/fleet/hub".into(),
            false,
            hub_pushing(hello, dir.clone(), binary),
            vec![(LINK.channel, counter(&opens)), (crate::devsbd::proto::channel::PTY, counter(&opens))],
            Receivers::new(|_| {}),
            Some(dir.clone()),
            Follow { steps, dev_builds: Arc::new(move || dev_builds) },
            timing,
            |_| {},
        )
        .unwrap();
        Lent { lending, dir, opens }
    }

    fn wait_status(lending: &Lending, pred: impl Fn(&str) -> bool) -> String {
        let start = std::time::Instant::now();
        loop {
            if let Status::Retrying(why) = lending.status()
                && pred(&why)
            {
                return why;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "status: {:?}", lending.status());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A hub on this build that serves `AGENT`: while the link is up its
    /// agent socket is here, each connection a stream to the hub's agent.
    #[test]
    fn the_hubs_agent_is_bound_here_while_the_link_serves() {
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        let lent = lend_to(LINK.hello, "agent", Arc::new(Fake::default()), false, no_binary());
        let socket = agent::socket(&lent.dir, "hub");
        assert_eq!(socket.file_name().unwrap(), "agent-fleet-hub.sock");
        let start = std::time::Instant::now();
        while !socket.exists() {
            assert!(start.elapsed() < Duration::from_secs(10), "bound");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lent.lending.status(), Status::Connected);
        assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(crate::serve::endpoint::agent_sockets(&lent.dir), [socket.clone()], "what `bridge::host_agent` tries");
        let mut c = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(b"sign this").unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"sign this", "the hub's echo, its end passed on");
    }

    fn has_release() -> bool {
        update::release_target(std::env::consts::OS, std::env::consts::ARCH).is_some()
    }

    /// A hub on another published build: the worker installs it and asks
    /// for the handoff, a downgrade included; nothing is served meanwhile.
    #[test]
    fn a_published_hub_build_is_installed_and_handed_off_to() {
        let steps = Arc::new(Fake::reporting("0.0.1"));
        let lent = lend_to("0.0.1 0123abcd published", "follow", Arc::clone(&steps), false, no_binary());
        if has_release() {
            assert_eq!(wait_status(&lent.lending, |w| w.starts_with("updated")), "updated to 0.0.1: handing off");
            assert!(steps.calls().iter().any(|c| c == "handoff "), "{:?}", steps.calls());
            assert!(steps.calls()[0].contains("/releases/download/v0.0.1/devsandbox-0.0.1-"));
        } else {
            wait_status(&lent.lending, |w| w.contains("no devsandbox release is built for"));
        }
        assert_eq!(lent.opens.load(Ordering::SeqCst), 0);
    }

    /// A failed download stops offering with why, and is retried at the
    /// backoff's pace on the next link.
    #[test]
    fn a_failed_update_stops_offering_and_is_retried() {
        let steps = Arc::new(Fake { fail: Some("fetch"), ..Fake::reporting("0.0.1") });
        let lent = lend_to("0.0.1 0123abcd published", "fail", Arc::clone(&steps), false, no_binary());
        let why = wait_status(&lent.lending, |w| w.starts_with("update to 0.0.1 failed: "));
        if has_release() {
            assert!(why.ends_with("fetch broke"), "{why}");
            let start = std::time::Instant::now();
            while steps.calls().len() < 2 {
                assert!(start.elapsed() < Duration::from_secs(10), "retried");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(!steps.calls().iter().any(|c| c.starts_with("install") || c.starts_with("handoff")));
    }

    /// A hub's dev build for this platform: `hub` is its `Hello` hash.
    fn dev_hub() -> &'static str {
        Box::leak(format!("0.0.1 0123abcd dev {}", update::PLATFORM).into_boxed_str())
    }

    /// A hub binary under `dir`, written when `binary()` is called.
    fn hub_binary(dir: PathBuf, gate: Option<std::sync::mpsc::Receiver<()>>) -> Binary {
        let gate = Mutex::new(gate);
        Arc::new(move || {
            if let Some(gate) = gate.lock().unwrap().as_ref() {
                let _ = gate.recv();
            }
            let path = dir.join("hub-exe");
            std::fs::write(&path, b"the hub's build")?;
            Ok(path)
        })
    }

    /// A hub on another dev build for this platform, `dev-builds` on: the
    /// link serves no `API` stream while the hub's binary comes over
    /// `TRANSFER`; then it is checked, installed and handed off to.
    #[test]
    fn a_dev_hub_build_is_received_installed_and_handed_off_to() {
        let hub = dev_hub();
        let steps = Arc::new(Fake::building(hub));
        let (go, gate) = std::sync::mpsc::channel();
        let base = std::env::temp_dir().join(format!("dsv-worker-devbin-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let lent = lend_to(hub, "dev-ok", Arc::clone(&steps), true, hub_binary(base.clone(), Some(gate)));
        wait_status(&lent.lending, |w| w == "receiving the hub's build 0.0.1 (dev, 0123abcd)");

        // The trust table is out of reach: an `API` stream is refused, and
        // so is a `PTY` one.
        for sock in ["fleet-lap.sock", "fleet-lap.pty.sock"] {
            let mut probe = std::os::unix::net::UnixStream::connect(lent.dir.join(sock)).unwrap();
            probe.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            assert_eq!(std::io::Read::read(&mut probe, &mut [0u8; 16]).unwrap(), 0, "{sock} closed: refused");
        }
        assert!(!agent::socket(&lent.dir, "hub").exists(), "no agent socket while receiving");
        assert_eq!(lent.opens.load(Ordering::SeqCst), 0);
        assert!(steps.calls().is_empty(), "nothing before the binary");

        go.send(()).unwrap();
        assert_eq!(wait_status(&lent.lending, |w| w.starts_with("updated")), "updated to 0.0.1 (dev, 0123abcd): handing off");
        assert_eq!(steps.calls(), ["build received", "install /dest/devsandbox", "build dest", "handoff "]);
        assert_eq!(lent.opens.load(Ordering::SeqCst), 0);
        drop(lent);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The hub's binary reporting another build: never installed; the next
    /// link receives it again (after the backoff).
    #[test]
    fn a_received_build_that_isnt_the_hubs_is_refused_and_retried() {
        let hub = dev_hub();
        let steps = Arc::new(Fake { received_build: "0.0.1 ffff dev x/y".into(), ..Fake::building(hub) });
        let base = std::env::temp_dir().join(format!("dsv-worker-devbad-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let lent = lend_to(hub, "dev-bad", Arc::clone(&steps), true, hub_binary(base.clone(), None));
        let why = wait_status(&lent.lending, |w| w.starts_with("update to 0.0.1 (dev, 0123abcd) failed: "));
        assert!(why.contains("the hub's binary (") && why.contains("reports \"0.0.1 ffff dev x/y\""), "{why}");
        let start = std::time::Instant::now();
        while steps.calls().len() < 2 {
            assert!(start.elapsed() < Duration::from_secs(10), "retried");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(steps.calls().iter().all(|c| c == "build received"), "{:?}", steps.calls());
        assert_eq!(lent.opens.load(Ordering::SeqCst), 0);
        drop(lent);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A hub on another dev build, `dev-builds` off: no update, the reason
    /// shown.
    #[test]
    fn a_dev_hub_build_without_dev_builds_stops_offering() {
        let steps = Arc::new(Fake::building(dev_hub()));
        let lent = lend_to(dev_hub(), "dev-off", Arc::clone(&steps), false, no_binary());
        let why = wait_status(&lent.lending, |w| w.starts_with("dev build"));
        assert_eq!(
            why,
            "dev build: not taken (`dev-builds = true` in member.toml or `devsandbox machine trust full` takes it; the hub runs 0.0.1 (dev, 0123abcd))"
        );
        assert!(steps.calls().is_empty());
    }

    /// A hub's dev build for another platform: no update, whatever
    /// `dev-builds` says.
    #[test]
    fn a_dev_hub_build_for_another_platform_stops_offering() {
        let steps = Arc::new(Fake::building("x"));
        let lent = lend_to("0.0.1 0123abcd dev plan9/mips", "dev-plat", Arc::clone(&steps), true, no_binary());
        let why = wait_status(&lent.lending, |w| w.starts_with("dev build"));
        assert_eq!(why, format!("dev build: can't run on {} (the hub runs 0.0.1 (dev, 0123abcd) for plan9/mips)", update::PLATFORM));
        assert!(steps.calls().is_empty());
    }

    /// The `binary` receiver takes one transfer, only while a link waits
    /// for it; a wait whose transfer never starts ends with why, its link
    /// shut.
    #[test]
    fn the_binary_receiver_takes_one_while_waited_for() {
        use crate::serve::fleet::transfer::{self, Header};
        let dir = std::env::temp_dir().join(format!("dsv-worker-incoming-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let incoming = Arc::new(Incoming::default());
        let receivers = Arc::new(incoming.receiver(Receivers::new(|_| {})));
        let (size, sha256) = transfer::digest(&b"bin"[..]).unwrap();
        let header = Header { kind: BINARY_KIND.into(), name: "devsandbox".into(), size, sha256, to: None };
        let err = transfer::send(receivers.accept().unwrap(), &header, &b"bin"[..]).unwrap_err();
        assert_eq!(format!("{err:#}"), "transfer refused: this worker isn't waiting for a binary");

        let (link, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        incoming.expect("h".into(), dir.clone(), Some(link.into()));
        transfer::send(receivers.accept().unwrap(), &header, &b"bin"[..]).unwrap();
        let err = transfer::send(receivers.accept().unwrap(), &header, &b"bin"[..]).unwrap_err();
        assert_eq!(format!("{err:#}"), "transfer refused: this worker isn't waiting for a binary", "once");
        let start = std::time::Instant::now();
        while incoming.want.lock().unwrap().as_ref().unwrap().received.is_none() {
            assert!(start.elapsed() < Duration::from_secs(5), "received");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(std::io::Read::read(&mut &peer, &mut [0u8; 1]).unwrap(), 0, "the link is shut");
        let want = incoming.take().unwrap();
        assert_eq!(want.received, Some(dir.join("devsandbox")));
        assert_eq!(std::fs::read(dir.join("devsandbox")).unwrap(), b"bin");

        let (link, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let seq = incoming.expect("h".into(), dir.clone(), Some(link.into()));
        incoming.watch(seq, Duration::from_millis(20));
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(std::io::Read::read(&mut &peer, &mut [0u8; 1]).unwrap(), 0, "shut by the watch");
        assert_eq!(incoming.take().unwrap().why.as_deref(), Some("the hub sent no binary within 20ms"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_link_argv() {
        let root = Path::new("/d/fleet/me@hub");
        let argv = link_argv("ssh://me@hub:2200", "lap", root, false).unwrap();
        let want = [
            "ssh",
            "-T",
            "-o",
            "BatchMode=yes",
            "-p",
            "2200",
            "--",
            "me@hub",
            r#"exec "$SHELL" -lc 'exec devsandbox '\''fleet-link'\'' '\''--worker'\'' '\''lap'\'' '\''--root'\'' '\''/d/fleet/me@hub'\'''"#,
        ];
        assert_eq!(argv, want.map(OsString::from));
        assert_eq!(link_argv("hub", "lap", root, false).unwrap()[4..6], [OsString::from("--"), "hub".into()]);
        let drain = link_argv("hub", "lap", root, true).unwrap();
        assert!(drain[6].to_string_lossy().ends_with(r#"'\''--drain'\'''"#), "{drain:?}");
        assert!(link_argv("-oProxyCommand=x", "lap", root, false).is_err());
        assert!(link_argv("hub", "a/b", root, false).is_err());
    }

    mod e2e {
        use std::collections::BTreeMap;
        use std::io::{BufRead, BufReader, Write as _};
        use std::os::unix::net::UnixStream;
        use std::path::{Path, PathBuf};
        use std::time::Instant;

        use serde_json::{Value, json};

        use super::*;
        use crate::serve::client::{self, Hello};
        use crate::serve::daemon::Exit;
        use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
        use crate::serve::endpoint;
        use crate::serve::fleet::lend::{self, Lend};
        use crate::serve::fleet::member;
        use crate::serve::machines::{self, Entry, link as mlink};
        use crate::serve::proto::{HelloResult, Version};

        fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
            let start = Instant::now();
            while !cond() {
                assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting: {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// A raw connection past a hello as `version`, retrying while the
        /// daemon binds.
        fn stream_at(dir: &Path) -> endpoint::Stream {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(mut stream) = endpoint::connect(dir) {
                    match client::hello(&mut stream, &Version { semver: "0.0.0".into(), build: 0 }, "test").unwrap() {
                        Hello::Ready(_) => return stream,
                        other => panic!("{other:?}"),
                    }
                }
                assert!(Instant::now() < deadline, "daemon never listened");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn conn_at(dir: &Path) -> client::Conn {
            let reply = HelloResult { version: "0.0.0".into(), build: 0, protocol: 1, handoff: false };
            client::Conn::new(stream_at(dir), reply).unwrap()
        }

        /// A transport whose every link is the hub's `fleet-link` for `lap`
        /// offering `root` in-process, its socket and daemon in `hub` (which
        /// it asks for the agent, `fleet.agent`).
        fn in_process_hub(hub: PathBuf, root: PathBuf) -> Transport {
            Arc::new(move |argv: &[OsString]| {
                let words: Vec<_> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
                assert_eq!(&words[..5], ["ssh", "-T", "-o", "BatchMode=yes", "--"]);
                assert_eq!(words[5], "me@hub");
                assert!(words[6].contains("fleet-link") && words[6].contains("lap"), "{}", words[6]);
                assert!(words[6].contains(&format!("'\\''--root'\\'' '\\''{}'\\''", root.display())), "{}", words[6]);
                let (worker, hub_io) = UnixStream::pair()?;
                let (hub, root) = (hub.clone(), root.clone());
                std::thread::spawn(move || {
                    let (r, w) = (hub_io.try_clone().unwrap(), hub_io);
                    let asked = hub.clone();
                    let agent = move || crate::commands::fleet_link::agent_of(&mut conn_at(&asked), "lap");
                    crate::commands::fleet_link::run(r, w, &hub.clone(), "lap", &root, false, move || Ok(conn_at(&hub)), agent)
                });
                Ok(Duplex::socket(worker))
            })
        }

        /// A fake ssh-agent at `dir/agent.sock`: echoes each connection.
        fn echo_agent(dir: &Path) -> PathBuf {
            let path = dir.join("agent.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { return };
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut &conn, &mut &conn);
                    });
                }
            });
            path
        }

        /// `sign` through the agent socket at `path` (half-closed after):
        /// what came back, empty when the stream was refused.
        fn agent_through(path: &Path) -> Vec<u8> {
            use std::io::Read;
            let Ok(mut c) = UnixStream::connect(path) else { return Vec::new() };
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = c.write_all(b"sign");
            let _ = c.shutdown(Shutdown::Write);
            let mut got = Vec::new();
            let _ = c.read_to_end(&mut got);
            got
        }

        /// A raw stream over the hub's offered dial to `lap`, past the
        /// API relay's sentinel line.
        fn offered_stream(hub_dir: &Path) -> endpoint::Stream {
            let dial = crate::serve::fleet::offer::dial(hub_dir.to_path_buf(), Arc::new(|_: &Entry| unreachable!()));
            let duplex = dial(&Entry::offered("lap")).unwrap();
            let mut reader = BufReader::new(duplex.stream.try_clone().unwrap());
            duplex.stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut line = String::new();
            while line != "devsandbox-api 1\n" {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0, "no sentinel");
            }
            assert!(reader.buffer().is_empty());
            duplex.stream
        }

        /// A worker lending to an in-process hub: its offer registers it,
        /// and through the hub's offered dial the trust table (`worker`)
        /// answers in the fleet root and the worker's own roots, nowhere
        /// else.
        #[test]
        fn a_lending_worker_serves_its_hub_the_trust_table() {
            let base = scratch("fleet-lend");
            let (worker_dir, hub_dir) = (base.join("w"), base.join("hub"));
            let data = base.join("worker-data");
            let fleet_root = lend::fleet_root(&data, "me@hub").unwrap();
            let cfg = fleet_root.join("cfg");
            std::fs::create_dir_all(&cfg).unwrap();
            std::fs::write(cfg.join(crate::config::CONFIG_FILE), "[sandbox.web]\nimage = \"a\"\n").unwrap();
            let mine = base.join("mine");
            std::fs::create_dir_all(&mine).unwrap();
            std::fs::write(mine.join(crate::config::CONFIG_FILE), "[sandbox.web]\nimage = \"a\"\n").unwrap();
            let (cfg, mine) = (std::fs::canonicalize(&cfg).unwrap(), std::fs::canonicalize(&mine).unwrap());
            let nested = mine.join("nested");
            std::fs::create_dir_all(&nested).unwrap();
            let inst = |name: &str, dir: &Path| {
                format!(
                    "[instance.{name}]\nsandbox = \"web\"\ncontainer = \"devsandbox-{name}\"\nfolder = \"/w\"\n\
                     workspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                    dir.to_str().unwrap()
                )
            };
            let worker_state = data.join("state.toml");
            std::fs::write(&worker_state, inst("web-w", &cfg) + &inst("own", &mine)).unwrap();
            let lend_path = data.join("lend.toml");
            lend::write_for_test(&lend_path, &Lend { state: lend::State::On, hub: "me@hub".into() });
            member::write_for_test(&data.join("member.toml"), &member::Member { name: "lap".into(), ..Default::default() });

            let (runner, verb_dir) =
                crate::serve::verbs::tests::script_runner("fleet-lend", r#"for a; do n=$a; done; echo "hi $n path=$PATH""#);
            let mut worker = opts(1, 300);
            worker.verbs = Some(runner);
            worker.state = Some(worker_state);
            worker.home = Some(base.join("worker-home"));
            worker.machines = Some(base.join("worker-machines.toml"));
            worker.lend = Some(lend_path.clone());
            std::fs::create_dir_all(&fleet_root).unwrap();
            let canonical_root = std::fs::canonicalize(&fleet_root).unwrap();
            worker.lend_transport = Some(in_process_hub(hub_dir.clone(), canonical_root.clone()));
            worker.lend_env = Some(BTreeMap::from([("PATH".into(), "/worker/bin".into())]));
            let registry = base.join("machines.toml");
            let mut hub = opts(1, 300);
            hub.hub = true;
            hub.remotes = Some(base.join("hub-data/remotes.json"));
            hub.machines = Some(registry.clone());
            hub.state = Some(base.join("hub-data/state.toml"));
            // The hub daemon's agent (`fleet.agent`): an echo.
            let hub_agent = echo_agent(&base);
            hub.host_agent = Some(Arc::new(move || Some(hub_agent.clone())));
            let hub_daemon = spawn_daemon(&hub_dir, hub);
            drop(conn_at(&hub_dir));
            let worker_daemon = spawn_daemon(&worker_dir, worker);

            let lap = Entry { roots: vec![canonical_root.clone()], ..Entry::offered("lap") };
            wait_for(|| machines::load(&registry).is_ok_and(|e| e == [lap.clone()]), "the offer registers `lap` with its fleet root");
            wait_for(|| endpoint::fleet_socket(&hub_dir, "lap").exists(), "the hub's fleet socket");
            // Lending keeps the worker up past its 300 ms idle timeout.
            std::thread::sleep(Duration::from_millis(600));
            assert!(!worker_daemon.is_finished(), "a lending daemon doesn't idle out");

            // The hub's own link to it: `machines::link::handshake` as for
            // an ssh machine.
            let dial = crate::serve::fleet::offer::dial(hub_dir.clone(), Arc::new(|_: &Entry| unreachable!()));
            let mut link = mlink::handshake("lap", dial(&Entry::offered("lap")).unwrap(), Duration::from_secs(5)).unwrap();
            assert_eq!(link.remote().version, "0.6.0");
            let call = |link: &mut mlink::Link, m: &str, p: Value| link.conn.call(m, p).unwrap();
            let r = call(&mut link, "sandboxes.list", json!({"root": cfg}));
            assert_eq!(r.result.unwrap()[0]["name"], "web", "inside the fleet root");
            let r = call(&mut link, "sandboxes.list", json!({"root": mine}));
            assert_eq!(r.result.unwrap()[0]["name"], "web", "an own root of the worker's");
            let r = call(&mut link, "sandboxes.list", json!({"root": nested}));
            assert_eq!(r.error.unwrap().code, "denied", "not what's nested in one");
            let r = call(&mut link, "sandboxes.list", json!({"root": cfg.join("../../../../mine")}));
            assert_eq!(r.error.unwrap().code, "denied", "`..`");
            let r = call(&mut link, "config.migrate", json!({"root": mine}));
            assert_eq!(r.error.unwrap().code, "denied", "no edit of the worker's own config");
            for (m, p) in [("machines.list", json!({})), ("shutdown", json!({})), ("member.get", json!({}))] {
                assert_eq!(call(&mut link, m, p).error.unwrap().code, "denied", "{m}");
            }
            let roots = call(&mut link, "roots.list", json!({})).result.unwrap();
            let roots: Vec<&str> = roots.as_array().unwrap().iter().map(|r| r["root"].as_str().unwrap()).collect();
            assert_eq!(roots, [mine.to_str().unwrap(), cfg.to_str().unwrap()], "read-only");
            // `web` matches both instances: the fleet root's and the own root's.
            let r = call(&mut link, "instances.resolve", json!({"name": "web"}));
            assert_eq!(r.result.unwrap()["matches"], json!(["own", "web-w"]));
            assert!(call(&mut link, "instances.done", json!({"instance": "own", "done": true})).error.is_none());
            drop(link);

            // Through the hub as any client would: a quick call forwarded.
            let mut c = conn_at(&hub_dir);
            let r = c.call("instances.resolve", json!({"name": "web-w", "machine": "lap"})).unwrap();
            assert_eq!(r.result.unwrap()["found"], json!([{"name": "web-w", "machine": null, "root": cfg, "sandbox": "web"}]));
            let r = c.call("sandboxes.list", json!({"root": nested, "machine": "lap"})).unwrap();
            assert_eq!(r.error.unwrap().code, "denied");

            // A federated watch on the hub watches the offered root on the
            // worker: its snapshot arrives as an ssh machine's does.
            c.call("subscribe", json!({"topics": ["instances"]})).unwrap();
            let here = base.join("hub-root");
            std::fs::create_dir_all(&here).unwrap();
            assert!(c.call("instances.watch", json!({"dir": here, "machines": true})).unwrap().error.is_none());
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let n = c.next_notification(Duration::from_millis(200)).unwrap();
                if let Some(n) = n.filter(|n| n.method == "instances.snapshot") {
                    let m = &n.params["machines"][0];
                    if m["state"] == "connected" && m["runtime"] == "Disk" {
                        assert_eq!(m["name"], "lap");
                        break;
                    }
                }
                assert!(Instant::now() < deadline, "the worker's fleet root never pushed");
            }
            c.call("instances.unwatch", json!({"dir": here})).unwrap();

            // A verb: the worker's env, not the hub's.
            let stream = stream_at(&hub_dir);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let params = json!({"root": cfg, "env": {"PATH": "/hub/bin"}, "instance": "web-w", "machine": "lap"});
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
                    out.push_str(msg["params"]["data"].as_str().unwrap());
                }
            };
            assert!(answer["error"].is_null(), "{answer}");
            assert_eq!(out, "hi web-w path=/worker/bin\n");
            drop((reader, stream, c));

            // The hub's agent on the worker: no socket while the hub's
            // entry has `agent` off; `machines.set` on binds it within the
            // probe window and the hub's agent answers through it; off
            // unbinds it again.
            let agent_socket = super::agent::socket(&worker_dir, "me@hub");
            std::thread::sleep(super::agent::PROBE + Duration::from_millis(500));
            assert!(!agent_socket.exists(), "agent off: not bound");
            let mut c = conn_at(&hub_dir);
            assert!(c.call("machines.set", json!({"name": "lap", "agent": true})).unwrap().error.is_none());
            wait_for(|| agent_socket.exists(), "on: bound");
            assert_eq!(agent_through(&agent_socket), b"sign", "the hub's agent answers");
            assert!(c.call("machines.set", json!({"name": "lap", "agent": false})).unwrap().error.is_none());
            wait_for(|| !agent_socket.exists(), "off again: unbound");
            assert!(c.call("machines.set", json!({"name": "lap", "agent": true})).unwrap().error.is_none());
            wait_for(|| agent_socket.exists(), "on again: bound");
            drop(c);

            // A newer hello from the hub never hands the worker off.
            let mut raw = offered_stream(&hub_dir);
            let newer = Version { semver: "99.0.0".into(), build: 99 };
            match client::hello(&mut raw, &newer, "hub").unwrap() {
                Hello::Ready(r) => assert!(!r.handoff),
                other => panic!("{other:?}"),
            }
            drop(raw);
            // Nor a forced one: that's for this user's local clients only.
            let mut raw = offered_stream(&hub_dir);
            match client::hello_with(&mut raw, &newer, "hub", true).unwrap() {
                Hello::Ready(r) => assert!(!r.handoff),
                other => panic!("{other:?}"),
            }
            drop(raw);
            assert!(!worker_daemon.is_finished());

            // `lend.*` on a local connection: the link's state, and a change
            // applied at once (off drops the link, on brings it back).
            let mut local = conn_at(&worker_dir);
            let info = local.call("lend.get", Value::Null).unwrap().result.unwrap();
            assert_eq!((&info["state"], &info["link"], &info["name"]), (&json!("on"), &json!("connected"), &json!("lap")));
            assert_eq!(info["fleet_root"], json!(std::fs::canonicalize(&fleet_root).unwrap()));
            let off = local.call("lend.set", json!({"state": "off"})).unwrap().result.unwrap();
            assert_eq!(off["link"], "off");
            wait_for(|| !endpoint::fleet_socket(&hub_dir, "lap").exists(), "off: the hub's socket gone");
            wait_for(|| !agent_socket.exists(), "off: the agent socket gone with the link");
            assert_eq!(lend::load(&lend_path).unwrap().unwrap().state, lend::State::Off, "written");
            local.call("lend.set", json!({"state": "drain"})).unwrap().result.unwrap();
            wait_for(|| endpoint::fleet_socket(&hub_dir, "lap").exists(), "drain: linked again");
            wait_for(
                || local.call("lend.get", Value::Null).unwrap().result.unwrap()["link"] == "connected",
                "drain: connected",
            );
            drop(local);

            // A local client stops the worker; its link goes with it, which
            // ends the hub's end and unlinks its socket.
            assert!(conn_at(&worker_dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(worker_daemon.join().unwrap().unwrap(), Exit::Shutdown);
            wait_for(|| !endpoint::fleet_socket(&hub_dir, "lap").exists(), "the hub's socket gone");
            assert!(!agent_socket.exists(), "the agent socket gone with the worker");
            // A hub never idles out: stopped by hand.
            assert!(conn_at(&hub_dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(hub_daemon.join().unwrap().unwrap(), Exit::Shutdown);
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&verb_dir);
        }

        /// A verb request on `dir`'s daemon: its answer, its stdout and
        /// stderr as streamed.
        fn verb(dir: &Path, method: &str, params: Value) -> (Value, String, String) {
            let stream = stream_at(dir);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            writeln!(&stream, "{}", json!({"id": 1, "method": method, "params": params})).unwrap();
            let (mut out, mut err) = (String::new(), String::new());
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0, "closed before the answer");
                let msg: Value = serde_json::from_str(&line).unwrap();
                if msg.get("id").is_some() {
                    return (msg, out, err);
                }
                if msg["method"] == "output" {
                    let data = msg["params"]["data"].as_str().unwrap();
                    if msg["params"]["stream"] == "stdout" { out.push_str(data) } else { err.push_str(data) }
                }
            }
        }

        /// A hub with an offered worker `lap` (in-process, as above) and a
        /// config root whose sandbox's folder is a checkout of a local
        /// origin: a `run` that pins nothing is placed on `lap` (its fresh
        /// `capacity.get` beats this host's neutral report), the root's tar
        /// and the branch's bundle land in the worker's fleet root, and the
        /// request is forwarded there in that copy; `here`, and a sandbox
        /// that can't move, run here; a draining `lap` gets no `run`.
        #[test]
        fn a_run_is_placed_on_a_worker_with_its_code() {
            use crate::serve::capacity::Capacity;
            use crate::serve::fleet::code::repo::tests::{git, origin_and_checkout};

            let base = scratch("fleet-place");
            let (worker_dir, hub_dir) = (base.join("w"), base.join("hub"));
            let src = base.join("src");
            std::fs::create_dir_all(&src).unwrap();
            origin_and_checkout(&src, "web");
            std::fs::create_dir_all(src.join("plain")).unwrap();
            git(&src.join("plain"), &["init", "-q"]);
            let cfg = src.join("cfg");
            std::fs::create_dir_all(cfg.join("dockerfiles")).unwrap();
            std::fs::write(
                cfg.join(crate::config::CONFIG_FILE),
                "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\n\n[sandbox.plain]\nfolder = \"../plain\"\nimage = \"img\"\n",
            )
            .unwrap();
            std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM x\n").unwrap();
            let cfg = std::fs::canonicalize(&cfg).unwrap();

            let data = base.join("worker-data");
            let fleet_root = lend::fleet_root(&data, "me@hub").unwrap();
            std::fs::create_dir_all(&fleet_root).unwrap();
            let fleet_root = std::fs::canonicalize(&fleet_root).unwrap();
            let lend_path = data.join("lend.toml");
            lend::write_for_test(&lend_path, &Lend { state: lend::State::On, hub: "me@hub".into() });
            member::write_for_test(&data.join("member.toml"), &member::Member { name: "lap".into(), ..Default::default() });
            let path = std::env::var("PATH").unwrap_or_default();
            // `$3` is the `-C` root: show what's there, as the verb would see it.
            let (w_runner, w_verbs) = crate::serve::verbs::tests::script_runner(
                "fleet-place-w",
                r#"echo "worker $*"; cat "$3/dockerfiles/web.Dockerfile" "$3/../web/F""#,
            );
            let mut worker = opts(1, 300);
            worker.verbs = Some(w_runner);
            worker.state = Some(data.join("state.toml"));
            worker.home = Some(base.join("worker-home"));
            worker.machines = Some(base.join("worker-machines.toml"));
            worker.lend = Some(lend_path.clone());
            worker.lend_transport = Some(in_process_hub(hub_dir.clone(), fleet_root.clone()));
            worker.lend_env = Some(BTreeMap::from([("PATH".into(), path.clone())]));
            // Its `capacity.get`: plenty of room (the hub's own is the test
            // default, an empty report: neutral).
            const G: u64 = 1 << 30;
            let roomy = Capacity {
                cpus: Some(8),
                mem_total: Some(32 * G),
                mem_available: Some(28 * G),
                disk_free: Some(400 * G),
                disk_total: Some(500 * G),
                ..Capacity::default()
            };
            worker.local_capacity = Some(Arc::new(move || roomy.clone()));

            let (h_runner, h_verbs) = crate::serve::verbs::tests::script_runner("fleet-place-h", r#"echo "local $*""#);
            let registry = base.join("machines.toml");
            let mut hub = opts(1, 300);
            hub.hub = true;
            hub.verbs = Some(h_runner);
            hub.remotes = Some(base.join("hub-data/remotes.json"));
            hub.machines = Some(registry.clone());
            hub.state = Some(base.join("hub-data/state.toml"));
            hub.repos = Some(base.join("hub-data/repos"));
            let hub_daemon = spawn_daemon(&hub_dir, hub);
            drop(conn_at(&hub_dir));
            let worker_daemon = spawn_daemon(&worker_dir, worker);
            wait_for(|| endpoint::fleet_transfer_socket(&hub_dir, "lap").exists(), "the worker's link");
            let mut c = conn_at(&hub_dir);
            wait_for(
                || {
                    let rows = c.call("machines.list", Value::Null).unwrap().result.unwrap();
                    rows.as_array().unwrap().iter().any(|r| r["name"] == "lap" && r["state"] == "connected")
                },
                "the hub's link to lap",
            );
            // Nothing watched: the hub has no report of lap's, and its own
            // is neutral; only lap's fresh `capacity.get` tips it.
            let rows = c.call("machines.list", Value::Null).unwrap().result.unwrap();
            assert!(rows[0]["capacity"].is_null(), "{rows}");
            // Through the trust table, as the hub asks it.
            let dial = crate::serve::fleet::offer::dial(hub_dir.clone(), Arc::new(|_: &Entry| unreachable!()));
            let mut link = mlink::handshake("lap", dial(&Entry::offered("lap")).unwrap(), Duration::from_secs(5)).unwrap();
            let r = link.conn.call("capacity.get", json!({})).unwrap().result.unwrap();
            assert_eq!((&r["cpus"], &r["lending"]), (&json!(8), &json!("on")), "{r}");
            crate::serve::dts::assert_matches("Capacity", &r);
            assert!(crate::serve::dts::prop_type("ApiMethods", "capacity.get").is_some_and(|t| t.contains("result: Capacity")));
            drop((link, c));
            let params = |sandbox: &str, extra: Value| {
                let mut p = json!({"root": cfg, "env": {"PATH": path}, "sandbox": sandbox});
                p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                p
            };

            // Placed: the code there first, then the run in its copy.
            let (answer, out, err) = verb(&hub_dir, "run", params("web", json!({"branch": "feature"})));
            assert!(answer["error"].is_null(), "{answer}");
            assert_eq!(answer["result"]["machine"], "lap", "{answer}");
            assert!(err.starts_with("placed on lap ("), "{err}");
            let roots: Vec<_> = std::fs::read_dir(fleet_root.join("roots")).unwrap().flatten().map(|e| e.file_name()).filter(|n| n != ".in").collect();
            assert_eq!(roots.len(), 1, "{roots:?}");
            let remote = fleet_root.join("roots").join(&roots[0]).join("cfg");
            assert_eq!(out, format!("worker --in-process -C {} run --branch=feature -- web\nFROM x\nfeature\n", remote.display()));
            assert_eq!(git(&remote.join("../web"), &["symbolic-ref", "--short", "HEAD"]), "feature");

            // A branch origin lacks: the worker starts it at the bundled tip.
            let (answer, out, _) = verb(&hub_dir, "run", params("web", json!({"branch": "new-one"})));
            assert!(answer["error"].is_null(), "{answer}");
            assert!(out.starts_with(&format!("worker --in-process -C {} run --branch=new-one --base=main -- web\n", remote.display())), "{out}");

            // `here`: this host, nothing placed.
            let (answer, out, err) = verb(&hub_dir, "run", params("web", json!({"here": true})));
            assert!(answer["error"].is_null() && answer["result"].get("machine").is_none(), "{answer}");
            assert_eq!(out, format!("local --in-process -C {} run -- web\n", cfg.display()));
            assert_eq!(err, "");

            // A folder without `origin` can't move: here, with the note.
            let (answer, out, err) = verb(&hub_dir, "run", params("plain", json!({})));
            assert!(answer["error"].is_null(), "{answer}");
            assert_eq!(out, format!("local --in-process -C {} run -- plain\n", cfg.display()));
            assert!(err.starts_with("note: not placed on lap: ") && err.ends_with("; running here\n"), "{err}");
            assert!(err.contains("has no `origin`"), "{err}");

            // `drain` on the worker: its fresh report says so, so the hub
            // runs here; pinned there, `run` is refused, other verbs aren't.
            let mut local = conn_at(&worker_dir);
            assert!(local.call("lend.set", json!({"state": "drain"})).unwrap().error.is_none());
            drop(local);
            let (answer, out, err) = verb(&hub_dir, "run", params("web", json!({})));
            assert!(answer["error"].is_null() && answer["result"].get("machine").is_none(), "{answer}");
            assert_eq!(out, format!("local --in-process -C {} run -- web\n", cfg.display()));
            assert_eq!(err, "");
            let (answer, ..) = verb(&hub_dir, "run", params("web", json!({"root": remote, "machine": "lap"})));
            assert_eq!(answer["error"]["code"], "denied", "{answer}");
            assert_eq!(answer["error"]["message"], crate::serve::fleet::lent::DRAINING);
            let (answer, ..) = verb(&hub_dir, "stop", json!({"root": remote, "env": {}, "instance": "nope", "machine": "lap"}));
            assert!(answer["error"].is_null(), "stop isn't refused (the verb ran there): {answer}");

            assert!(conn_at(&worker_dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(worker_daemon.join().unwrap().unwrap(), Exit::Shutdown);
            assert!(conn_at(&hub_dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(hub_daemon.join().unwrap().unwrap(), Exit::Shutdown);
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&w_verbs);
            let _ = std::fs::remove_dir_all(&h_verbs);
        }
    }

    #[test]
    fn a_failing_transport_is_retried_until_dropped() {
        let tries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&tries);
        let transport: Transport = Arc::new(move |_: &[OsString]| {
            counted.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("no route")
        });
        let timing = Timing { backoff: Duration::from_millis(5), backoff_cap: Duration::from_millis(10), ..Timing::default() };
        let lending =
            Lending::spawn("hub".into(), "lap".into(), "/fleet/hub".into(), false, transport, Vec::new(), Receivers::new(|_| {}), None, Follow { steps: Arc::new(Fake::default()), dev_builds: Arc::new(|| false) }, timing, |_| {}).unwrap();
        let start = std::time::Instant::now();
        while tries.load(Ordering::SeqCst) < 3 {
            assert!(start.elapsed() < Duration::from_secs(5), "retried");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lending.status(), Status::Retrying("no route".into()));
        drop(lending);
        let n = tries.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(tries.load(Ordering::SeqCst), n, "stopped");
    }
}
