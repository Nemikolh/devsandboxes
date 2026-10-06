//! `devsandbox api --stdio`: a stateless relay between stdin/stdout and the
//! daemon's socket, for clients that can only spawn a process (editor
//! extensions, Electron apps, the npm package). docs/api.md, *Relay*.
//!
//! It parses nothing: bytes from stdin go to the socket, bytes from the socket
//! go to stdout (flushed per read), so the JSON-lines framing, the `hello` and
//! the protocol check all stay with the client. Only protocol bytes reach
//! stdout; diagnostics go to stderr.
//!
//! Exit: 0 once stdin hits EOF (after the daemon has answered what was in
//! flight, up to [`GRACE`]) or stdout is gone; [`EXIT_NO_HOST`] (75) when the
//! daemon closes the connection first (it exited: reconnect, which starts a
//! new one).

use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};

use super::client::{self, Conn};
use super::endpoint::{self, Stream};
use crate::devsbd::control::{EXIT_NO_HOST, EXIT_OK};

/// How long, after stdin's EOF, the relay waits for the daemon to answer the
/// requests still in flight and hang up.
pub const GRACE: Duration = Duration::from_secs(5);

/// How a relay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// stdin hit EOF: the client is done.
    Stdin,
    /// stdout can't be written: the client is gone.
    Client,
    /// The daemon closed the connection (or it broke) while the client was
    /// still there.
    Daemon,
}

impl End {
    pub fn code(self) -> i32 {
        match self {
            End::Stdin | End::Client => EXIT_OK,
            End::Daemon => EXIT_NO_HOST,
        }
    }
}

/// `devsandbox api --stdio`: lazy-start the daemon, then relay until one side
/// ends. Returns the exit code. `client` names the relay's own probe hello
/// (`serve.log`); the client's hello goes through the relay untouched.
pub fn stdio(client: &str) -> Result<i32> {
    let dir = endpoint::socket_dir()?;
    let stream = open(&dir, || client::connect(&dir, client))?;
    Ok(pump(std::io::stdin(), std::io::stdout(), stream, GRACE)?.code())
}

/// A fresh connection for the relay, after `probe` (a hello'd connection: the
/// lazy start and any version handoff). The relayed connection sends no hello
/// of its own; the client's first line is. The probe is held until the new
/// connection is up, so the daemon can't idle out in between.
fn open(dir: &Path, probe: impl FnOnce() -> Result<Conn>) -> Result<Stream> {
    let _probe = probe()?;
    endpoint::connect(dir).with_context(|| format!("cannot connect to {}", endpoint::socket_path(dir).display()))
}

enum Event {
    /// stdin is done (EOF or a read error): `Ok`; the socket refused a
    /// write: `Err`.
    Up(std::result::Result<(), ()>),
    /// The socket is done (EOF or a read error): `Ok`; stdout refused a
    /// write: `Err`.
    Down(std::result::Result<(), ()>),
}

/// Copy `input` → `daemon` and `daemon` → `output` until one side ends.
/// After `input`'s EOF the socket's write half is shut, so the daemon answers
/// what it has and hangs up; that's waited for up to `grace`. The `input`
/// thread may stay blocked in a read: the caller exits the process.
pub fn pump<R, W>(input: R, output: W, daemon: Stream, grace: Duration) -> Result<End>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let to_daemon = daemon.try_clone().context("cannot clone the daemon connection")?;
    let up = tx.clone();
    std::thread::Builder::new()
        .name("api-relay-up".into())
        .spawn(move || {
            let r = copy(input, &to_daemon);
            let eof = r.is_ok();
            // Report before half-closing: once shut, the daemon may answer and
            // hang up before this thread runs again, and a `Down` seen first
            // would read as the daemon leaving (exit 75) instead of stdin's EOF.
            let _ = up.send(Event::Up(r));
            if eof {
                let _ = to_daemon.shutdown(Shutdown::Write);
            }
        })
        .context("cannot start the relay")?;
    std::thread::Builder::new()
        .name("api-relay-down".into())
        .spawn(move || {
            let _ = tx.send(Event::Down(copy_down(&daemon, output)));
        })
        .context("cannot start the relay")?;

    Ok(match rx.recv() {
        Ok(Event::Up(Ok(()))) => match rx.recv_timeout(grace) {
            Ok(Event::Down(Err(()))) => End::Client,
            _ => End::Stdin,
        },
        // The daemon refused a write: it's gone.
        Ok(Event::Up(Err(()))) => End::Daemon,
        Ok(Event::Down(Ok(()))) => End::Daemon,
        Ok(Event::Down(Err(()))) => End::Client,
        Err(_) => End::Daemon,
    })
}

