//! Remote bridges (docs/architecture/daemon/forwards.md, *Remote bridges*):
//! the hidden `devsandbox tunnel` a machine runs at the far end of a forward's
//! ssh, the one-line header it answers with, and [`remote_opener`], the
//! forward engine's opener that spawns that ssh here.
//!
//! The far side resolves the route on its own host (`port::resolver`: its
//! config, state, runtime), writes a [`Header`], then becomes the
//! in-container `devsbd bridge` (`bridge::bridge_command`, `exec`'d), so the
//! ssh's stdio carries the usual bridge handshake and mux right after the
//! header line. This side reads the header byte by byte (nothing past its
//! newline is consumed) and hands the rest of the stream to
//! `bridge::attach`.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::devsbd::bridge::{self, Bridge};
use crate::devsbd::forward::{Opened, Opener, ProcessLookup, Route};
use crate::serve::machines::ssh::Tail;
use crate::state::State;

/// What starts the header line; anything before it (a chatty login shell, a
/// motd) is skipped.
const MAGIC: &str = "devsandbox-tunnel";

/// The header's format version, after the magic. Bumped only on an
/// incompatible change; a reader refuses any other.
const VERSION: u32 = 1;

/// How much a reader takes before giving up on a header: noise and the line
/// itself together.
const HEADER_LIMIT: usize = 64 * 1024;

/// How long the remote opener waits for the header: ssh's connect and the
/// far side's route resolution (a helper install included). Matches the
/// engine's handshake wait.
const HEADER_WAIT: Duration = Duration::from_secs(10);

/// How often the remote opener checks `stop` while it waits.
const STOP_POLL: Duration = Duration::from_millis(50);

/// How long ssh's stderr is waited for after it ended without a header (or
/// its handshake died), so the reason it printed last is in.
const STDERR_WAIT: Duration = Duration::from_millis(500);

/// The one line `devsandbox tunnel` writes before the bridge's bytes:
/// `devsandbox-tunnel 1 <json>\n`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Header {
    /// The route couldn't be resolved there: why, as the far side says it.
    Error { error: String },
    /// The route, resolved there: its label, what the far helper dials, the
    /// helper's hash (informational, for the Hello) and whether the port
    /// lives in the container the bridge runs in (`helper`: its helper can
    /// say who listens; not for a via-instance route).
    Route {
        label: String,
        host: String,
        port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        #[serde(default)]
        helper: bool,
    },
}

impl Header {
    /// The header of a route resolved on this host.
    fn of_route(route: &Route) -> Header {
        Header::Route {
            label: route.label.clone(),
            host: route.host.clone(),
            port: route.port,
            hash: route.arch.and_then(crate::devsbd::hash).map(Into::into),
            helper: route.process_container == route.container,
        }
    }

    /// The header as its line, newline included.
    pub fn line(&self) -> String {
        let json = serde_json::to_string(self).expect("a header serializes");
        format!("{MAGIC} {VERSION} {json}\n")
    }
}

/// Why no header was read.
#[derive(Debug)]
pub enum HeaderError {
    /// The stream ended first: the far side never got to write one (ssh
    /// failed, an older devsandbox without `tunnel`).
    Eof,
    /// [`HEADER_LIMIT`] bytes and no header.
    TooLong,
    /// A header line that doesn't parse (another version, garbage).
    Bad(String),
    Io(io::Error),
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::Eof => write!(f, "the tunnel ended before its header"),
            HeaderError::TooLong => write!(f, "no tunnel header in the first {HEADER_LIMIT} bytes"),
            HeaderError::Bad(why) => write!(f, "{why}"),
            HeaderError::Io(e) => write!(f, "reading the tunnel header: {e}"),
        }
    }
}

/// Read the header from `r`, one byte at a time so nothing past its newline
/// is taken (the bridge's handshake follows). Lines without the magic are
/// skipped; the magic may follow noise on its line (a motd without a final
/// newline). Bounded by [`HEADER_LIMIT`].
pub fn read_header(r: &mut impl Read) -> Result<Header, HeaderError> {
    let mut line = Vec::new();
    let mut total = 0;
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) => return Err(HeaderError::Eof),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(HeaderError::Io(e)),
        }
        total += 1;
        if total > HEADER_LIMIT {
            return Err(HeaderError::TooLong);
        }
        if byte[0] != b'\n' {
            line.push(byte[0]);
            continue;
        }
        if let Some(header) = parse_line(&line)? {
            return Ok(header);
        }
        line.clear();
    }
}

