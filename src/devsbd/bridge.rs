//! Host half of the ssh-agent relay (docs/sandbox-helper.md): runs
//! `exec -i <c> devsbd bridge` and serves the streams it carries, connecting
//! each `Open` to the host agent. Owners: CLI `exec` (for the command's
//! lifetime) and the TUI (one per running instance while open).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::mux::{self, Conn, Mux};
use super::proto::caps;
use super::{proto, relay_mode, BIN};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// A running bridge; dropping it kills the `exec` (the in-container bridge
/// then sees stdin EOF and exits).
pub struct Bridge {
    // Shared so the keepalive/handshake-timeout threads can kill the child too;
    // `Drop` and those threads race on `kill()`, which is idempotent.
    child: Arc<Mutex<Child>>,
    done: Arc<AtomicBool>,
    // Set by the handshake thread when the failure is a protocol version
    // mismatch — either detected host-side (`proto::version_mismatch`) or by the
    // bridge exiting `proto::MISMATCH_EXIT` (daemon mismatch). A mismatched
    // bridge is not retried while its container stays running: only a restart,
    // which rewrites the helper, can fix it.
    mismatch: Arc<AtomicBool>,
    handshake: mpsc::Receiver<Result<(), String>>,
    // Published by the handshake thread once `Hello` succeeds: the live mux (to
    // open forward streams on) and the daemon's advertised caps. Absent while
    // the handshake is pending or after it failed.
    forward: Arc<OnceLock<Forward>>,
    // The instance's container, for the "outdated helper" message.
    container: String,
}

/// The forwarding side of a healthy bridge: the mux carrying its streams and
/// the peer (daemon) caps negotiated in the handshake.
struct Forward {
    mux: Arc<Mux>,
    peer_caps: u32,
}

impl Bridge {
    /// The handshake result, waiting up to `timeout` for it; `None` while it
    /// is still pending. Yields the result once (it's consumed).
    pub fn outcome(&self, timeout: Duration) -> Option<Result<(), String>> {
        self.handshake.recv_timeout(timeout).ok()
    }

    /// The relay ended (container stopped, daemon gone, handshake failed).
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    /// The relay failed on a protocol version mismatch (host or daemon side).
    pub fn is_mismatch(&self) -> bool {
        self.mismatch.load(Ordering::Relaxed)
    }

    /// The daemon's advertised capabilities, available once the handshake has
    /// succeeded; `None` while it's pending or after it failed.
    #[allow(dead_code)] // host forwarder engine (docs/port-forwarding.md, step 5)
    pub fn peer_caps(&self) -> Option<u32> {
        self.forward.get().map(|f| f.peer_caps)
    }

    /// Open a flow-controlled forward stream to `host:port`, dialed by the
    /// daemon inside the container, relaying `conn` over it. `on_close` runs
    /// once when the stream ends, with the reason (see [`mux::OnClose`]).
    ///
    /// Errors before the handshake is done, when the bridge is dead, or when the
    /// daemon doesn't advertise `TCP_FORWARD` (an outdated helper) — the last as
    /// `Unsupported`, so callers can phrase the restart hint.
    #[allow(dead_code)] // host forwarder engine (docs/port-forwarding.md, step 5)
    pub fn connect(
        &self,
        conn: impl Into<Conn>,
        host: &str,
        port: u16,
        on_close: impl FnOnce(String) + Send + 'static,
    ) -> std::io::Result<u32> {
        let Some(forward) = self.forward.get() else {
            let kind = if self.is_done() {
                std::io::ErrorKind::BrokenPipe
            } else {
                std::io::ErrorKind::NotConnected
            };
            let msg = if self.is_done() { "bridge is not running" } else { "bridge handshake not done" };
            return Err(std::io::Error::new(kind, msg));
        };
        if self.is_done() {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "bridge is not running"));
        }
        if forward.peer_caps & caps::TCP_FORWARD == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("helper in {} is outdated (no port forwarding): restart the instance", self.container),
            ));
        }
        forward.mux.connect(conn, host, port, on_close)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The host agent socket, read per stream so agent rotation needs no restart.
