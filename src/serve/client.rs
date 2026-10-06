//! The client side: connect to the daemon, starting it when nothing answers
//! (the lazy start), and handle a version handoff. Commands that need live
//! features call [`connect`] ([`ensure_bridge`] for the ssh-agent relay).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::endpoint::{self, Stream};
use super::proto::{self, HelloParams, HelloResult, Notification, Request, Response, Version};

/// How long [`connect`] waits for a daemon it started (or one handing off).
pub const START_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// A connection past the hello. `daemon` is what the daemon reported. Held
/// open, it makes its client a holder (the TUI holds one for its life).
///
/// Sync and single-threaded: [`call`](Self::call) sets aside any
/// notification that arrives before its response, and
/// [`next_notification`](Self::next_notification) hands them out, oldest
/// first, then waits for the next.
pub struct Conn {
    reader: BufReader<Stream>,
    writer: Stream,
    next_id: u64,
    #[allow(dead_code)] // read by tests; clients that check `protocol` will
    pub daemon: HelloResult,
    /// Notifications read while waiting for a response.
    notes: VecDeque<Notification>,
    /// A line cut short by a read timeout, completed by the next read.
    partial: Vec<u8>,
}

/// One line from the daemon.
enum Incoming {
    Response(Response),
    Notification(Notification),
}

impl Conn {
    fn new(stream: Stream, daemon: HelloResult) -> Result<Self> {
        let reader = BufReader::new(stream.try_clone().context("cannot clone the daemon connection")?);
        Ok(Self { reader, writer: stream, next_id: 1, daemon, notes: VecDeque::new(), partial: Vec::new() })
    }

    /// One request, one response; notifications read meanwhile are kept for
    /// [`next_notification`](Self::next_notification).
    pub fn call(&mut self, method: &str, params: Value) -> Result<Response> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&Request { id: Some(id), method: method.into(), params })?;
        line.push(b'\n');
        self.writer.write_all(&line).context("cannot write to devsandbox serve")?;
        self.reader.get_ref().set_read_timeout(None).context("cannot clear the read timeout")?;
        loop {
            match self.read()? {
                Some(Incoming::Notification(n)) => self.notes.push_back(n),
                Some(Incoming::Response(r)) if r.id == Some(id) => return Ok(r),
                Some(Incoming::Response(r)) => bail!("devsandbox serve answered request {:?}, expected {id}", r.id),
                None => bail!("timed out reading from devsandbox serve"),
            }
        }
    }

    /// The oldest notification set aside, else wait up to `timeout` for one;
    /// `None` when none came.
    pub fn next_notification(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        if let Some(n) = self.notes.pop_front() {
            return Ok(Some(n));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.reader.get_ref().set_read_timeout(Some(left)).context("cannot set a read timeout")?;
            match self.read()? {
                Some(Incoming::Notification(n)) => return Ok(Some(n)),
                Some(Incoming::Response(r)) => bail!("devsandbox serve sent a response {:?} nobody asked for", r.id),
                None => {}
            }
        }
    }

    /// One line, `None` on a read timeout (a partial line is kept for the
    /// next call). A line with a `method` is a notification.
    fn read(&mut self) -> Result<Option<Incoming>> {
        let limit = proto::MAX_LINE.saturating_sub(self.partial.len() as u64);
        match (&mut self.reader).take(limit).read_until(b'\n', &mut self.partial) {
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Ok(None),
            Err(e) => return Err(e).context("cannot read from devsandbox serve"),
        }
        if !self.partial.ends_with(b"\n") {
            if self.partial.len() as u64 >= proto::MAX_LINE {
                bail!("devsandbox serve sent a line over {} bytes", proto::MAX_LINE);
            }
            bail!("devsandbox serve closed the connection");
        }
        let line = std::mem::take(&mut self.partial);
        let value: Value = serde_json::from_slice(&line).context("bad message from devsandbox serve")?;
        Ok(Some(if value.get("method").is_some() {
            Incoming::Notification(serde_json::from_value(value).context("bad notification from devsandbox serve")?)
        } else {
            Incoming::Response(serde_json::from_value(value).context("bad response from devsandbox serve")?)
        }))
    }
}

