//! The runtime's container event stream (`docker events`, `podman events`;
//! [`crate::runtime::Backend::events_argv`]) as wake-ups: one long-lived
//! child, idle until a container changes, instead of a listing on a timer.
//!
//! The watcher only says *that* something changed ([`relevant`] lines) and
//! whether the stream is up; the poll re-lists and works out what.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::runtime::bounded::kill_group;
use crate::runtime::events::{parse, relevant};

/// Gap between a stream ending (or failing to start) and the next try.
pub const EVENTS_RETRY: Duration = Duration::from_secs(10);

type Stop = Arc<(Mutex<bool>, Condvar)>;

/// The `serve-events` thread and its child. Dropping it kills the child and
/// joins the thread.
pub struct EventWatcher {
    live: Arc<AtomicBool>,
    stop: Stop,
    /// The current child, shared so `Drop` can kill it: that closes its
    /// stdout, which is what unblocks the thread's read.
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl EventWatcher {
    /// Follow `program argv…` (no argv: no thread, never live). `wake` is
    /// called on each relevant event and on every up/down change of the
    /// stream (so a listing follows each subscribe and each loss); `log`
    /// writes one `serve.log` line; `retry` is the gap before a respawn
    /// ([`EVENTS_RETRY`]).
    pub fn start(
        program: &str,
        argv: Option<Vec<String>>,
        wake: Box<dyn Fn() + Send>,
        log: fn(&str),
        retry: Duration,
    ) -> EventWatcher {
        let mut watcher = EventWatcher {
            live: Arc::default(),
            stop: Arc::default(),
            child: Arc::default(),
            thread: None,
        };
        let Some(argv) = argv else { return watcher };
        let program = program.to_string();
        let (live, stop, child) = (Arc::clone(&watcher.live), Arc::clone(&watcher.stop), Arc::clone(&watcher.child));
        let spawned = std::thread::Builder::new()
            .name("serve-events".into())
            .spawn(move || follow(&program, &argv, &*wake, log, retry, &live, &stop, &child));
        match spawned {
            Ok(t) => watcher.thread = Some(t),
            Err(e) => log(&format!("cannot start the container events thread: {e}")),
        }
        watcher
    }

    /// Whether the event stream's child is running, i.e. changes are being
    /// pushed. False with no stream, between retries, and after a loss.
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
}

impl Drop for EventWatcher {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.stop;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cvar.notify_all();
        // Set before taking the slot: the thread spawns only under the slot's
        // lock after checking `stop`, so no child can appear after this.
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            kill_group(&mut child);
            // The thread finds the slot empty and won't reap it: do it here.
            let _ = child.wait();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn stopped(stop: &(Mutex<bool>, Condvar)) -> bool {
    *stop.0.lock().unwrap_or_else(|e| e.into_inner())
}

/// The thread: spawn, read, and on the end wait `retry` and respawn, until
/// stopped. The outage is logged once when the stream goes down, and its
/// end once a respawned stream delivers its first line: a child that starts
/// but exits at once (podman with `events_logger = "none"`) isn't "back".
#[allow(clippy::too_many_arguments)]
fn follow(
    program: &str,
    argv: &[String],
    wake: &dyn Fn(),
    log: fn(&str),
    retry: Duration,
    live: &AtomicBool,
    stop: &(Mutex<bool>, Condvar),
    slot: &Mutex<Option<Child>>,
) {
    let mut down = false;
    loop {
        let stdout = {
            let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            if stopped(stop) {
                return;
            }
            let mut cmd = Command::new(program);
            cmd.args(argv).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
            // Its own group, so `Drop`'s kill reaches anything it forks.
            std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
            match cmd.spawn() {
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    *slot = Some(child);
                    stdout
                }
                Err(e) => {
                    if !down {
                        log(&format!("cannot run {program} events ({e}); polling, retrying every {}s", retry.as_secs()));
                        down = true;
                    }
                    None
                }
            }
        };
        if let Some(stdout) = stdout {
            live.store(true, Ordering::Release);
            // Events from before the subscribe are lost: re-list now.
            wake();
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if down {
                    log(&format!("{program} events flow again"));
                    down = false;
                }
                if parse(&String::from_utf8_lossy(&line)).is_some_and(|e| relevant(&e)) {
                    wake();
                }
            }
            live.store(false, Ordering::Release);
            // Reap it, unless `Drop` took it to kill it.
            let status = slot.lock().unwrap_or_else(|e| e.into_inner()).take().map(|mut c| {
                kill_group(&mut c);
                c.wait()
            });
            if stopped(stop) {
                return;
            }
            // Whatever changed meanwhile goes unpushed: re-list now.
            wake();
            if !down {
                let how = match status {
                    Some(Ok(s)) => format!(" ({s})"),
                    _ => String::new(),
                };
                log(&format!("{program} events ended{how}; polling, retrying every {}s", retry.as_secs()));
                down = true;
            }
        }
        let (lock, cvar) = stop;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = cvar.wait_timeout_while(guard, retry, |s| !*s).unwrap_or_else(|e| e.into_inner());
        if *guard {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    fn quiet(_: &str) {}

    const START: &str = r#"{"Type":"container","Action":"start","Actor":{"ID":"a","Attributes":{"name":"devsandbox-x"}}}"#;
    const EXEC: &str = r#"{"Type":"container","Action":"exec_start: sh -c true","Actor":{"ID":"a","Attributes":{"name":"devsandbox-x"}}}"#;

    /// A watcher over `sh -c script`, its wakes counted.
    fn watch(script: &str, retry: Duration) -> (EventWatcher, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let wake = Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let argv = vec!["-c".to_string(), script.to_string()];
        (EventWatcher::start("sh", Some(argv), wake, quiet, retry), wakes)
    }

    fn until(what: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn relevant_lines_wake_and_noise_does_not() {
        // Subscribe (1), START, EXEC (no), START, garbage (no), then blocked.
        let script = format!("printf '%s\\n' '{START}' '{EXEC}' '{START}' 'garbage'; exec sleep 1000");
        let (w, wakes) = watch(&script, Duration::from_secs(60));
        assert!(until(|| wakes.load(Ordering::SeqCst) >= 3));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(wakes.load(Ordering::SeqCst), 3);
        assert!(w.is_live());
    }

    #[test]
    fn an_ended_stream_is_down_woken_and_respawned_after_the_gap() {
        let (tx, rx) = mpsc::channel::<bool>();
        let live = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
        let seen = Arc::clone(&live);
        let wake = Box::new(move || {
            if let Some(l) = &*seen.lock().unwrap() {
                let _ = tx.send(l.load(Ordering::SeqCst));
            }
        });
        let argv = vec!["-c".to_string(), "exit 3".to_string()];
        // Hold the wake until the watcher's flag is wired in.
        let slot = live.lock().unwrap();
        let w = EventWatcher::start("sh", Some(argv), wake, quiet, Duration::from_millis(300));
        let mut slot = slot;
        *slot = Some(Arc::clone(&w.live));
        drop(slot);
        let t0 = Instant::now();
        let wait = |rx: &mpsc::Receiver<bool>| rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(wait(&rx), "up on spawn");
        assert!(!wait(&rx), "down on exit");
        assert!(wait(&rx), "respawned");
        assert!(t0.elapsed() >= Duration::from_millis(300), "{:?}", t0.elapsed());
        assert!(!wait(&rx));
        assert!(until(|| !w.is_live()));
    }

    #[test]
    fn drop_kills_a_blocked_child_promptly() {
        // Not `exec`: the kill must reach the forked `sleep` holding stdout.
        let (w, wakes) = watch("sleep 1000; true", Duration::from_secs(60));
        assert!(until(|| w.is_live() && wakes.load(Ordering::SeqCst) == 1));
        let t0 = Instant::now();
        drop(w);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        // Stopped: no wake for the loss.
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn no_argv_is_never_live() {
        let w = EventWatcher::start("sh", None, Box::new(|| panic!("woken")), quiet, Duration::from_millis(1));
        assert!(w.thread.is_none());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!w.is_live());
    }

    #[test]
    fn a_missing_program_is_down() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let wake = Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let argv = Some(vec!["events".to_string()]);
        let w = EventWatcher::start("/nonexistent/devsandbox-runtime", argv, wake, quiet, Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!w.is_live());
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
    }

    /// Against the real `docker events`: a devsandbox container starting
    /// wakes the watcher.
    #[test_utils::docker_test]
    fn docker_events_wake_on_a_container_start() -> Result<(), &'static str> {
        use crate::runtime::{Backend, Dockerlike};
        let rt = Dockerlike::DOCKER;
        let argv = rt.events_argv().map(|a| a.into_iter().map(String::from).collect());
        let (tx, rx) = mpsc::channel::<()>();
        let tx = Mutex::new(tx);
        let wake = Box::new(move || {
            let _ = tx.lock().unwrap().send(());
        });
        let w = EventWatcher::start(rt.bin(), argv, wake, quiet, EVENTS_RETRY);
        if !until(|| w.is_live()) {
            return Err("docker events did not start");
        }
        // The subscribe's wake.
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "no wake on subscribe")?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-evtest-{stamp}");
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            let up = Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "30"]).output();
            if !matches!(up, Ok(o) if o.status.success()) {
                return Err("docker run alpine:3.20 failed");
            }
            assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok(), "no wake for {name}");
            assert!(w.is_live());
            Ok(())
        })
    }
}