/// `$SSH_AUTH_SOCK` set and its path present on the host; the existence check
/// also gates whether a bridge is worth spawning (`has_host_agent`).
fn host_agent() -> Option<PathBuf> {
    let sock = PathBuf::from(std::env::var_os("SSH_AUTH_SOCK")?);
    std::fs::metadata(&sock).is_ok().then_some(sock)
}

/// Whether the host has a usable ssh-agent right now. Gates bridge spawning
/// (`exec_status`, `Bridges::reconcile`) and `SSH_AUTH_SOCK` injection: with no
/// agent a bridge and the env var only cost an extra `exec` that can't help.
pub fn has_host_agent() -> bool {
    host_agent().is_some()
}

/// Start a bridge for `info` without waiting for its handshake (see
/// `outcome`). Fully quiet (stderr captured), since the TUI owns the
/// screen; `None` only when the `exec` can't even be spawned.
pub fn spawn(info: &Instance) -> Option<Bridge> {
    let hash = info.devsbd_arch.and_then(super::hash).unwrap_or_default();
    spawn_with(&info.container, hash, Some(host_agent))
}

/// Start a bridge for `container` with helper `hash`, optionally serving the
/// host ssh-agent. `with_agent` = true wires the real `host_agent` provider (a
/// forward bridge serves ssh-agent when the host has one, advertising
/// `SSH_AGENT` honestly); false makes an agent-less bridge that the daemon
/// won't route agent clients to. The forwarder (docs/port-forwarding.md, step
/// 5) uses this so `spawn_with`'s generic agent type stays private.
pub fn spawn_for(container: &str, hash: &str, with_agent: bool) -> Option<Bridge> {
    if with_agent {
        spawn_with(container, hash, Some(host_agent))
    } else {
        let none: Option<fn() -> Option<PathBuf>> = None;
        spawn_with(container, hash, none)
    }
}