/// The daemon's answer to a hello.
#[derive(Debug)]
pub enum Hello {
    Ready(HelloResult),
    /// This client is newer; the daemon is draining and will exit.
    Handoff,
    /// The connection closed or timed out first (the daemon is exiting).
    Closed,
}

/// Connect to the daemon in `dir`, starting it if nothing answers, and say
/// hello as `client` (`tui`, `cli`, …).
pub fn connect(dir: &Path, client: &str) -> Result<Conn> {
    connect_with(dir, client, &Version::current(), START_TIMEOUT, &spawn_detached)
}

/// Make sure a daemon runs (lazy start), for a command that just started a
/// container: its dispatcher must get a live host even with no dashboard
/// open. Best effort: a failure is one warning line on stderr (stdout stays
/// clean for `run --json`), never an error. The connection is dropped at
/// once; from then on the running instance holds the daemon, if it's one
/// that expects a host.
pub fn ensure_running(client: &str) {
    if let Err(e) = endpoint::socket_dir().and_then(|dir| connect(&dir, client)) {
        eprintln!("warning: devsandbox serve: {e:#}");
    }
}

/// Ask the daemon (lazy start, as `cli`) for instance `instance`'s bridge
/// (`bridges.ensure`, docs/api.md), reporting `agent`, the caller's own
/// `SSH_AUTH_SOCK`. Without `wait` it returns once the daemon took the
/// request; with it, once the bridge's handshake is decided or `wait` ran out,
/// with the [`Readiness`](crate::devsbd::bridge::Readiness) (`None` from a
/// daemon too old to wait). Hold the returned connection while the command
/// that needs the relay runs: it makes the command a holder.
pub fn ensure_bridge(
    instance: &str,
    agent: Option<&Path>,
    wait: Option<Duration>,
) -> Result<(Conn, Option<crate::devsbd::bridge::Readiness>)> {
    let mut conn = connect(&endpoint::socket_dir()?, "cli")?;
    // A non-UTF-8 path can't go on the wire: the daemon falls back to its own.
    let agent = agent.and_then(Path::to_str);
    let mut params = serde_json::json!({ "instance": instance, "agent": agent });
    if let Some(wait) = wait {
        params["wait"] = wait.as_secs_f64().into();
    }
    let r = conn.call("bridges.ensure", params)?;
    if let Some(e) = r.error {
        bail!("devsandbox serve: {} ({})", e.message, e.code);
    }
    let ready = readiness(&r.result.unwrap_or(Value::Null));
    Ok((conn, ready))
}

/// A waited `bridges.ensure` result's readiness; `None` without `ready` (a
/// daemon that predates `wait` answers a plain `{"ok":true}`).
fn readiness(result: &Value) -> Option<crate::devsbd::bridge::Readiness> {
    let ready = result.get("ready")?.as_bool()?;
    let error = result.get("error").and_then(Value::as_str).map(str::to_string);
    Some(crate::devsbd::bridge::Readiness { ready, error })
}

/// Connect to a daemon already running in `dir`, without the lazy start:
/// `None` when nothing answers, or the one answering is exiting (a handoff,
/// which this `version` may have just triggered). `timeout` bounds the hello.
pub(crate) fn connect_running(dir: &Path, client: &str, version: &Version, timeout: Duration) -> Result<Option<Conn>> {
    let Ok(mut stream) = endpoint::connect(dir) else { return Ok(None) };
    stream.set_read_timeout(Some(timeout)).context("cannot set a read timeout")?;
    match hello(&mut stream, version, client)? {
        Hello::Ready(daemon) => {
            stream.set_read_timeout(None).context("cannot clear the read timeout")?;
            Ok(Some(Conn::new(stream, daemon)?))
        }
        Hello::Handoff | Hello::Closed => Ok(None),
    }
}