/// One line: `Ok(None)` when it's noise.
fn parse_line(line: &[u8]) -> Result<Option<Header>, HeaderError> {
    let line = String::from_utf8_lossy(line);
    let Some(at) = line.find(&format!("{MAGIC} ")) else { return Ok(None) };
    let rest = line[at + MAGIC.len() + 1..].trim_end_matches('\r');
    let (version, json) = rest.split_once(' ').unwrap_or((rest, ""));
    if version != VERSION.to_string() {
        return Err(HeaderError::Bad(format!(
            "tunnel header version `{version}`, this devsandbox speaks {VERSION}: update devsandbox on the older side"
        )));
    }
    serde_json::from_str(json).map(Some).map_err(|e| HeaderError::Bad(format!("bad tunnel header: {e}")))
}

/// `devsandbox tunnel`: resolve the route to `port` of `instance` (a name, as
/// `resolve_instance` takes it) and/or `service` in the config at `dir`,
/// write its [`Header`], then become the in-container `devsbd bridge`. Its
/// stdout is the header and then the bridge's bytes, nothing else. An
/// unresolvable route is an [`Header::Error`] and a failure (stderr, exit 1).
pub fn tunnel(dir: PathBuf, instance: Option<String>, service: Option<String>, port: u16) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let route = (|| -> Result<Route, String> {
        if instance.is_none() && service.is_none() {
            return Err("name an instance or a service".into());
        }
        let key = match &instance {
            Some(name) => {
                let state = State::load().map_err(|e| format!("{e:#}"))?;
                Some(super::resolve_instance_noninteractive(&state, name).map_err(|e| format!("{e:#}"))?)
            }
            None => None,
        };
        super::port::resolver(dir, key, service, port)()
    })();
    let header = match &route {
        Ok(route) => Header::of_route(route),
        Err(error) => Header::Error { error: error.clone() },
    };
    let mut out = io::stdout().lock();
    out.write_all(header.line().as_bytes()).and_then(|()| out.flush()).context("writing the tunnel header")?;
    drop(out);
    match route {
        Err(error) => bail!("{error}"),
        // Only returns on failure; the far end then sees the handshake end
        // and this process's stderr says why.
        Ok(route) => Err(bridge::bridge_command(&route.container).exec())
            .with_context(|| format!("cannot exec a bridge into {}", route.container)),
    }
}

/// Builds a remote forward's argv (an ssh running `devsandbox tunnel`, from
/// `ssh::tunnel_argv`); called on every spawn.
pub type TunnelArgv = Box<dyn Fn() -> Result<Vec<OsString>, String> + Send + Sync>;

/// The opener of a forward into `machine`'s instance or service: spawn
/// `argv()` (rebuilt per spawn, so a changed registry entry heals), read the
/// [`Header`], then run the bridge over the rest of its stdio
/// (`bridge::attach`). The route shows as `<machine>/<label>`; the far helper
/// answers the listening-process lookup when the header says the port lives
/// in its container, else it stays unknown (the engine's probe fallback is
/// the caller's: `None` for remote).
pub fn remote_opener(machine: String, argv: TunnelArgv) -> Opener {
    Box::new(move |stop| open_remote(&machine, &argv()?, stop))
}

