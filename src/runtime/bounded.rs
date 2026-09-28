//! Child processes with bounded output and a wall-clock timeout, for host
//! calls whose output or duration a container controls (dispatcher run ops,
//! the TUI's `devsbd run ls`, the dispatch `devsandbox` subprocess). Silent:
//! nothing is inherited, so it's safe while the TUI owns the terminal, and
//! every killed child is reaped.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Stderr kept from a [`run`] child; the rest is drained and dropped. It only
/// feeds error messages.
const STDERR_CAP: usize = 64 * 1024;

/// Longest gap between liveness polls while waiting on a child.
const MAX_POLL: Duration = Duration::from_millis(100);

/// How long [`run`] waits for a reaped child's pipes to close; only a
/// grandchild still holding them can take longer.
const PIPE_GRACE: Duration = Duration::from_secs(1);

/// What a [`run`] child left behind.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    /// At most the cap passed to [`run`].
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Stdout went past the cap: the child was killed and `stdout` is a
    /// prefix, not the whole output.
    pub truncated: bool,
}

#[derive(Debug)]
pub enum Error {
    /// Couldn't spawn or wait on the child.
    Io(io::Error),
    /// Still running at the timeout: killed and reaped.
    TimedOut(Duration),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::TimedOut(d) => write!(f, "timed out after {}", human(*d)),
        }
    }
}

/// `d` for messages: whole minutes as `N min`, else seconds.
pub fn human(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 && secs % 60 == 0 { format!("{} min", secs / 60) } else { format!("{secs} s") }
}

/// Run `cmd` (stdin as the caller set it) with stdout and stderr piped:
/// stdout is kept up to `cap` bytes — one more kills the child and sets
/// `truncated` — and a child still running after `timeout` is killed and
/// reaped ([`Error::TimedOut`]).
pub fn run(cmd: &mut Command, cap: usize, timeout: Duration) -> Result<Output, Error> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(Error::Io)?;
    let over = Arc::new(AtomicBool::new(false));
    let stdout = read_capped(child.stdout.take(), cap, Some(Arc::clone(&over)));
    let stderr = read_capped(child.stderr.take(), STDERR_CAP, None);
    let status = match wait_until(&mut child, Instant::now() + timeout, || over.load(Ordering::Acquire)) {
        Ok(Some(status)) => status,
        Ok(None) if over.load(Ordering::Acquire) => {
            let _ = child.kill();
            child.wait().map_err(Error::Io)?
        }
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::TimedOut(timeout));
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Io(e));
        }
    };
    let (stdout, done) = match stdout.recv_timeout(PIPE_GRACE) {
        Ok(out) => (out, true),
        Err(_) => (Vec::new(), false),
    };
    let stderr = stderr.recv_timeout(PIPE_GRACE).unwrap_or_default();
    // A pipe still open past the grace (a grandchild kept it) means stdout
    // may be incomplete: report it as truncated rather than cut silently.
    let truncated = over.load(Ordering::Acquire) || !done;
    Ok(Output { status, stdout, stderr, truncated })
}

/// Read `pipe` on its own thread: up to `cap` bytes are sent back once it
/// closes or overflows. Past `cap` with `over` set (stdout): flag it and drop
/// the pipe, so writers get EPIPE while the caller kills the child. Without
/// `over` (stderr): drain the rest after sending, so the child never blocks.
/// Sending before draining keeps a grandchild that inherited the pipe from
/// withholding the prefix.
fn read_capped<R: Read + Send + 'static>(pipe: Option<R>, cap: usize, over: Option<Arc<AtomicBool>>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let Some(mut pipe) = pipe else {
        let _ = tx.send(Vec::new());
        return rx;
    };
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut pipe).take(cap as u64 + 1).read_to_end(&mut buf);
        if buf.len() <= cap {
            let _ = tx.send(buf);
            return;
        }
        buf.truncate(cap);
        if let Some(over) = &over {
            over.store(true, Ordering::Release);
        }
        let _ = tx.send(buf);
        if over.is_none() {
            let _ = io::copy(&mut pipe, &mut io::sink());
        }
    });
    rx
}

/// Wait for `child` until `deadline` or until `stop` says to give up:
/// `Some(status)` once it exited (reaped), `None` when it's still running.
pub fn wait_until(child: &mut Child, deadline: Instant, stop: impl Fn() -> bool) -> io::Result<Option<ExitStatus>> {
    let mut poll = Duration::from_millis(5);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if stop() || now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(poll.min(deadline - now));
        poll = (poll * 2).min(MAX_POLL);
    }
}

/// Kill `child`'s whole process group (on unix it must have been spawned with
/// `process_group(0)`, so its pgid is its pid) and reap it, so grandchildren
/// (docker, git) die with it. Elsewhere only `child` itself is killed.
pub fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: a plain syscall on integers; a negative pid addresses
            // the process group. The child isn't reaped yet, so its pid (and
            // thus the group id) can't have been reused.
            unsafe {
                kill(-pid, SIGKILL);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]).stdin(Stdio::null());
        cmd
    }

    #[test]
    fn normal_output_and_status() {
        let out = run(&mut sh("printf hello; printf oops >&2; exit 3"), 16, Duration::from_secs(10)).unwrap();
        assert_eq!(out.stdout, b"hello");
        assert_eq!(out.stderr, b"oops");
        assert_eq!(out.status.code(), Some(3));
        assert!(!out.truncated);
        // Exactly the cap is not truncated.
        let out = run(&mut sh("printf 12345678"), 8, Duration::from_secs(10)).unwrap();
        assert_eq!((out.stdout.as_slice(), out.truncated), (&b"12345678"[..], false));
    }

    #[test]
    fn output_past_the_cap_is_truncated_and_the_child_killed() {
        let start = Instant::now();
        let out = run(&mut sh("yes"), 1000, Duration::from_secs(30)).unwrap();
        assert!(out.truncated);
        assert_eq!(out.stdout.len(), 1000);
        assert!(!out.status.success());
        assert!(start.elapsed() < Duration::from_secs(10), "killed, not left to run: {:?}", start.elapsed());
    }

    #[test]
    fn timeout_kills_and_reaps() {
        let start = Instant::now();
        let err = run(&mut sh("exec sleep 30"), 16, Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, Error::TimedOut(_)), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(err.to_string(), "timed out after 0 s");
    }

    #[test]
    fn spawn_failure_is_io() {
        let err = run(&mut Command::new("/nonexistent/devsandbox-test"), 16, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn human_durations() {
        assert_eq!(human(Duration::from_secs(1800)), "30 min");
        assert_eq!(human(Duration::from_secs(330)), "330 s");
        assert_eq!(human(Duration::from_secs(5)), "5 s");
    }

    #[cfg(unix)]
    #[test]
    fn kill_group_takes_grandchildren() {
        use std::os::unix::process::CommandExt;
        // The grandchild `sleep` holds the stdout pipe: it only closes once
        // the whole group is dead.
        let mut child = sh("sleep 30 & wait").process_group(0).stdout(Stdio::piped()).spawn().unwrap();
        let mut pipe = child.stdout.take().unwrap();
        let deadline = Instant::now() + Duration::from_millis(200);
        assert!(wait_until(&mut child, deadline, || false).unwrap().is_none());
        kill_group(&mut child);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(pipe.read_to_end(&mut Vec::new()).is_ok());
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(true), "grandchild died, pipe closed");
    }
}