/// [`connect`] with the version, timeout and spawner injectable for tests.
///
/// Starts at most one daemon: when the connect fails, or when the daemon
/// answers `handoff` (ours then waits for the old one's lock while it
/// drains). Then polls until `timeout`. Known gap: if an older client's
/// daemon wins that lock race, this gives up after `timeout`; the next call
/// hands off again.
pub(crate) fn connect_with(
    dir: &Path,
    client: &str,
    version: &Version,
    timeout: Duration,
    spawn: &dyn Fn(&Path) -> Result<()>,
) -> Result<Conn> {
    let deadline = Instant::now() + timeout;
    let mut spawned = false;
    loop {
        let start = match endpoint::connect(dir) {
            Ok(mut stream) => {
                let left = deadline.saturating_duration_since(Instant::now()).max(POLL);
                stream.set_read_timeout(Some(left)).context("cannot set a read timeout")?;
                match hello(&mut stream, version, client)? {
                    Hello::Ready(daemon) => {
                        stream.set_read_timeout(None).context("cannot clear the read timeout")?;
                        return Conn::new(stream, daemon);
                    }
                    Hello::Handoff => true,
                    // Exiting daemon: its lock holds off a successor, retry.
                    Hello::Closed => false,
                }
            }
            Err(_) => true,
        };
        if start && !spawned {
            spawn(dir)?;
            spawned = true;
        }
        if Instant::now() >= deadline {
            let log = endpoint::log_path().map(|p| format!("; see {}", p.display())).unwrap_or_default();
            bail!("devsandbox serve did not answer on {} within {timeout:?}{log}", endpoint::socket_path(dir).display());
        }
        std::thread::sleep(POLL);
    }
}

/// Send the hello and read its reply. I/O trouble (EOF, timeout) is
/// [`Hello::Closed`]; an error response is an error. Reads byte by byte so
/// nothing past the reply's newline is consumed before [`Conn`] buffers.
pub(crate) fn hello(stream: &mut Stream, version: &Version, client: &str) -> Result<Hello> {
    let params = HelloParams { version: version.semver.clone(), build: version.build, client: client.into() };
    let request = Request { id: Some(0), method: "hello".into(), params: serde_json::to_value(params)? };
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    if stream.write_all(&line).is_err() {
        return Ok(Hello::Closed);
    }
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) if (reply.len() as u64) < proto::MAX_LINE => reply.push(byte[0]),
            _ => return Ok(Hello::Closed),
        }
    }
    let response: Response = serde_json::from_slice(&reply).context("bad hello reply from devsandbox serve")?;
    if let Some(e) = response.error {
        bail!("devsandbox serve refused hello: {} ({})", e.message, e.code);
    }
    let result: HelloResult = serde_json::from_value(response.result.unwrap_or(Value::Null))
        .context("bad hello result from devsandbox serve")?;
    Ok(if result.handoff { Hello::Handoff } else { Hello::Ready(result) })
}

