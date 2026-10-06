//! The client side: connect to the daemon, starting it when nothing answers
//! (the lazy start), and handle a version handoff. Commands that need live
//! features call [`connect`] (steps 5-8 wire them in).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::endpoint::{self, Stream};
use super::proto::{self, HelloParams, HelloResult, Request, Response, Version};

/// How long [`connect`] waits for a daemon it started (or one handing off).
pub const START_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// A connection past the hello. `daemon` is what the daemon reported. Held
/// open, it makes its client a holder (the TUI holds one for its life).
#[allow(dead_code)] // `reader`/`daemon`: the API (step 6)
pub struct Conn {
    reader: BufReader<Stream>,
    writer: Stream,
    next_id: u64,
    pub daemon: HelloResult,
}

#[allow(dead_code)] // the API (step 6) builds on it
impl Conn {
    fn new(stream: Stream, daemon: HelloResult) -> Result<Self> {
        let reader = BufReader::new(stream.try_clone().context("cannot clone the daemon connection")?);
        Ok(Self { reader, writer: stream, next_id: 1, daemon })
    }

    /// One request, one response. No notifications exist yet (step 6 adds
    /// `subscribe`, and with it a reader that sets them aside).
    pub fn call(&mut self, method: &str, params: Value) -> Result<Response> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&Request { id: Some(id), method: method.into(), params })?;
        line.push(b'\n');
        self.writer.write_all(&line).context("cannot write to devsandbox serve")?;
        let mut reply = String::new();
        if (&mut self.reader).take(proto::MAX_LINE).read_line(&mut reply).context("cannot read from devsandbox serve")? == 0 {
            bail!("devsandbox serve closed the connection");
        }
        let response: Response = serde_json::from_str(&reply).context("bad response from devsandbox serve")?;
        if response.id != Some(id) {
            bail!("devsandbox serve answered request {:?}, expected {id}", response.id);
        }
        Ok(response)
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
        assert_eq!(conn.daemon, HelloResult { version: "0.6.0".into(), build: 1, handoff: false });
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