/// Start a bridge. `agent` is the host ssh-agent socket provider, or `None` for
/// an agent-less bridge (a `devsandbox port` forward): it advertises no
/// `SSH_AGENT` cap and refuses agent `Open`s, so the daemon won't route agent
/// clients to it and it can't steal ssh from an older agent bridge.
fn spawn_with(
    container: &str,
    hash: &str,
    agent: Option<impl Fn() -> Option<PathBuf> + Send + 'static>,
) -> Option<Bridge> {
    let mut child = Command::new(backend().bin())
        .args(["exec", "-i", "-u", "root", container, BIN, "bridge"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let child = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));
    let mismatch = Arc::new(AtomicBool::new(false));
    let (tx, handshake) = mpsc::channel();
    let hash = hash.to_string();
    let container = container.to_string();
    let finished = Arc::clone(&done);
    let hs_mismatch = Arc::clone(&mismatch);
    let forward = Arc::new(OnceLock::new());
    let hs_forward = Arc::clone(&forward);
    // The handshake thread moves `container` (for `mismatch_message`); keep a
    // copy for the `Bridge`'s own `connect` error message.
    let bridge_container = container.clone();
    // Watchdog: if the handshake hasn't produced a result within the timeout,
    // kill the child, which unblocks the blocking `handshake` read below. The
    // handshake thread then sees the read fail and, because `timed_out` is set,
    // reports the timeout instead of the resulting EOF.
    let timed_out = Arc::new(AtomicBool::new(false));
    let bridged = Arc::new(AtomicBool::new(false));
    {
        let child = Arc::clone(&child);
        let timed_out = Arc::clone(&timed_out);
        let bridged = Arc::clone(&bridged);
        std::thread::spawn(move || {
            std::thread::sleep(HANDSHAKE_TIMEOUT);
            // `bridged` flips once the handshake succeeds and routing starts;
            // past that there's no handshake to time out.
            if !bridged.load(Ordering::Relaxed) {
                timed_out.store(true, Ordering::Relaxed);
                let _ = child.lock().unwrap().kill();
            }
        });
    }
    let hs_child = Arc::clone(&child);
    std::thread::spawn(move || {
        // The bridge advertises to us `own & daemon` caps, so `daemon_caps` is
        // what the whole path (daemon included) can serve.
        let mut daemon_caps = 0;
        let result = proto::handshake(&mut stdout, &mut stdin, &hash, 0).map(|peer| daemon_caps = peer.caps).map_err(|e| {
            if timed_out.load(Ordering::Relaxed) {
                return "helper handshake timed out".to_string();
            }
            // A version mismatch is a typed error carrying both versions, so we
            // phrase it with the right direction and container. Otherwise the
            // bridge died before `Hello` (no daemon, no binary) and its stderr
            // says why — but when the *daemon* is the mismatch, the bridge
            // already printed its own direction-aware line, so prefer stderr.
            if let Some(vm) = proto::version_mismatch(&e) {
                hs_mismatch.store(true, Ordering::Relaxed);
                return mismatch_message(&container, vm);
            }
            let mut msg = String::new();
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                let _ = std::io::Read::read_to_string(&mut stderr, &mut msg);
                // The bridge exits `MISMATCH_EXIT` when *its* handshake with the
                // daemon failed on a version mismatch (it printed the line we
                // just read). Classify that from the exit code, not the text, so
                // reconcile can stop retrying. `wait()` after EOF is prompt.
                if let Ok(status) = hs_child.lock().unwrap().wait() {
                    if status.code() == Some(proto::MISMATCH_EXIT) {
                        hs_mismatch.store(true, Ordering::Relaxed);
                    }
                }
            }
            match msg.trim() {
                "" => e.to_string(),
                m => m.trim_start_matches("devsbd: ").to_string(),
            }
        });
        let ok = result.is_ok();
        // Stop the watchdog from racing a kill against a healthy bridge.
        bridged.store(ok, Ordering::Relaxed);
        if !ok {
            let _ = tx.send(result);
            finished.store(true, Ordering::Relaxed);
            return;
        }
        {
            let mux = Mux::new(stdin);
            // Tell the daemon our caps right after the handshake, on stream 0:
            // `SSH_AGENT` only when this bridge has an agent provider and it
            // yields a socket now. An agent-less bridge (a `devsandbox port`
            // forward) advertises 0, so the daemon won't route agent clients to
            // it. Best effort — a send failure just means the connection died.
            let has_agent = agent.as_ref().and_then(|a| a()).is_some();
            let own = if has_agent { caps::SSH_AGENT } else { 0 };
            let _ = mux.send(&proto::Frame::Caps(own));
            // Publish the mux + daemon caps *before* reporting the handshake
            // result, so a caller that sees `outcome() == Ok` can rely on
            // `peer_caps()` / `connect()` being ready (no publish race).
            let _ = hs_forward.set(Forward { mux: Arc::clone(&mux), peer_caps: daemon_caps });
            let _ = tx.send(result);
            // Keepalive: a wedged daemon (stopped reading) is caught by inbound
            // silence; `on_dead` kills the child, ending the `serve` read below
            // so `done` flips and `Bridges::reconcile` retries.
            let dead_child = Arc::clone(&hs_child);
            mux.keepalive(mux::KEEPALIVE_INTERVAL, mux::KEEPALIVE_TIMEOUT, move || {
                let _ = dead_child.lock().unwrap().kill();
            });
            mux.serve(stdout, move |_, channel| {
                if channel != proto::channel::SSH_AGENT {
                    return None;
                }
                let agent = agent.as_ref()?;
                std::os::unix::net::UnixStream::connect(agent()?).ok().map(Into::into)
            });
        }
        finished.store(true, Ordering::Relaxed);
    });
    Some(Bridge { child, done, mismatch, handshake, forward, container: bridge_container })
}

