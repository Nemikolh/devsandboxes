//! The daemon side of `devsandbox serve`: start lock, accept loop, version
//! handoff and idle exit (docs/serve.md).
//!
//! Threading: the calling thread runs the accept loop, waking every [`TICK`]
//! to re-check the idle countdown and the handoff flag; each connection gets
//! its own thread reading JSON lines. Exiting (idle or handoff) closes the
//! listener and removes the socket first, then shuts down the read side of
//! every connection, so idle ones end at once while a request in flight still
//! writes its response, and waits up to [`DRAIN_TIMEOUT`] for them. The start
//! lock is released last, so a successor never binds while this one drains.

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::endpoint::{self, Listener, Stream};
use super::idle::{self, Decision, Holders};
use super::proto::{self, HelloParams, HelloResult, Request, Response, Version};

/// How often the accept loop wakes without a connection.
const TICK: Duration = Duration::from_millis(100);
/// How long an exiting daemon waits for in-flight requests.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a starting daemon waits for a lock whose holder doesn't answer
/// (starting up, or draining after a handoff). Longer than [`DRAIN_TIMEOUT`].
const LOCK_WAIT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(50);

pub struct Options {
    pub version: Version,
    /// No idle exit. `serve.keep-alive = true` in a global config will set
    /// this too once one exists (there is none yet: config is per `-C` root,
    /// the daemon per user); `serve install` (step 10) implies it.
    pub keep_alive: bool,
    /// [`idle::IDLE_TIMEOUT`] outside tests.
    pub idle_timeout: Duration,
}

impl Options {
    pub fn new(keep_alive: bool) -> Self {
        Self { version: Version::current(), keep_alive, idle_timeout: idle::IDLE_TIMEOUT }
    }
}

/// Why [`run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Another daemon holds the lock and answers: nothing to do.
    AlreadyRunning,
    Idle,
    /// A newer client asked; it starts the successor.
    Handoff,
}

/// `devsandbox serve`: run the daemon in the foreground until it idles out or
/// hands off. `socket_dir` overrides [`endpoint::socket_dir`] (the lazy start
/// passes the dir its client resolved, so both agree).
pub fn serve(socket_dir: Option<PathBuf>, keep_alive: bool) -> Result<()> {
    let dir = match socket_dir {
        Some(dir) => dir,
        None => endpoint::socket_dir()?,
    };
    let opts = Options::new(keep_alive);
    match run(&dir, &opts)? {
        Exit::AlreadyRunning => {}
        Exit::Idle => log("idle, exiting"),
        Exit::Handoff => log("handed off to a newer version, exiting"),
    }
    Ok(())
}

/// Run a daemon on `dir` (created 0700 if needed). Returns at once with
/// [`Exit::AlreadyRunning`] when another daemon owns the dir.
pub fn run(dir: &Path, opts: &Options) -> Result<Exit> {
    endpoint::ensure_private_dir(dir)?;
    let Some(_lock) = acquire_lock(dir)? else {
        return Ok(Exit::AlreadyRunning);
    };
    let listener = Listener::bind(dir)?;
    log(&format!(
        "listening on {} (pid {}, version {} build {}{})",
        endpoint::socket_path(dir).display(),
        std::process::id(),
        opts.version.semver,
        opts.version.build,
        if opts.keep_alive { ", keep-alive" } else { "" },
    ));

    let shared = Arc::new(Shared::new(opts.version.clone()));
    let mut next_conn = 0u64;
    let exit = loop {
        if shared.handoff.load(Ordering::SeqCst) {
            break Exit::Handoff;
        }
        let now = Instant::now();
        let holders = shared.holders();
        if holders.any() {
            *shared.idle_since.lock().unwrap() = now;
        }
        let idle_since = *shared.idle_since.lock().unwrap();
        if idle::decide(&holders, idle_since, now, opts.keep_alive, opts.idle_timeout) == Decision::Exit {
            break Exit::Idle;
        }
        match listener.accept_timeout(TICK) {
            Ok(Some(stream)) => {
                next_conn += 1;
                shared.spawn_conn(next_conn, stream);
            }
            Ok(None) => {}
            Err(e) => {
                // EMFILE and friends: don't spin, the next tick retries.
                log(&format!("accept failed: {e}"));
                std::thread::sleep(TICK);
            }
        }
    };

    // Stop accepting before anything else, then release the handoff replies:
    // once a newer client reads `handoff`, nothing answers on the socket, so
    // the successor it spawns waits for this lock instead of seeing a live
    // daemon and giving up.
    listener.remove();
    shared.close();
    shared.drain(DRAIN_TIMEOUT);
    Ok(exit)
}