/// Start `devsandbox serve --socket-dir <dir>` detached: its own session
/// (`setsid`, so the terminal's hangup and Ctrl-C don't reach it), cwd `/`
/// (pins no directory), stdin from /dev/null, stdout+stderr appended to
/// `serve.log`.
fn spawn_detached(dir: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the devsandbox binary")?;
    let log_path = endpoint::log_path()?;
    if let Some(parent) = log_path.parent() {
        endpoint::ensure_private_dir(parent)?;
    }
    let log = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--socket-dir")
        .arg(dir)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone().context("cannot clone the log handle")?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = cmd.spawn().context("cannot start devsandbox serve")?;
    // Reap it if it exits while this process lives on (a TUI), so it
    // doesn't linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::serve::daemon::Exit;
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};

    type Daemons = Mutex<Vec<std::thread::JoinHandle<Result<Exit>>>>;

    fn v(build: u64) -> Version {
        Version { semver: "0.6.0".into(), build }
    }

    /// A spawner that runs an in-process daemon of `build`, counting calls.
    fn fake_spawn<'a>(build: u64, count: &'a AtomicUsize, daemons: &'a Daemons) -> impl Fn(&Path) -> Result<()> + 'a {
        move |dir| {
            count.fetch_add(1, Ordering::SeqCst);
            daemons.lock().unwrap().push(spawn_daemon(dir, opts(build, 200)));
            Ok(())
        }
    }

    fn join_all(daemons: Daemons) -> Vec<Exit> {
        daemons.into_inner().unwrap().into_iter().map(|d| d.join().unwrap().unwrap()).collect()
    }

    #[test]
    fn lazy_start_spawns_once_and_connects() {
        let dir = scratch("lazy");
        let (count, daemons) = (AtomicUsize::new(0), Daemons::default());
        let mut conn = connect_with(&dir, "test", &v(1), START_TIMEOUT, &fake_spawn(1, &count, &daemons)).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(conn.daemon, HelloResult { version: "0.6.0".into(), build: 1, protocol: proto::PROTOCOL, handoff: false });
        let r = conn.call("nope", Value::Null).unwrap();
        assert_eq!(r.error.unwrap().code, "unknown-method");

        // A second client finds it running: no spawn.
        let second = connect_with(&dir, "test", &v(1), START_TIMEOUT, &fake_spawn(1, &count, &daemons)).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop((conn, second));
        assert_eq!(join_all(daemons), [Exit::Idle]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lazy_start_gives_up_when_the_daemon_never_answers() {
        let dir = scratch("never");
        let count = AtomicUsize::new(0);
        let spawn = |_: &Path| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let started = Instant::now();
        let err = connect_with(&dir, "test", &v(1), Duration::from_millis(300), &spawn).err().unwrap();
        assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert_eq!(count.load(Ordering::SeqCst), 1, "spawned more than once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn waited_ensure_results_parse() {
        use crate::devsbd::bridge::Readiness;
        assert_eq!(readiness(&serde_json::json!({"ok": true})), None, "older daemon");
        assert_eq!(
            readiness(&serde_json::json!({"ok": true, "ready": true, "error": null})),
            Some(Readiness { ready: true, error: None })
        );
        assert_eq!(
            readiness(&serde_json::json!({"ok": true, "ready": false, "error": "outdated"})),
            Some(Readiness { ready: false, error: Some("outdated".into()) })
        );
    }

    #[test]
    fn spawn_failure_is_reported() {
        let dir = scratch("spawnfail");
        let err = connect_with(&dir, "test", &v(1), START_TIMEOUT, &|_| bail!("no exe")).err().unwrap();
        assert_eq!(format!("{err:#}"), "no exe");
    }

    #[test]
    fn newer_client_hands_off_and_reaches_its_own_daemon() {
        let dir = scratch("cl-handoff");
        let old = spawn_daemon(&dir, opts(1, 60_000));
        while endpoint::connect(&dir).is_err() {
            std::thread::sleep(POLL);
        }
        let (count, daemons) = (AtomicUsize::new(0), Daemons::default());
        // Older/equal clients use the old daemon as is.
        let equal = connect_with(&dir, "old", &v(1), START_TIMEOUT, &fake_spawn(1, &count, &daemons)).unwrap();
        assert_eq!((equal.daemon.build, count.load(Ordering::SeqCst)), (1, 0));
        drop(equal);

        let conn = connect_with(&dir, "new", &v(2), START_TIMEOUT, &fake_spawn(2, &count, &daemons)).unwrap();
        assert_eq!(conn.daemon.build, 2);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(old.join().unwrap().unwrap(), Exit::Handoff);
        drop(conn);
        assert_eq!(join_all(daemons), [Exit::Idle]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