/// How long the host waits for the daemon handshake before killing the child
/// and reporting a timeout, so a container that accepts the `exec` but never
/// completes `Hello` (wedged daemon, stuck runtime) can't block a bridge owner
/// forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A host-side, direction-aware message for a helper/host protocol mismatch.
/// Peer (the helper) older → tell the user to restart the instance so the
/// binary is rewritten; peer newer → this `devsandbox` is the stale side.
/// The instance key isn't on hand here (only the container), so the fix hint
/// stays generic ("restart the instance").
fn mismatch_message(container: &str, vm: &proto::VersionMismatch) -> String {
    if vm.peer < vm.ours {
        format!(
            "helper in {container} is outdated (protocol {}, need {}): restart the instance",
            vm.peer, vm.ours
        )
    } else {
        format!(
            "this devsandbox is older than the helper in {container} (protocol {}, need {})",
            vm.peer, vm.ours
        )
    }
}

/// Minimum gap between bridge attempts for one container, so a container
/// whose bridge keeps failing (daemon down) isn't re-exec'd every tick.
const RETRY: Duration = Duration::from_secs(10);

/// Gap after a version mismatch. Long, since only a helper rewrite fixes it,
/// but not infinite: `devsandbox start` on a still-running container rewrites
/// the helper without the container ever leaving the running set.
const MISMATCH_RETRY: Duration = Duration::from_secs(300);

/// What `reconcile` should do with the current bridge for one container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// No live bridge for this container yet: start one.
    Spawn,
    /// Keep the existing entry untouched (healthy, or dead but still within
    /// its retry gap).
    Keep,
    /// The bridge is dead and past its retry gap: replace it.
    Respawn,
}

/// The retry policy for one container's bridge, factored out so it's testable
/// without a container runtime. `entry` is the current live bridge's state, or
/// `None` when there is none: `(done, mismatch, since_spawn)`. A dead bridge
/// is retried after `RETRY`, or `MISMATCH_RETRY` when it died on a version
/// mismatch (a restart, which drops the entry, retries at once).
fn retry_decision(entry: Option<(bool, bool, Duration)>) -> Retry {
    match entry {
        None => Retry::Spawn,
        Some((true, mismatch, since)) => {
            let gap = if mismatch { MISMATCH_RETRY } else { RETRY };
            if since >= gap { Retry::Respawn } else { Retry::Keep }
        }
        Some((false, ..)) => Retry::Keep,
    }
}

/// The TUI's set of bridges, reconciled against the running instances on each
/// snapshot. Owned by a worker thread (`spawn_worker`), never touched on the UI
/// thread.
#[derive(Default)]
pub struct Bridges {
    live: HashMap<String, (Bridge, Instant)>,
}

impl Bridges {
    /// Keep one bridge per running container in `running` whose instance
    /// wants one; drop the rest. Dead bridges are retried per `retry_decision`.
    pub fn reconcile(&mut self, running: &[&str]) {
        self.live.retain(|c, _| running.contains(&c.as_str()));
        // No host agent → nothing to relay; skip the per-instance `exec`.
        if !has_host_agent() {
            self.live.clear();
            return;
        }
        let Ok(state) = State::load() else { return };
        for info in state.instances.values() {
            if !running.contains(&info.container.as_str()) || !relay_mode(info) {
                continue;
            }
            let entry = self
                .live
                .get(&info.container)
                .map(|(b, at)| (b.is_done(), b.is_mismatch(), at.elapsed()));
            if retry_decision(entry) != Retry::Keep {
                self.live.remove(&info.container);
                if let Some(b) = spawn(info) {
                    self.live.insert(info.container.clone(), (b, Instant::now()));
                }
            }
        }
    }

