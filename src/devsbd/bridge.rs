//! Host half of the ssh-agent relay (docs/sandbox-helper.md): runs
//! `exec -i <c> devsbd bridge` and serves the streams it carries, connecting
//! each `Open` to the host agent. Owners: CLI `exec` (for the command's
//! lifetime) and the TUI (one per running instance while open).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use super::{proto, BIN};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// Whether `info` should get a bridge: the helper runs in its container and
/// the ssh-agent bind mount isn't in place (it occupies the socket path the
/// daemon listens on). Step 6 of docs/sandbox-helper.md replaces this with an
/// explicit ssh mode.
pub fn wanted(info: &Instance) -> bool {
    info.devsbd_arch.is_some() && info.ssh_auth_sock.is_none()
}

/// A running bridge; dropping it kills the `exec` (the in-container bridge
/// then sees stdin EOF and exits).
pub struct Bridge {
    child: Child,
    done: Arc<AtomicBool>,
    handshake: mpsc::Receiver<Result<(), String>>,
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
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The host agent socket, read per stream so agent rotation needs no restart.
fn host_agent() -> Option<PathBuf> {
    std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from)
}

/// Start a bridge for `info` without waiting for its handshake (see
/// `outcome`). Fully quiet (stderr captured), since the TUI owns the
/// screen; `None` only when the `exec` can't even be spawned.
pub fn spawn(info: &Instance) -> Option<Bridge> {
    let hash = info.devsbd_arch.and_then(super::hash).unwrap_or_default();
    spawn_with(&info.container, hash, host_agent)
}

fn spawn_with(
    container: &str,
    hash: &str,
    agent: impl Fn() -> Option<PathBuf> + Send + 'static,
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
    let done = Arc::new(AtomicBool::new(false));
    let (tx, handshake) = mpsc::channel();
    let hash = hash.to_string();
    let finished = Arc::clone(&done);
    std::thread::spawn(move || {
        let result = proto::handshake(&mut stdout, &mut stdin, &hash).map(|_| ()).map_err(|e| {
            // The bridge died before `Hello` (no daemon, no binary): its stderr
            // says why. Otherwise (version mismatch) our own error is the
            // right-way-round one.
            let mut msg = String::new();
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                let _ = std::io::Read::read_to_string(&mut stderr, &mut msg);
            }
            match msg.trim() {
                "" => e.to_string(),
                m => m.trim_start_matches("devsbd: ").to_string(),
            }
        });
        let ok = result.is_ok();
        let _ = tx.send(result);
        if ok {
            let mux = super::mux::Mux::new(stdin);
            mux.serve(stdout, |_, channel| {
                if channel != proto::channel::SSH_AGENT {
                    return None;
                }
                std::os::unix::net::UnixStream::connect(agent()?).ok()
            });
        }
        finished.store(true, Ordering::Relaxed);
    });
    Some(Bridge { child, done, handshake })
}

/// Minimum gap between bridge attempts for one container, so a container
/// whose bridge keeps failing (daemon down) isn't re-exec'd every tick.
const RETRY: Duration = Duration::from_secs(10);

/// The TUI's set of bridges, reconciled against the running instances on each
/// snapshot.
#[derive(Default)]
pub struct Bridges {
    live: HashMap<String, (Bridge, Instant)>,
}

impl Bridges {
    /// Keep one bridge per running container in `running` whose instance
    /// wants one; drop the rest. Dead bridges are retried after `RETRY`.
    pub fn reconcile(&mut self, running: &[&str]) {
        self.live.retain(|c, _| running.contains(&c.as_str()));
        let Ok(state) = State::load() else { return };
        for info in state.instances.values() {
            if !running.contains(&info.container.as_str()) || !wanted(info) {
                continue;
            }
            let retry_due = match self.live.get(&info.container) {
                None => true,
                Some((b, at)) => b.is_done() && at.elapsed() >= RETRY,
            };
            if retry_due {
                self.live.remove(&info.container);
                if let Some(b) = spawn(info) {
                    self.live.insert(info.container.clone(), (b, Instant::now()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::{hash, install, start_daemon, Arch};
    use std::process::Command;

    fn ok(cmd: &mut Command) -> bool {
        matches!(cmd.output(), Ok(o) if o.status.success())
    }

    /// Docker-gated end to end: a real `ssh-add -l` in an alpine container
    /// lists a key held by a host `ssh-agent`, through daemon + bridge.
    /// Also checks the daemon falls back to the older bridge when the newest
    /// one ends. Skips without docker, an embedded helper, host
    /// ssh-agent/ssh-keygen, or network for `apk add`.
    #[test]
    fn relays_host_agent_into_container_with_docker() {
        let skip = |why: &str| eprintln!("skipping relays_host_agent_into_container_with_docker: {why}");
        if !ok(Command::new("docker").arg("info")) || hash(Arch::host()).is_none() {
            return skip("docker or embedded helper unavailable");
        }
        if !ok(Command::new("ssh-agent").arg("-h")) && !ok(Command::new("which").arg("ssh-agent")) {
            return skip("no host ssh-agent");
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
        let cleanup = |agent: &mut Child| {
            let _ = agent.kill();
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
            let _ = std::fs::remove_dir_all(&tmp);
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
                spawn_with(&name, hash(arch).unwrap(), move || Some(agent_path.clone())).unwrap()
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

            // No daemon: the handshake fails with the bridge's own reason.
            let orphan = bridge().outcome(Duration::from_secs(10)).expect("handshake finished");
            assert!(orphan.unwrap_err().contains("daemon not running"));

            start_daemon(&name);
            // Optimistic start: the client connects before any bridge exists
            // and is held until one attaches.
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
            Ok(())
        }));
        cleanup(&mut agent);
        match result {
            Ok(Ok(())) => {}
            Ok(Err(why)) => skip(why),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}