/// Take the start lock, or `None` when a live daemon (one answering on the
/// socket) holds it. A holder that doesn't answer is starting up or draining
/// after a handoff; wait for it, up to [`LOCK_WAIT`].
fn acquire_lock(dir: &Path) -> Result<Option<File>> {
    let path = endpoint::lock_path(dir);
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e).with_context(|| format!("cannot lock {}", path.display())),
        }
        if endpoint::connect(dir).is_ok() {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            bail!(
                "{} is locked but nothing answers on {}",
                path.display(),
                endpoint::socket_path(dir).display()
            );
        }
        std::thread::sleep(LOCK_POLL);
    }
}

struct Shared {
    version: Version,
    /// Live connections (a clone of each stream, for the drain's shutdown).
    /// Its size is the `clients` holder count.
    conns: Mutex<HashMap<u64, Stream>>,
    /// The last moment a holder was seen.
    idle_since: Mutex<Instant>,
    /// A newer client asked for a handoff.
    handoff: AtomicBool,
    /// The listener is gone; handoff replies may go out.
    closed: (Mutex<bool>, Condvar),
}

impl Shared {
    fn new(version: Version) -> Self {
        Self {
            version,
            conns: Mutex::new(HashMap::new()),
            idle_since: Mutex::new(Instant::now()),
            handoff: AtomicBool::new(false),
            closed: (Mutex::new(false), Condvar::new()),
        }
    }

    fn holders(&self) -> Holders {
        Holders { clients: self.conns.lock().unwrap().len(), ..Default::default() }
    }

    /// Register before the thread starts, so the next holder snapshot counts
    /// it even if the client is gone by then.
    fn spawn_conn(self: &Arc<Self>, id: u64, stream: Stream) {
        let Ok(clone) = stream.try_clone() else { return };
        self.conns.lock().unwrap().insert(id, clone);
        let shared = Arc::clone(self);
        let spawned = std::thread::Builder::new().name(format!("serve-conn-{id}")).spawn(move || {
            serve_conn(stream, &shared);
            shared.conns.lock().unwrap().remove(&id);
            *shared.idle_since.lock().unwrap() = Instant::now();
        });
        if spawned.is_err() {
            self.conns.lock().unwrap().remove(&id);
        }
    }

    fn close(&self) {
        let (lock, cvar) = &self.closed;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    /// Flag the handoff and wait (bounded) until the accept loop has closed
    /// the listener; see the comment at the end of [`run`].
    fn begin_handoff(&self) {
        self.handoff.store(true, Ordering::SeqCst);
        let (lock, cvar) = &self.closed;
        let guard = lock.lock().unwrap();
        let _ = cvar.wait_timeout_while(guard, Duration::from_secs(2), |closed| !*closed);
    }

    fn drain(&self, timeout: Duration) {
        for stream in self.conns.lock().unwrap().values() {
            let _ = stream.shutdown(Shutdown::Read);
        }
        let deadline = Instant::now() + timeout;
        while !self.conns.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// One connection: a request per line, a response per request, until EOF.
fn serve_conn(stream: Stream, shared: &Shared) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(proto::MAX_LINE).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let too_long = !line.ends_with(b"\n") && line.len() as u64 >= proto::MAX_LINE;
        let response = if too_long {
            Response::err(None, "invalid", "request line too long")
        } else if line.trim_ascii().is_empty() {
            continue;
        } else {
            handle(&line, shared)
        };
        let Ok(mut out) = serde_json::to_vec(&response) else { return };
        out.push(b'\n');
        if writer.write_all(&out).and_then(|()| writer.flush()).is_err() || too_long {
            return;
        }
    }
}

fn handle(line: &[u8], shared: &Shared) -> Response {
    let req: Request = match serde_json::from_slice(line) {
        Ok(req) => req,
        Err(e) => return Response::err(None, "invalid", format!("not a request: {e}")),
    };
    match req.method.as_str() {
        "hello" => hello(req.id, req.params, shared),
        other => Response::err(req.id, "unknown-method", format!("unknown method `{other}`")),
    }
}

fn hello(id: Option<u64>, params: Value, shared: &Shared) -> Response {
    let params: HelloParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "invalid", format!("bad hello params: {e}")),
    };
    let client = Version { semver: params.version, build: params.build };
    let handoff = client.is_newer_than(&shared.version);
    if handoff {
        log(&format!(
            "client `{}` has version {} build {}: handing off",
            params.client, client.semver, client.build
        ));
        shared.begin_handoff();
    }
    let result = HelloResult { version: shared.version.semver.clone(), build: shared.version.build, handoff };
    Response::ok(id, serde_json::to_value(result).unwrap_or(Value::Null))
}