    /// Move a `Bridges` onto its own thread, fed running-container lists over an
    /// mpsc. Reconcile (which does `State::load` and `exec` spawns) never runs
    /// on the UI thread; the snapshot arm just `send`s the owned list. Dropping
    /// the returned [`BridgeWorker`] closes the channel and joins the thread,
    /// which drops every live `Bridge` (killing its `exec`) before returning.
    pub fn spawn_worker() -> BridgeWorker {
        let (tx, rx) = mpsc::channel::<Vec<String>>();
        let handle = std::thread::spawn(move || {
            let mut bridges = Bridges::default();
            // Block for the next list, then coalesce: drain everything already
            // queued and reconcile only against the newest, so a burst of
            // snapshots costs one reconcile.
            while let Ok(mut running) = rx.recv() {
                while let Ok(next) = rx.try_recv() {
                    running = next;
                }
                let refs: Vec<&str> = running.iter().map(String::as_str).collect();
                bridges.reconcile(&refs);
            }
            // Sender dropped: `bridges` drops here, killing every live bridge.
        });
        BridgeWorker { tx: Some(tx), handle: Some(handle) }
    }
}

/// Handle to the bridge worker thread. Send running-container lists with
/// [`send`](Self::send); on drop the channel closes and the thread is joined,
/// so all bridges are killed before the caller (the TUI) restores the terminal.
pub struct BridgeWorker {
    tx: Option<mpsc::Sender<Vec<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl BridgeWorker {
    /// Hand the worker the current running-container set. Never blocks the UI
    /// thread; a dead worker (thread gone) is silently ignored.
    pub fn send(&self, running: Vec<String>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(running);
        }
    }
}