/// `input` → the socket. `Ok` at EOF (or when `input` fails: the client side
/// is gone either way); `Err` when the socket refuses a write.
fn copy(mut input: impl Read, mut to: &Stream) -> std::result::Result<(), ()> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(()),
        };
        to.write_all(&buf[..n]).map_err(|_| ())?;
    }
}

/// The socket → `output`, flushed per read so a line is never held back.
/// `Ok` at the socket's EOF (or a read error); `Err` when `output` fails.
fn copy_down(mut from: &Stream, mut output: impl Write) -> std::result::Result<(), ()> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(()),
        };
        output.write_all(&buf[..n]).map_err(|_| ())?;
        output.flush().map_err(|_| ())?;
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    use serde_json::{Value, json};

    use super::*;
    use crate::serve::daemon::Exit;
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::proto::{self, Version};

    /// A relay over socket pairs: returns (client's stdin writer, client's
    /// stdout reader, the fake daemon's end, the relay's result).
    fn relay() -> (UnixStream, BufReader<UnixStream>, UnixStream, std::thread::JoinHandle<Result<End>>) {
        let (stdin_w, stdin_r) = UnixStream::pair().unwrap();
        let (stdout_w, stdout_r) = UnixStream::pair().unwrap();
        let (relay_end, daemon_end) = UnixStream::pair().unwrap();
        stdout_r.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        daemon_end.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let handle = std::thread::spawn(move || pump(stdin_r, stdout_w, relay_end, Duration::from_secs(2)));
        (stdin_w, BufReader::new(stdout_r), daemon_end, handle)
    }

    fn line(r: &mut impl BufRead) -> String {
        let mut s = String::new();
        r.read_line(&mut s).unwrap();
        s
    }

    #[test]
    fn bytes_pass_both_ways_and_stdin_eof_waits_for_the_answer() {
        let (mut stdin, mut stdout, daemon, relay) = relay();
        let mut from_relay = BufReader::new(daemon.try_clone().unwrap());
        let mut to_relay = daemon;
        // Two lines in one write, one split across writes: all arrive as sent.
        stdin.write_all(b"{\"id\":0}\n{\"id\":1}\n{\"id\"").unwrap();
        stdin.write_all(b":2}\n").unwrap();
        assert_eq!(line(&mut from_relay), "{\"id\":0}\n");
        assert_eq!(line(&mut from_relay), "{\"id\":1}\n");
        assert_eq!(line(&mut from_relay), "{\"id\":2}\n");
        to_relay.write_all(b"{\"method\":\"inbox.changed\"}\n{\"id\":0,").unwrap();
        assert_eq!(line(&mut stdout), "{\"method\":\"inbox.changed\"}\n");
        to_relay.write_all(b"\"result\":{}}\n").unwrap();
        assert_eq!(line(&mut stdout), "{\"id\":0,\"result\":{}}\n");

        // stdin's EOF reaches the daemon as EOF; its late answer still gets out.
        stdin.shutdown(Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        from_relay.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert!(!relay.is_finished());
        to_relay.write_all(b"{\"id\":1,\"result\":{}}\n").unwrap();
        assert_eq!(line(&mut stdout), "{\"id\":1,\"result\":{}}\n");
        drop((to_relay, from_relay));
        assert_eq!(relay.join().unwrap().unwrap(), End::Stdin);
        assert_eq!(End::Stdin.code(), 0);
    }

    #[test]
    fn the_daemon_hanging_up_first_is_exit_75() {
        let (_stdin, mut stdout, daemon, relay) = relay();
        let mut to_relay = daemon;
        to_relay.write_all(b"{\"method\":\"closing\",\"params\":{\"reason\":\"idle\"}}\n").unwrap();
        drop(to_relay);
        assert_eq!(line(&mut stdout), "{\"method\":\"closing\",\"params\":{\"reason\":\"idle\"}}\n");
        assert_eq!(line(&mut stdout), "", "stdout not at EOF");
        let end = relay.join().unwrap().unwrap();
        assert_eq!((end, end.code()), (End::Daemon, EXIT_NO_HOST));
        assert_eq!(EXIT_NO_HOST, 75);
    }

    #[test]
    fn a_daemon_that_never_hangs_up_is_left_after_the_grace() {
        let (stdin, _stdout, _daemon, relay) = relay();
        let started = Instant::now();
        stdin.shutdown(Shutdown::Write).unwrap();
        assert_eq!(relay.join().unwrap().unwrap(), End::Stdin);
        assert!(started.elapsed() >= Duration::from_secs(2));
    }

    #[test]
    fn a_gone_client_ends_the_relay_quietly() {
        let (_stdin, stdout, daemon, relay) = relay();
        drop(stdout);
        let mut to_relay = daemon;
        // The first write may land in the socket buffer; keep writing.
        for _ in 0..50 {
            if relay.is_finished() || to_relay.write_all(b"{\"method\":\"inbox.changed\"}\n").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(relay.join().unwrap().unwrap(), End::Client);
    }

    #[test]
    fn relays_a_real_daemon_without_its_own_hello() {
        let dir = scratch("relay");
        // 1 s idle: 200 ms flaked under a loaded parallel test run.
        let daemon = spawn_daemon(&dir, opts(1, 1000));
        let probe = || {
            let v = Version { semver: "0.6.0".into(), build: 1 };
            client::connect_with(&dir, "api", &v, client::START_TIMEOUT, &|_, _| Ok(()))
        };
        let stream = open(&dir, probe).unwrap();
        let (mut stdin, stdin_r) = UnixStream::pair().unwrap();
        let (stdout_w, stdout_r) = UnixStream::pair().unwrap();
        stdout_r.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let relay = std::thread::spawn(move || pump(stdin_r, stdout_w, stream, GRACE));
        let mut stdout = BufReader::new(stdout_r);
        let ask = |stdin: &mut UnixStream, stdout: &mut BufReader<UnixStream>, req: Value| {
            stdin.write_all(format!("{req}\n").as_bytes()).unwrap();
            serde_json::from_str::<Value>(&line(stdout)).unwrap()
        };
        let hello = json!({"id": 0, "method": "hello", "params": {"version": "0.0.0", "client": "npm:test"}});
        let r = ask(&mut stdin, &mut stdout, hello);
        assert_eq!((r["id"].as_u64(), r["result"]["protocol"].as_u64()), (Some(0), Some(proto::PROTOCOL as u64)));
        let r = ask(&mut stdin, &mut stdout, json!({"id": 1, "method": "nope"}));
        assert_eq!(r["error"]["code"], "unknown-method");
        // A request in flight when stdin closes is still answered.
        stdin.write_all(b"{\"id\":2,\"method\":\"inbox.threads.list\"}\n").unwrap();
        stdin.shutdown(Shutdown::Write).unwrap();
        let r: Value = serde_json::from_str(&line(&mut stdout)).unwrap();
        assert_eq!(r["id"], 2);
        assert!(r["result"].is_array(), "{r}");
        assert_eq!(relay.join().unwrap().unwrap(), End::Stdin);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