/// One line to stderr, which the lazy start points at `serve.log`.
fn log(msg: &str) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    eprintln!("[{secs}] devsandbox serve: {msg}");
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::serve::client::{self, Hello};

    pub(crate) fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    pub(crate) fn opts(build: u64, idle_ms: u64) -> Options {
        Options {
            version: Version { semver: "0.6.0".into(), build },
            keep_alive: false,
            idle_timeout: Duration::from_millis(idle_ms),
        }
    }

    pub(crate) fn spawn_daemon(dir: &Path, opts: Options) -> std::thread::JoinHandle<Result<Exit>> {
        let dir = dir.to_owned();
        std::thread::spawn(move || run(&dir, &opts))
    }

    fn v(build: u64) -> Version {
        Version { semver: "0.6.0".into(), build }
    }

    /// Connect (retrying while the daemon binds) and say hello.
    fn hello_at(dir: &Path, version: &Version) -> (Stream, Hello) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stream) = endpoint::connect(dir) {
                let mut stream = stream;
                let hello = client::hello(&mut stream, version, "test").unwrap();
                return (stream, hello);
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn concurrent_starts_have_one_winner_and_all_clients_reach_it() {
        let dir = scratch("race");
        let daemons: Vec<_> = (0..8).map(|_| spawn_daemon(&dir, opts(1, 300))).collect();
        let clients: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || hello_at(&dir, &v(1)).1)
            })
            .collect();
        for c in clients {
            match c.join().unwrap() {
                Hello::Ready(result) => assert_eq!(result.build, 1),
                other => panic!("{other:?}"),
            }
        }
        let mut exits: Vec<_> = daemons.into_iter().map(|d| d.join().unwrap().unwrap()).collect();
        exits.sort_by_key(|e| *e != Exit::Idle);
        assert_eq!(exits[0], Exit::Idle);
        assert!(exits[1..].iter().all(|e| *e == Exit::AlreadyRunning), "{exits:?}");
        assert!(!endpoint::socket_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_hello_hands_off_drains_and_exits() {
        let dir = scratch("handoff");
        let daemon = spawn_daemon(&dir, opts(1, 60_000));

        // Equal and older clients just proceed.
        let (mut idle_client, equal) = hello_at(&dir, &v(1));
        assert!(matches!(equal, Hello::Ready(_)), "{equal:?}");
        let (_older_conn, older) = hello_at(&dir, &Version { semver: "0.5.9".into(), build: 9 });
        assert!(matches!(older, Hello::Ready(_)), "{older:?}");

        let started = Instant::now();
        let (_newer_conn, newer) = hello_at(&dir, &v(2));
        assert!(matches!(newer, Hello::Handoff), "{newer:?}");
        // Nothing answers once the handoff reply is out.
        assert!(endpoint::connect(&dir).is_err());
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        // Idle connections are closed, not waited out.
        assert!(started.elapsed() < DRAIN_TIMEOUT);
        let mut buf = [0u8; 1];
        assert_eq!(idle_client.read(&mut buf).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_methods_and_garbage_get_errors_with_ids() {
        let dir = scratch("unknown");
        let daemon = spawn_daemon(&dir, opts(1, 200));
        let (stream, _) = hello_at(&dir, &v(1));
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut ask = |line: &str| {
            writer.write_all(format!("{line}\n").as_bytes()).unwrap();
            let mut out = String::new();
            reader.read_line(&mut out).unwrap();
            serde_json::from_str::<Value>(&out).unwrap()
        };
        let r = ask(r#"{"id":7,"method":"inbox.threads.list"}"#);
        assert_eq!(r["id"], 7);
        assert_eq!(r["error"]["code"], "unknown-method");
        let r = ask("not json");
        assert_eq!(r["error"]["code"], "invalid");
        let r = ask(r#"{"id":8,"method":"hello","params":{"nope":1}}"#);
        assert_eq!((r["id"].as_u64(), r["error"]["code"].as_str()), (Some(8), Some("invalid")));
        drop(ask);
        drop(writer);
        drop(reader);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_connected_client_holds_and_keep_alive_never_idles() {
        let dir = scratch("hold");
        let daemon = spawn_daemon(&dir, opts(1, 150));
        let (conn, _) = hello_at(&dir, &v(1));
        std::thread::sleep(Duration::from_millis(500));
        assert!(!daemon.is_finished(), "exited while a client was connected");
        drop(conn);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);

        let keep = Options { keep_alive: true, ..opts(1, 50) };
        let daemon = spawn_daemon(&dir, keep);
        let _ = hello_at(&dir, &v(1));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!daemon.is_finished(), "keep-alive daemon idled out");
        // Stop it the only way there is: a handoff.
        let (_c, h) = hello_at(&dir, &v(2));
        assert!(matches!(h, Hello::Handoff));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Handoff);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