impl Drop for BridgeWorker {
    fn drop(&mut self) {
        // Close the channel first so the worker's `recv` returns and it drops
        // `Bridges` (killing every bridge), then join so that teardown finishes
        // before the terminal is restored.
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::{hash, install, start_daemon};
    use std::process::Command;

    fn ok(cmd: &mut Command) -> bool {
        matches!(cmd.output(), Ok(o) if o.status.success())
    }

    #[test]
    fn retry_policy() {
        // No entry: always start one.
        assert_eq!(retry_decision(None), Retry::Spawn);
        // Alive (not done): keep, regardless of elapsed time.
        let (zero, day) = (Duration::ZERO, Duration::from_secs(86_400));
        assert_eq!(retry_decision(Some((false, false, zero))), Retry::Keep);
        assert_eq!(retry_decision(Some((false, false, day))), Retry::Keep);
        // Dead, not a mismatch: retried after RETRY.
        assert_eq!(retry_decision(Some((true, false, zero))), Retry::Keep);
        assert_eq!(retry_decision(Some((true, false, RETRY))), Retry::Respawn);
        // Dead on a mismatch: only after the long MISMATCH_RETRY.
        assert_eq!(retry_decision(Some((true, true, RETRY))), Retry::Keep);
        assert_eq!(retry_decision(Some((true, true, MISMATCH_RETRY))), Retry::Respawn);
    }

    /// The worker reconciles against only the newest queued list. With no host
    /// agent every reconcile is a cheap no-op (no `exec`), so this exercises the
    /// send/coalesce/join path without docker.
    #[test]
    fn worker_coalesces_and_joins() {
        let worker = Bridges::spawn_worker();
        for i in 0..100 {
            worker.send(vec![format!("devsandbox-c{i}")]);
        }
        // Dropping joins the thread; it must have drained without deadlock.
        drop(worker);
    }

    /// Docker-gated end to end: a real `ssh-add -l` in an alpine container
    /// lists a key held by a host `ssh-agent`, through daemon + bridge.
    /// Also checks the daemon falls back to the older bridge when the newest
    /// one ends. Skips (fails on CI) without docker, an embedded helper, host
    /// ssh-agent/ssh-keygen, or network for `apk add`.
    #[test_utils::docker_test(helper)]
    fn relays_host_agent_into_container_with_docker() -> Result<(), &'static str> {
        if !ok(Command::new("ssh-agent").arg("-h")) && !ok(Command::new("which").arg("ssh-agent")) {
            return Err("no host ssh-agent");
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("devsbd-relay-{stamp}"));
        std::fs::create_dir_all(&tmp).unwrap();
        let sock = tmp.join("agent.sock");
        let key = tmp.join("id");
        let name = format!("devsandbox-relay-test-{stamp}");

        let mut agent = Command::new("ssh-agent").arg("-D").arg("-a").arg(&sock).stdout(Stdio::null()).spawn().unwrap();
        let cleanup = || {
            let _ = agent.kill();
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
            let _ = std::fs::remove_dir_all(&tmp);
        };
        crate::test_support::with_cleanup(cleanup, || {
            for _ in 0..100 {
                if sock.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(ok(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "devsbd-relay-test", "-f"])
                .arg(&key)));
            assert!(ok(Command::new("ssh-add").arg(&key).env("SSH_AUTH_SOCK", &sock)));

            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            if !ok(Command::new("docker").args(["exec", &name, "apk", "add", "--no-cache", "-q", "openssh-client-default"])) {
                return Err("apk add failed (no network?)");
            }
            let arch = install(&name, None).unwrap();
            let bridge = || {
                let agent_path = sock.clone();
                spawn_with(&name, hash(arch).unwrap(), Some(move || Some(agent_path.clone()))).unwrap()
            };
            let ssh_add = || {
                Command::new("docker")
                    .args(["exec", "-e", "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock", &name, "ssh-add", "-l"])
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
            };
            let list = || String::from_utf8_lossy(&ssh_add().wait_with_output().unwrap().stdout).into_owned();
            let pidfile = || {
                let out = Command::new("docker").args(["exec", &name, "cat", "/run/devsandbox/devsbd.pid"]).output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };

            // No daemon yet: the bridge self-heals by starting one, so the
            // handshake succeeds and the key lists — no `start_daemon` first.
            let boot = bridge();
            assert_eq!(boot.outcome(Duration::from_secs(10)), Some(Ok(())), "self-started daemon");
            assert!(list().contains("devsbd-relay-test"), "via self-started daemon");
            drop(boot);
            // The daemon outlives the bridge that started it (own process
            // group), so it's still routing once `boot` is gone.

            // start_daemon on the running daemon is a no-op takeover (same
            // build → pidfile lock held → the new process exits).
            start_daemon(&name);
            // Optimistic start: the client connects before any bridge is
            // attached and is held until one attaches.
            let early = ssh_add();
            std::thread::sleep(Duration::from_millis(300));
            let long = bridge();
            let out = String::from_utf8_lossy(&early.wait_with_output().unwrap().stdout).into_owned();
            assert!(out.contains("devsbd-relay-test"), "held client: {out}");
            assert_eq!(long.outcome(Duration::from_secs(5)), Some(Ok(())));

            let short = bridge();
            assert_eq!(short.outcome(Duration::from_secs(5)), Some(Ok(())));
            assert!(list().contains("devsbd-relay-test"), "via newest bridge");
            drop(short);
            // The daemon notices the dropped bridge asynchronously.
            let mut fell_back = false;
            for _ in 0..50 {
                if list().contains("devsbd-relay-test") {
                    fell_back = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(fell_back, "daemon fell back to the older bridge");
            assert!(!long.is_done());

            // Takeover: pretend the running daemon is another build; a new
            // daemon makes it quit (its bridges end) and replaces it.
            let old = pidfile();
            let old_pid = old.split_whitespace().next().unwrap().to_string();
            let fake = format!("printf '{old_pid} outdated\\n' > /run/devsandbox/devsbd.pid");
            assert!(ok(Command::new("docker").args(["exec", &name, "sh", "-c", &fake])));
            start_daemon(&name);
            let mut replaced = false;
            for _ in 0..50 {
                let now = pidfile();
                if !now.is_empty() && !now.starts_with(&format!("{old_pid} ")) {
                    assert!(now.ends_with(hash(arch).unwrap()), "{now}");
                    replaced = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(replaced, "new daemon took over");
            for _ in 0..50 {
                if long.is_done() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(long.is_done(), "old daemon's bridge ended");
            let fresh = bridge();
            assert_eq!(fresh.outcome(Duration::from_secs(5)), Some(Ok(())));
            assert!(list().contains("devsbd-relay-test"), "via new daemon");
            // Same build again: a second daemon leaves the running one alone.
            let before = pidfile();
            start_daemon(&name);
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(pidfile(), before);

            // Kill the daemon (as a container restart would) with no
            // start_daemon: the next bridge revives it and the key lists.
            drop(fresh);
            let killed = "kill $(cut -d' ' -f1 /run/devsandbox/devsbd.pid)";
            assert!(ok(Command::new("docker").args(["exec", &name, "sh", "-c", killed])));
            let revived = bridge();
            assert_eq!(revived.outcome(Duration::from_secs(10)), Some(Ok(())), "revived daemon");
            assert!(list().contains("devsbd-relay-test"), "via revived daemon");
            Ok(())
        })
    }

    /// Docker-gated: an agent-less bridge (a `devsandbox port` forward) spawned
    /// *after* an agent bridge must not steal agent routing — `ssh-add -l` still
    /// lists the key via the older agent bridge. Regression for the routing fix
    /// in docs/port-forwarding.md (a shell with no agent starting a forward
    /// would otherwise become the newest bridge and break ssh in the container).
    #[test_utils::docker_test(helper)]
    fn agent_less_bridge_does_not_steal_agent_routing_with_docker() -> Result<(), &'static str> {
        if !ok(Command::new("ssh-agent").arg("-h")) && !ok(Command::new("which").arg("ssh-agent")) {
            return Err("no host ssh-agent");
        }
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let tmp = std::env::temp_dir().join(format!("devsbd-agentroute-{stamp}"));
        std::fs::create_dir_all(&tmp).unwrap();
        let sock = tmp.join("agent.sock");
        let key = tmp.join("id");
        let name = format!("devsandbox-agentroute-test-{stamp}");
        let mut agent = Command::new("ssh-agent").arg("-D").arg("-a").arg(&sock).stdout(Stdio::null()).spawn().unwrap();
        let cleanup = || {
            let _ = agent.kill();
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
            let _ = std::fs::remove_dir_all(&tmp);
        };
        crate::test_support::with_cleanup(cleanup, || {
            for _ in 0..100 {
                if sock.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(ok(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "devsbd-agentroute", "-f"])
                .arg(&key)));
            assert!(ok(Command::new("ssh-add").arg(&key).env("SSH_AUTH_SOCK", &sock)));
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])));
            if !ok(Command::new("docker").args(["exec", &name, "apk", "add", "--no-cache", "-q", "openssh-client-default"])) {
                return Err("apk add failed (no network?)");
            }
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let list = || {
                let out = Command::new("docker")
                    .args(["exec", "-e", "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock", &name, "ssh-add", "-l"])
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).into_owned()
            };