fn open_remote(machine: &str, argv: &[OsString], stop: &AtomicBool) -> Result<(Opened, Bridge), String> {
    let (program, args) = argv.split_first().ok_or("empty tunnel argv")?;
    let mut cmd = Command::new(program);
    // The preferred live agent, as the daemon's machine links get it (its own
    // inherited `$SSH_AUTH_SOCK` may be long stale); a CLI's falls back to
    // its own `$SSH_AUTH_SOCK`, so there it changes nothing.
    if let Some(agent) = bridge::host_agent() {
        cmd.env("SSH_AUTH_SOCK", agent);
    }
    let mut child = cmd
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", program.to_string_lossy()))?;
    let (Some(stdin), Some(mut stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        reap(&mut child);
        return Err("the tunnel's pipes are missing".into());
    };
    // stderr is read from the start (a chatty ssh can't fill its pipe and
    // stall the header), into a tail both the errors here and the bridge's
    // own handshake error read.
    let tail = match Tail::collect(stderr, "tunnel-stderr") {
        Ok(tail) => tail,
        Err(e) => {
            reap(&mut child);
            return Err(format!("cannot start the tunnel's reader: {e}"));
        }
    };
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::Builder::new().name("tunnel-header".into()).spawn(move || {
        let header = read_header(&mut stdout);
        let _ = tx.send((header, stdout));
    });
    if let Err(e) = reader {
        reap(&mut child);
        return Err(format!("cannot start the tunnel's reader: {e}"));
    }
    let deadline = Instant::now() + HEADER_WAIT;
    let (header, stdout) = loop {
        if stop.load(Ordering::Relaxed) {
            reap(&mut child);
            return Err("stopped".into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            reap(&mut child);
            return Err(no_header(machine, &tail.read(Duration::ZERO)));
        }
        match rx.recv_timeout(STOP_POLL.min(left)) {
            Ok(got) => break got,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                reap(&mut child);
                return Err("the tunnel's reader died".into());
            }
        }
    };
    match header {
        Ok(Header::Route { label, host, port, hash, helper }) => {
            let label = format!("{machine}/{label}");
            let stderr = TailReader { tail, rest: None };
            let bridge = bridge::attach(child, stdin, stdout, stderr, hash.as_deref().unwrap_or_default(), &label, Some(machine));
            let lookup = if helper { ProcessLookup::Helper { container: String::new() } } else { ProcessLookup::None };
            Ok((Opened { label, host, port, lookup }, bridge))
        }
        Ok(Header::Error { error }) => {
            reap(&mut child);
            Err(error)
        }
        Err(HeaderError::Eof) => {
            reap(&mut child);
            Err(no_header(machine, &tail.read(STDERR_WAIT)))
        }
        Err(e) => {
            reap(&mut child);
            Err(format!("{machine}: {e}"))
        }
    }
}

/// Why `machine` gave no header, from ssh's stderr `tail`: an older
/// devsandbox there (clap refusing `tunnel`), ssh's own reason
/// (`Permission denied (publickey).`), or silence.
fn no_header(machine: &str, tail: &str) -> String {
    let refused = ["unrecognized subcommand", "unknown subcommand", "wasn't expected"];
    if tail.contains("'tunnel'") && refused.iter().any(|r| tail.contains(r)) {
        return format!("devsandbox on {machine} can't forward ports yet: update it");
    }
    match tail.trim() {
        "" => format!("no answer from {machine}"),
        tail => tail.to_string(),
    }
}

fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The tunnel's stderr as `bridge::attach` reads it (on a failed handshake,
/// to say why): the [`Tail`] once the stream ended, or after
/// [`STDERR_WAIT`].
struct TailReader {
    tail: Arc<Tail>,
    rest: Option<io::Cursor<Vec<u8>>>,
}

impl Read for TailReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let tail = &self.tail;
        self.rest.get_or_insert_with(|| io::Cursor::new(tail.read(STDERR_WAIT).into_bytes())).read(buf)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, TcpStream};

    use super::*;
    use crate::devsbd::forward::{Forward, ForwardSpec, ForwardState, HostPort};

    fn route_header() -> Header {
        Header::Route { label: "web:3000".into(), host: "127.0.0.1".into(), port: 3000, hash: Some("abc".into()), helper: true }
    }

    #[test]
    fn headers_round_trip() {
        for header in [
            route_header(),
            Header::Route { label: "db:5432 (via instance web)".into(), host: "db".into(), port: 5432, hash: None, helper: false },
            Header::Error { error: "no sandbox instance `web`".into() },
        ] {
            let line = header.line();
            assert!(line.starts_with("devsandbox-tunnel 1 {") && line.ends_with("}\n"), "{line}");
            assert_eq!(read_header(&mut line.as_bytes()).unwrap(), header);
        }
        assert_eq!(
            Header::Error { error: "x".into() }.line(),
            "devsandbox-tunnel 1 {\"error\":\"x\"}\n"
        );
    }

    #[test]
    fn the_route_header_says_where_the_port_lives() {
        let route = |process_container: &str| Route {
            container: "c".into(),
            arch: None,
            host: "db".into(),
            port: 5432,
            label: "db:5432".into(),
            process_container: process_container.into(),
        };
        let header = |helper| Header::Route { label: "db:5432".into(), host: "db".into(), port: 5432, hash: None, helper };
        assert_eq!(Header::of_route(&route("c")), header(true));
        assert_eq!(Header::of_route(&route("svc")), header(false));
    }

    #[test]
    fn noise_before_the_header_is_skipped_and_nothing_after_it_is_read() {
        let stream = format!("Welcome to box!\r\nLast login: today\n\nmotd without a newline{}BRIDGE", route_header().line());
        let mut r = stream.as_bytes();
        assert_eq!(read_header(&mut r).unwrap(), route_header());
        assert_eq!(r, b"BRIDGE", "the bridge's bytes stay unread");
        // CRLF on the header line itself.
        let crlf = route_header().line().replace('\n', "\r\n");
        assert_eq!(read_header(&mut crlf.as_bytes()).unwrap(), route_header());
    }

    #[test]
    fn a_reader_is_bounded_and_refuses_what_it_cant_read() {
        let noise = "x".repeat(HEADER_LIMIT + 1);
        assert!(matches!(read_header(&mut noise.as_bytes()), Err(HeaderError::TooLong)));
        let lines = "noise\n".repeat(HEADER_LIMIT / 6 + 1) + &route_header().line();
        assert!(matches!(read_header(&mut lines.as_bytes()), Err(HeaderError::TooLong)));
        let e = read_header(&mut &b"devsandbox-tunnel 2 {}\n"[..]).unwrap_err();
        assert!(matches!(&e, HeaderError::Bad(m) if m.contains("version `2`")), "{e}");
        let e = read_header(&mut &b"devsandbox-tunnel 1 {\"label\":3}\n"[..]).unwrap_err();
        assert!(matches!(&e, HeaderError::Bad(m) if m.starts_with("bad tunnel header")), "{e}");
        assert!(matches!(read_header(&mut &b"devsandbox-tunnel 1\n"[..]), Err(HeaderError::Bad(_))));
        assert!(matches!(read_header(&mut &b"motd\nno header yet"[..]), Err(HeaderError::Eof)));
        assert!(matches!(read_header(&mut &b""[..]), Err(HeaderError::Eof)));
    }

    #[test]
    fn no_header_errors() {
        let update = "devsandbox on box can't forward ports yet: update it";
        assert_eq!(no_header("box", "error: unrecognized subcommand 'tunnel'\n\nUsage: devsandbox"), update);
        assert_eq!(no_header("box", "error: Found argument 'tunnel' which wasn't expected"), update);
        assert_eq!(no_header("box", "me@box: Permission denied (publickey)."), "me@box: Permission denied (publickey).");
        assert_eq!(no_header("box", " \n"), "no answer from box");
    }

    // ---- the remote opener against fake argvs ----

    fn sh(script: &str) -> TunnelArgv {
        let script = script.to_string();
        Box::new(move || Ok(["sh", "-c", &script].into_iter().map(OsString::from).collect()))
    }

    fn open(script: &str) -> Result<(Opened, Bridge), String> {
        remote_opener("box".into(), sh(script))(&AtomicBool::new(false))
    }

    #[test]
    fn the_far_sides_error_header_is_the_error() {
        let line = Header::Error { error: "no sandbox instance `web`".into() }.line();
        let script = format!("echo motd; echo chatty >&2; printf '%s' '{line}'; exec cat");
        assert_eq!(open(&script).err().as_deref(), Some("no sandbox instance `web`"));
    }

    #[test]
    fn a_devsandbox_without_tunnel_says_update_it() {
        let script = "echo \"error: unrecognized subcommand 'tunnel'\" >&2; exit 2";
        assert_eq!(open(script).err().as_deref(), Some("devsandbox on box can't forward ports yet: update it"));
    }

    #[test]
    fn without_a_header_ssh_says_why_or_nothing_does() {
        let script = "echo 'me@box: Permission denied (publickey).' >&2; exit 255";
        assert_eq!(open(script).err().as_deref(), Some("me@box: Permission denied (publickey)."));
        assert_eq!(open("echo motd; exit 0").err().as_deref(), Some("no answer from box"));
        let bad = remote_opener("box".into(), Box::new(|| Err("no machine `box`".into())));
        assert_eq!(bad(&AtomicBool::new(false)).err().as_deref(), Some("no machine `box`"));
        let gone = remote_opener("box".into(), Box::new(|| Ok(vec!["/nonexistent/ssh".into()])));
        assert!(gone(&AtomicBool::new(false)).err().unwrap().starts_with("cannot run /nonexistent/ssh"));
    }

    #[test]
    fn a_silent_tunnel_gives_up_when_stopped() {
        let stop = Arc::new(AtomicBool::new(false));
        let setter = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                stop.store(true, Ordering::Relaxed);
            })
        };
        let started = Instant::now();
        let got = remote_opener("box".into(), sh("exec sleep 30"))(&stop);
        assert!(got.is_err());
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
        setter.join().unwrap();
    }

    /// Docker-gated: a forward through the remote opener whose "ssh" is a
    /// shell printing noise and a header, then `exec`ing the bridge as the
    /// far `devsandbox tunnel` does. The echo round-trips and the listening
    /// process comes from the helper over that stream.
    #[test_utils::docker_test(helper)]
    fn forwards_through_a_tunnel_with_docker() -> Result<(), &'static str> {
        let ok = |cmd: &mut Command| matches!(cmd.output(), Ok(o) if o.status.success());
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("devsandbox-fwd-tunnel-{stamp}");
        let serve = "busybox nc -lk -s 127.0.0.1 -p 8080 -e cat";
        let cleanup = || {
            let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        };
        crate::test_support::with_cleanup(cleanup, || {
            assert!(ok(Command::new("docker").args(["run", "-d", "--name", &name, "alpine:3.20", "sh", "-c", serve])));
            let arch = crate::devsbd::install(&name, None).unwrap();
            let header = Header::Route {
                label: "web:8080".into(),
                host: "127.0.0.1".into(),
                port: 8080,
                hash: crate::devsbd::hash(arch).map(Into::into),
                helper: true,
            };
            let script = format!(
                "echo motd; printf '%s' '{}'; exec docker exec -i -u root {name} {} bridge",
                header.line(),
                crate::devsbd::BIN
            );
            let forward = Forward::start(ForwardSpec {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                host_port: HostPort::Prefer(0),
                open: remote_opener("box".into(), sh(&script)),
                probe: Box::new(|_, _| None),
            })
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            while forward.status().state != ForwardState::Active && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            assert_eq!(forward.status().state, ForwardState::Active);
            assert_eq!(forward.status().route_label, "box/web:8080");
            let addr = forward.status().local_addr;
            let ping = || -> io::Result<bool> {
                let mut c = TcpStream::connect(addr)?;
                c.set_read_timeout(Some(Duration::from_secs(5)))?;
                c.write_all(b"ping\n")?;
                let mut buf = [0u8; 5];
                c.read_exact(&mut buf)?;
                Ok(&buf == b"ping\n")
            };
            let mut round_tripped = false;
            for _ in 0..40 {
                if matches!(ping(), Ok(true)) {
                    round_tripped = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            assert!(round_tripped, "echo did not round-trip");
            // The first probe may beat `nc`'s bind; the next is 10 s later.
            let deadline = Instant::now() + Duration::from_secs(15);
            let found = loop {
                if let Some(p) = forward.status().process {
                    break p;
                }
                if Instant::now() >= deadline {
                    return Err("listening process never found");
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            assert!(found.contains(" (pid "), "{found}");
            drop(forward);
            Ok(())
        })
    }
}