            // The agent bridge attaches first and lists the key.
            let agent_path = sock.clone();
            let agent_bridge = spawn_with(&name, hash(arch).unwrap(), Some(move || Some(agent_path.clone()))).unwrap();
            assert_eq!(agent_bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "agent bridge");
            assert!(list().contains("devsbd-agentroute"), "via agent bridge");

            // Newer agent-less bridges attach while `ssh-add -l` clients keep
            // arriving. The dangerous window is between the bridge registering
            // with the daemon and its host's `Caps(0)` landing (the bridge↔host
            // handshake sits in between): the daemon must treat it as pending,
            // not capable, or those clients hit a host that refuses them. Each
            // round fires clients staggered across the spawn; every one must
            // list the key via the older agent bridge.
            let mut port_bridges = Vec::new();
            for round in 0..6 {
                let outputs = std::thread::scope(|s| {
                    let clients: Vec<_> = (0..12)
                        .map(|i| {
                            let list = &list;
                            s.spawn(move || {
                                std::thread::sleep(Duration::from_millis(15 * i));
                                list()
                            })
                        })
                        .collect();
                    let none: Option<fn() -> Option<PathBuf>> = None;
                    let port_bridge = spawn_with(&name, hash(arch).unwrap(), none).unwrap();
                    assert_eq!(port_bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "agent-less bridge");
                    port_bridges.push(port_bridge);
                    clients.into_iter().map(|c| c.join().unwrap()).collect::<Vec<_>>()
                });
                for (i, out) in outputs.iter().enumerate() {
                    assert!(out.contains("devsbd-agentroute"), "round {round} client {i}: agent-less bridge stole routing: {out:?}");
                }
            }
            // Settled (every `Caps(0)` in): still routed to the agent bridge.
            assert!(list().contains("devsbd-agentroute"), "agent-less bridge stole routing");
            Ok(())
        })
    }

    /// Docker-gated: a `Connect` over a host `Bridge` reaches a server bound to
    /// **127.0.0.1 inside the container** (only the daemon shares its network
    /// namespace) and round-trips data; a `Connect` to a closed port yields an
    /// `on_close` reason mentioning "refused". No ssh-agent needed — the bridge
    /// is agent-less, exercising the forwarding path on its own.
    #[test_utils::docker_test(helper)]
    fn forwards_a_connect_to_a_loopback_server_with_docker() -> Result<(), &'static str> {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;

        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-forward-test-{stamp}");
        // busybox nc echoes stdin back to the client (-e cat) on loopback only,
        // so the port is reachable only from inside the netns — where the daemon
        // dials. `-lk` keeps it serving across connects.
        let serve = "busybox nc -lk -s 127.0.0.1 -p 8080 -e cat";
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sh", "-c", serve])));
            let arch = install(&name, None).unwrap();
            start_daemon(&name);
            let none: Option<fn() -> Option<PathBuf>> = None;
            let bridge = spawn_with(&name, hash(arch).unwrap(), none).unwrap();
            assert_eq!(bridge.outcome(Duration::from_secs(10)), Some(Ok(())), "bridge handshake");
            assert_eq!(bridge.peer_caps(), Some(caps::TCP_FORWARD), "daemon advertises TCP_FORWARD");

            // Round-trip through the loopback echo server. Retry the connect a
            // few times: `nc -lk` may not have bound its listener the instant
            // the container starts.
            let mut round_tripped = false;
            for _ in 0..30 {
                let (mut ours, theirs) = UnixStream::pair().unwrap();
                ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let (tx, closed) = mpsc::channel();
                let Ok(_id) = bridge.connect(theirs, "127.0.0.1", 8080, move |r| {
                    let _ = tx.send(r);
                }) else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                ours.write_all(b"ping\n").unwrap();
                let mut buf = [0u8; 5];
                match ours.read_exact(&mut buf) {
                    Ok(()) if &buf == b"ping\n" => {
                        round_tripped = true;
                        break;
                    }
                    _ => {
                        // Server not up yet: this stream closed, try again.
                        let _ = closed.recv_timeout(Duration::from_secs(1));
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
            assert!(round_tripped, "loopback echo did not round-trip through the forward");

            // A Connect to a closed port comes back refused.
            let (_ours, theirs) = UnixStream::pair().unwrap();
            let (tx, closed) = mpsc::channel();
            bridge.connect(theirs, "127.0.0.1", 9, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
            let reason = closed.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(reason.contains("refused"), "closed port reason: {reason:?}");
            Ok(())
        })
    }
}
