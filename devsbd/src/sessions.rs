//! `devsbd session new|attach|supervise|ls|rm|prune`: interactive sessions that
//! outlive their client (docs/architecture/helper/in-container.md), dtach's
//! shape: one supervisor process owns a pty and the command on it, and
//! listens on a unix socket; a client relays its own terminal to it, and a
//! later client can attach again after the first one is gone.
//!
//! Each session is a directory `DIR/<id>/` (id: `control::valid_run_id`, the
//! caller picks it): `argv` and `meta` in the runs format (`runs.rs`, whose
//! bookkeeping this reuses over its own root), `sock` (0600, the supervisor's
//! listener, removed when the command ends) and `client` (present while a
//! client is attached, what `ls` reports). Same liveness rule as runs: a
//! session whose supervisor is gone without an `ended` is `lost`.
//!
//! The supervisor always drains the pty (output with no client, or beyond
//! what a stalled client takes, is dropped) and never blocks on a client: it
//! is one `poll` loop over non-blocking fds. One client at a time: a new
//! attach takes over and the old one gets a `detached` record. There is no
//! output log (a session may live forever; so would its log): a reattach
//! sees what comes next, plus the redraw the program does on the `SIGWINCH`
//! an attach causes.
//!
//! Socket wire (helper-only, both ends are this binary): records
//! `u8 tag | u32 len (BE) | payload`, see [`Record`]. Versioned
//! ([`WIRE_VERSION`]) twice: in `meta`, checked before a client dials, and
//! in the hello each end sends first; a new connection displaces the
//! attached client only once its hello matches.

use std::collections::VecDeque;
use std::ffi::{CStr, OsStr, c_int};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::control::{self, valid_run_id};
use crate::outbox;
use crate::runs::{self, State};

/// Next to the runs dir, same reasons (`runs::DIR`): in the writable layer,
/// world-writable + sticky (`INSTALL_SCRIPT`), sessions start as the sandbox
/// user. Separate from runs so `run ls` (dispatchers) never lists sessions.
pub const DIR: &str = "/var/lib/devsandbox/sessions";

/// A client's exit code when there is no session to attach to: unknown id,
/// `lost`, or the connection to the supervisor went away without an end.
/// Above every code a shell reports for a program (0–125, 126/127, 128+n for
/// signals up to 64 → 192), below ssh's 255, and apart from the helper's own
/// (`control::EXIT_*`, `proto::MISMATCH_EXIT`), so a caller can tell it from
/// the program's own status (only a program choosing this exact code can't be).
pub const NO_SESSION_EXIT: i32 = 250;

/// A client's exit code when another client took the session over: the
/// program runs on, attached elsewhere. Same family as [`NO_SESSION_EXIT`].
pub const DETACHED_EXIT: i32 = 251;

/// A client's exit code when the session was started by a devsbd speaking
/// another [`WIRE_VERSION`] (a supervisor keeps its binary while a newer
/// helper is installed): nothing was attached, the program runs on. Same
/// family as [`NO_SESSION_EXIT`], and not it: the host reports 250 as the
/// session being gone, and this one is still there.
pub const VERSION_EXIT: i32 = 252;

/// The session wire's version: in `meta` (`proto <N>`, written by
/// [`create_with`]) and in each end's [`Record::Hello`]. Bump it on any
/// change to the records; keep `Hello`'s own encoding as is, so any two
/// versions can still tell they differ.
const WIRE_VERSION: u8 = 1;

/// Largest record payload either end accepts; a bigger length is a protocol
/// error (the stream is dropped), so a bad peer can't make a reader buffer
/// without bound.
const MAX_PAYLOAD: usize = 64 * 1024;

/// Most output bytes queued for a client; past it new output is dropped, so a
/// client that stops reading never holds up the program.
const OUT_CAP: usize = 256 * 1024;

/// Most client input bytes queued for the pty; past it the supervisor stops
/// reading the client until the program takes some (backpressure on the
/// client, never a block in the supervisor).
const INPUT_CAP: usize = 64 * 1024;

/// The supervisor's `poll` timeout: how often it checks whether the command
/// exited when nothing else happens.
const TICK: c_int = 100;

/// How often a client checks its terminal's size (see [`relay`]).
const SIZE_POLL: Duration = Duration::from_millis(200);

/// How long the supervisor tries to hand a client its final records.
const FINAL_WRITE: Duration = Duration::from_secs(2);

/// How long a new connection has to send its [`Record::Hello`] before the
/// supervisor drops it. A client sends it first thing, so this only reaps
/// connections that never will; the attached client stays attached
/// meanwhile.
const HELLO_WAIT: Duration = Duration::from_secs(2);

/// Output records a client queues for its terminal before dropping more:
/// the supervisor's [`OUT_CAP`] in its 16 KiB reads. A terminal nobody
/// reads (ssh gone, the exec's pty kept) then costs output, never the
/// socket reader, so `detached` and `exit` always arrive.
const OUT_QUEUE: usize = OUT_CAP / (16 * 1024);

/// How long a finished client waits for its queued output to reach the
/// terminal, and then for its note to reach stderr (the same possibly
/// unread pty).
const TERMINAL_WAIT: Duration = Duration::from_secs(1);

/// How long a supervisor whose command couldn't be spawned waits for the
/// client to collect the error.
const FAILED_WAIT: Duration = Duration::from_secs(2);

/// `rm`: after SIGHUP, how long the command gets before SIGKILL.
const HUP_WAIT: Duration = Duration::from_secs(2);

/// How long an ended session's dir outlives its end before `new`'s automatic
/// prune deletes it. An attach to an ended session exits with its recorded
/// code, and the ssh reconnect loop (`serve::machines::cli` `Reconnect`,
/// backoff up to 10 s) relies on that when the program exits during an
/// outage: its re-dial must still find the end. Minutes, well past any
/// backoff, so a slow re-dial still gets the code.
const ENDED_GRACE: u64 = 5 * 60;

const USAGE: &str = "usage: devsbd session new --id <id> [--cwd DIR] -- <cmd>...\n\
       devsbd session attach <id>\n\
       devsbd session ls\n\
       devsbd session rm <id>\n\
       devsbd session prune [--keep N]";

/// `devsbd session …`; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let root = Path::new(DIR);
    let cmd = match parse(args) {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("devsbd session: {e}\n{USAGE}");
            return control::EXIT_USAGE;
        }
    };
    let result = match cmd {
        Cmd::New { id, cwd, argv } => {
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(e) => return failed(e),
            };
            let started = create_with(root, &id, &argv, cwd.as_deref(), |id| {
                let mut cmd = Command::new(&exe);
                cmd.args(["session", "supervise", id]);
                cmd
            });
            return match started {
                Ok(child) => terminal(|input, out, size| join(root, &id, child, input, out, size)),
                Err(e) => failed(e),
            };
        }
        Cmd::Attach { id } => return terminal(|input, out, size| attach(root, &id, input, out, size)),
        Cmd::Supervise { id } => supervise(root, &id),
        Cmd::Ls => ls(root).and_then(|text| io::stdout().write_all(text.as_bytes())),
        Cmd::Rm { id } => rm(root, &id),
        Cmd::Prune { keep } => runs::prune(root, keep).map(|n| println!("removed {n}")),
    };
    match result {
        Ok(()) => control::EXIT_OK,
        Err(e) => failed(e),
    }
}

fn failed(e: io::Error) -> i32 {
    eprintln!("devsbd session: {e}");
    control::EXIT_FAILED
}

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    New { id: String, cwd: Option<String>, argv: Vec<String> },
    Attach { id: String },
    Supervise { id: String },
    Ls,
    Rm { id: String },
    Prune { keep: usize },
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    let (sub, rest) = args.split_first().ok_or("missing subcommand")?;
    let (mut id, mut cwd, mut argv, mut keep) = (None, None, None, None);
    let mut words: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") && f.len() > 2 => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = || match inline.clone().or_else(|| it.next().cloned()) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(format!("{flag} needs a value")),
        };
        match (sub.as_str(), flag) {
            ("new", "--") => {
                argv = Some(it.by_ref().cloned().collect::<Vec<_>>());
                break;
            }
            ("new", "--id") => once(&mut id, value()?, flag)?,
            ("new", "--cwd") => once(&mut cwd, value()?, flag)?,
            ("prune", "--keep") => {
                let v = value()?;
                once(&mut keep, v.parse::<u64>().map_err(|_| format!("{flag}: bad number `{v}`"))?, flag)?
            }
            (_, f) if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option `{f}`")),
            _ => words.push(arg.clone()),
        }
    }
    let checked = |id: String| if valid_run_id(&id) { Ok(id) } else { Err(format!("bad session id `{id}`")) };
    let one_id = |words: &mut Vec<String>| match words.as_slice() {
        [_] => checked(words.remove(0)),
        _ => Err("takes exactly one <id>".to_string()),
    };
    match sub.as_str() {
        "new" => {
            if !words.is_empty() {
                return Err("the command goes after `--`".into());
            }
            let id = checked(id.ok_or("missing --id <id>")?)?;
            match argv {
                Some(argv) if !argv.is_empty() => Ok(Cmd::New { id, cwd, argv }),
                _ => Err("missing `-- <cmd>...`".into()),
            }
        }
        "attach" => Ok(Cmd::Attach { id: one_id(&mut words)? }),
        "supervise" => Ok(Cmd::Supervise { id: one_id(&mut words)? }),
        "ls" if words.is_empty() => Ok(Cmd::Ls),
        "ls" => Err("takes no arguments".into()),
        "rm" => Ok(Cmd::Rm { id: one_id(&mut words)? }),
        "prune" if words.is_empty() => {
            Ok(Cmd::Prune { keep: usize::try_from(keep.unwrap_or(0)).unwrap_or(usize::MAX) })
        }
        "prune" => Err("takes no arguments".into()),
        other => Err(format!("unknown subcommand `{other}`")),
    }
}

fn once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
    match slot.replace(value) {
        Some(_) => Err(format!("{flag} given twice")),
        None => Ok(()),
    }
}

fn session_dir(root: &Path, id: &str) -> io::Result<PathBuf> {
    let dir = root.join(id);
    if !valid_run_id(id) || !runs::is_real_dir(&dir) {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no session `{id}`")));
    }
    Ok(dir)
}

// ---- wire ----------------------------------------------------------------

const TAG_SIZE: u8 = 1;
const TAG_DATA: u8 = 2;
const TAG_EXIT: u8 = 3;
const TAG_DETACHED: u8 = 4;
const TAG_HELLO: u8 = 5;

/// One socket record. `Hello` (both ways, each end's first: its
/// [`WIRE_VERSION`], `u8`), `Size` (client → supervisor: `u16 cols, u16
/// rows`, BE), `Data` (both ways: terminal bytes), `Exit` (supervisor →
/// client: the code to exit with, `i32` BE), `Detached` (supervisor →
/// client: another client took over; empty).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    Hello(u8),
    Size { cols: u16, rows: u16 },
    Data(Vec<u8>),
    Exit(i32),
    Detached,
}

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn encode(record: &Record) -> Vec<u8> {
    match record {
        Record::Hello(version) => frame(TAG_HELLO, &[*version]),
        Record::Size { cols, rows } => {
            let mut p = cols.to_be_bytes().to_vec();
            p.extend_from_slice(&rows.to_be_bytes());
            frame(TAG_SIZE, &p)
        }
        Record::Data(bytes) => frame(TAG_DATA, bytes),
        Record::Exit(code) => frame(TAG_EXIT, &code.to_be_bytes()),
        Record::Detached => frame(TAG_DETACHED, &[]),
    }
}

/// Take one complete record off the front of `buf`: `Ok(None)` while it's
/// incomplete; an unknown tag, a payload of the wrong size or over
/// [`MAX_PAYLOAD`] is an error (the peer is dropped).
fn decode(buf: &mut Vec<u8>) -> Result<Option<Record>, String> {
    if buf.len() < 5 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(format!("record of {len} bytes"));
    }
    if buf.len() < 5 + len {
        return Ok(None);
    }
    let p = &buf[5..5 + len];
    let record = match (buf[0], len) {
        (TAG_HELLO, 1) => Record::Hello(p[0]),
        (TAG_SIZE, 4) => Record::Size { cols: u16::from_be_bytes([p[0], p[1]]), rows: u16::from_be_bytes([p[2], p[3]]) },
        (TAG_DATA, _) => Record::Data(p.to_vec()),
        (TAG_EXIT, 4) => Record::Exit(i32::from_be_bytes([p[0], p[1], p[2], p[3]])),
        (TAG_DETACHED, 0) => Record::Detached,
        (tag, len) => return Err(format!("bad record: tag {tag}, {len} bytes")),
    };
    buf.drain(..5 + len);
    Ok(Some(record))
}

// ---- libc ----------------------------------------------------------------

/// The few libc calls a pty needs, declared by hand: std links libc anyway
/// (musl, static) and the crate takes no dependencies. Constants and struct
/// layouts that differ are per `target_os`; `termios` is treated as opaque
/// (only `tcgetattr` / `cfmakeraw` / `tcsetattr` touch it) because its layout
/// differs between musl/glibc and macOS.
mod sys {
    use std::ffi::{c_char, c_int, c_ulong};

    #[cfg(target_os = "linux")]
    mod os {
        use std::ffi::{c_int, c_ulong};
        pub const TIOCSCTTY: c_ulong = 0x540E;
        pub const TIOCGWINSZ: c_ulong = 0x5413;
        pub const TIOCSWINSZ: c_ulong = 0x5414;
        pub const O_NOCTTY: c_int = 0o400;
        pub const O_NONBLOCK: c_int = 0o4000;
        pub type NfdsT = c_ulong;
        pub type ModeT = u32;
    }
    #[cfg(target_os = "macos")]
    mod os {
        use std::ffi::{c_int, c_uint, c_ulong};
        pub const TIOCSCTTY: c_ulong = 0x2000_7461;
        pub const TIOCGWINSZ: c_ulong = 0x4008_7468;
        pub const TIOCSWINSZ: c_ulong = 0x8008_7467;
        pub const O_NOCTTY: c_int = 0x2_0000;
        pub const O_NONBLOCK: c_int = 0x4;
        pub type NfdsT = c_uint;
        pub type ModeT = u16;
    }
    pub use os::*;

    pub const O_RDWR: c_int = 2;
    pub const F_SETFD: c_int = 2;
    pub const F_GETFL: c_int = 3;
    pub const F_SETFL: c_int = 4;
    pub const FD_CLOEXEC: c_int = 1;
    pub const TCSANOW: c_int = 0;
    pub const POLLIN: i16 = 0x1;
    pub const POLLOUT: i16 = 0x4;
    pub const POLLERR: i16 = 0x8;
    pub const POLLHUP: i16 = 0x10;
    pub const POLLNVAL: i16 = 0x20;

    #[repr(C)]
    pub struct PollFd {
        pub fd: c_int,
        pub events: i16,
        pub revents: i16,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct Winsize {
        pub ws_row: u16,
        pub ws_col: u16,
        pub ws_xpixel: u16,
        pub ws_ypixel: u16,
    }

    /// Opaque, larger than any platform's `struct termios` (60 bytes on
    /// Linux, 72 on macOS).
    #[repr(C, align(8))]
    #[derive(Clone, Copy)]
    pub struct Termios(pub [u8; 256]);

    unsafe extern "C" {
        pub fn posix_openpt(flags: c_int) -> c_int;
        pub fn grantpt(fd: c_int) -> c_int;
        pub fn unlockpt(fd: c_int) -> c_int;
        pub fn ptsname(fd: c_int) -> *mut c_char;
        pub fn setsid() -> c_int;
        pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        pub fn poll(fds: *mut PollFd, nfds: NfdsT, timeout: c_int) -> c_int;
        pub fn isatty(fd: c_int) -> c_int;
        pub fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int;
        pub fn tcsetattr(fd: c_int, action: c_int, t: *const Termios) -> c_int;
        pub fn cfmakeraw(t: *mut Termios);
        pub fn umask(mask: ModeT) -> ModeT;
    }
}

fn check(ret: c_int) -> io::Result<c_int> {
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on an fd we own; no memory is shared.
    unsafe {
        let flags = check(sys::fcntl(fd, sys::F_GETFL))?;
        check(sys::fcntl(fd, sys::F_SETFL, flags | sys::O_NONBLOCK))?;
    }
    Ok(())
}

fn set_size(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = sys::Winsize { ws_row: rows, ws_col: cols, ..Default::default() };
    // SAFETY: TIOCSWINSZ reads one `struct winsize`, which `ws` is.
    check(unsafe { sys::ioctl(fd, sys::TIOCSWINSZ, &ws as *const sys::Winsize) }).map(drop)
}

/// `fd`'s terminal size as `(cols, rows)`; `None` off a terminal.
fn get_size(fd: RawFd) -> Option<(u16, u16)> {
    let mut ws = sys::Winsize::default();
    // SAFETY: TIOCGWINSZ writes one `struct winsize` into `ws`.
    let ok = unsafe { sys::ioctl(fd, sys::TIOCGWINSZ, &mut ws as *mut sys::Winsize) } == 0;
    (ok && (ws.ws_col, ws.ws_row) != (0, 0)).then_some((ws.ws_col, ws.ws_row))
}

/// A new pty: `(master, slave)`, both close-on-exec (the command gets the
/// slave as its stdio, never the master).
fn open_pty() -> io::Result<(File, File)> {
    // SAFETY: plain calls; the fd is owned by `master` right away, and the
    // `ptsname` string is copied before anything else can reuse its buffer
    // (the supervisor opens one pty, on one thread).
    unsafe {
        let fd = check(sys::posix_openpt(sys::O_RDWR | sys::O_NOCTTY))?;
        let master = File::from_raw_fd(fd);
        check(sys::fcntl(fd, sys::F_SETFD, sys::FD_CLOEXEC))?;
        check(sys::grantpt(fd))?;
        check(sys::unlockpt(fd))?;
        let name = sys::ptsname(fd);
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        let path = PathBuf::from(OsStr::from_bytes(CStr::from_ptr(name).to_bytes()));
        let slave = OpenOptions::new().read(true).write(true).custom_flags(sys::O_NOCTTY).open(path)?;
        Ok((master, slave))
    }
}

/// Bind `path` with the socket file 0600 from the start (umask around the
/// bind: the supervisor is single-threaded there, so nothing else sees it).
fn bind_private(path: &Path) -> io::Result<UnixListener> {
    // SAFETY: umask only swaps the process mask.
    let old = unsafe { sys::umask(0o177) };
    let bound = UnixListener::bind(path);
    unsafe { sys::umask(old) };
    bound
}

// ---- supervisor ----------------------------------------------------------

/// Frames queued for one client. Kept whole, so dropping queued output (a
/// takeover) never cuts a frame the client has half-read.
#[derive(Default)]
struct Out {
    frames: VecDeque<Vec<u8>>,
    /// Bytes of the front frame already written.
    head: usize,
    bytes: usize,
}

impl Out {
    /// Queue `frame` unless that would pass [`OUT_CAP`] (dropped: the client
    /// isn't keeping up). Returns whether it was queued.
    fn push(&mut self, frame: Vec<u8>) -> bool {
        if self.bytes + frame.len() > OUT_CAP {
            return false;
        }
        self.push_always(frame);
        true
    }

    fn push_always(&mut self, frame: Vec<u8>) {
        self.bytes += frame.len();
        self.frames.push_back(frame);
    }

    /// Drop everything but a partly written front frame.
    fn clear_unstarted(&mut self) {
        let keep = if self.head > 0 { self.frames.pop_front() } else { None };
        self.frames.clear();
        self.bytes = 0;
        if let Some(f) = keep {
            self.bytes = f.len() - self.head;
            self.frames.push_back(f);
        }
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Write what `w` takes now (non-blocking: stops at `WouldBlock`; a
    /// blocking `w` with a timeout gets everything or an error).
    fn flush(&mut self, w: &mut impl Write) -> io::Result<()> {
        while let Some(f) = self.frames.front() {
            match w.write(&f[self.head..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.head += n;
                    self.bytes -= n;
                    if self.head == f.len() {
                        self.frames.pop_front();
                        self.head = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

struct Client {
    sock: UnixStream,
    inbuf: Vec<u8>,
    out: Out,
    /// Got its first size (the attach redraw is done).
    sized: bool,
}

/// A new connection that hasn't said hello yet: not a client, so it
/// displaces no one until [`hello_verdict`] says it speaks this wire.
struct Pending {
    sock: UnixStream,
    inbuf: Vec<u8>,
    /// Dropped past this ([`HELLO_WAIT`]).
    deadline: Instant,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// No complete first record yet.
    Waiting,
    /// Hello with this [`WIRE_VERSION`]: it takes over.
    Matched,
    /// Hello with another version: told ours, closed.
    Mismatched,
    /// Anything else first: closed.
    Bad,
}

/// What a new connection's first record (off the front of `inbuf`) makes it.
fn hello_verdict(inbuf: &mut Vec<u8>) -> Verdict {
    match decode(inbuf) {
        Ok(None) => Verdict::Waiting,
        Ok(Some(Record::Hello(WIRE_VERSION))) => Verdict::Matched,
        Ok(Some(Record::Hello(_))) => Verdict::Mismatched,
        Ok(Some(_)) | Err(_) => Verdict::Bad,
    }
}

struct Supervisor {
    dir: PathBuf,
    listener: UnixListener,
    master: File,
    /// False once the pty reads EOF/EIO (every slave fd closed).
    master_open: bool,
    /// Client input not yet written to the pty.
    input: Vec<u8>,
    client: Option<Client>,
    /// Connections waiting for their hello, oldest first.
    pending: Vec<Pending>,
}

/// `session supervise`: bind `sock`, open a pty, run the command on it (cwd
/// and env inherited from `new`, its own session with the pty as controlling
/// terminal, so its pid is its pgid, what `rm` signals), relay until it
/// exits, then record the end and tell the client. A command that can't be
/// spawned ends at once with exit 127, its error shown to the client.
fn supervise(root: &Path, id: &str) -> io::Result<()> {
    let dir = session_dir(root, id)?;
    let argv = runs::read_argv(&dir)?;
    let Some((program, rest)) = argv.split_first() else {
        return Err(io::Error::other("empty argv"));
    };
    let sock = dir.join("sock");
    let _ = fs::remove_file(&sock);
    let listener = bind_private(&sock)?;
    listener.set_nonblocking(true)?;
    let (master, slave) = open_pty()?;
    set_nonblocking(master.as_raw_fd())?;
    let spawned = spawn_on(&slave, program, rest);
    drop(slave);
    let mut sup = Supervisor { dir: dir.clone(), listener, master, master_open: true, input: Vec::new(), client: None, pending: Vec::new() };
    let end = match spawned {
        Ok(mut child) => {
            let pid = child.id();
            runs::append(&dir, &format!("pid {pid}\npgid {pid}\n"))?;
            runs::exit_line(sup.relay(&mut child)?)
        }
        Err(e) => {
            sup.show_failure(format!("devsbd: cannot run `{program}`: {e}\r\n").into_bytes());
            "exit 127".to_string()
        }
    };
    let state = runs::parse_meta(&format!("ended 0\n{end}\n")).exit.unwrap_or(State::Exited(1));
    // Socket gone first (no new attach), then `ended`, and only then the
    // client's `exit` and the listener's close: a client that loses the race
    // reads `ended` from `meta`.
    let _ = fs::remove_file(&sock);
    let _ = fs::remove_file(dir.join("client"));
    runs::append(&dir, &format!("ended {}\n{end}\n", runs::now()))?;
    if let Some(mut c) = sup.client.take() {
        c.out.push_always(encode(&Record::Exit(exit_code(state))));
        let _ = c.sock.set_nonblocking(false);
        let _ = c.sock.set_write_timeout(Some(FINAL_WRITE));
        let _ = c.out.flush(&mut c.sock);
    }
    Ok(())
}

/// Spawn `program` on the pty `slave` as a session leader with the slave as
/// its controlling terminal.
fn spawn_on(slave: &File, program: &str, rest: &[String]) -> io::Result<Child> {
    let mut cmd = Command::new(program);
    cmd.args(rest).stdin(slave.try_clone()?).stdout(slave.try_clone()?).stderr(slave.try_clone()?);
    // SAFETY: only async-signal-safe syscalls between fork and exec; fd 0 is
    // the slave by then.
    unsafe {
        cmd.pre_exec(|| {
            check(sys::setsid())?;
            check(sys::ioctl(0, sys::TIOCSCTTY, 0 as c_int))?;
            Ok(())
        });
    }
    cmd.spawn()
}

fn poll(fds: &mut [sys::PollFd], timeout: c_int) -> io::Result<()> {
    // SAFETY: `fds` is a valid array of `fds.len()` pollfds.
    match check(unsafe { sys::poll(fds.as_mut_ptr(), fds.len() as sys::NfdsT, timeout) }) {
        Err(e) if e.kind() != io::ErrorKind::Interrupted => Err(e),
        _ => Ok(()),
    }
}

fn pollfd(fd: RawFd, events: i16) -> sys::PollFd {
    sys::PollFd { fd, events, revents: 0 }
}

impl Supervisor {
    /// The relay loop, until the command exits; returns how it ended.
    fn relay(&mut self, child: &mut Child) -> io::Result<ExitStatus> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            if let Some(status) = child.try_wait()? {
                self.drain_master(&mut buf);
                return Ok(status);
            }
            let mut fds = vec![pollfd(self.listener.as_raw_fd(), sys::POLLIN)];
            let master_at = self.master_open.then(|| {
                let out = if self.input.is_empty() { 0 } else { sys::POLLOUT };
                fds.push(pollfd(self.master.as_raw_fd(), sys::POLLIN | out));
                fds.len() - 1
            });
            let client_at = self.client.as_ref().map(|c| {
                let read = if self.input.len() < INPUT_CAP { sys::POLLIN } else { 0 };
                let write = if c.out.is_empty() { 0 } else { sys::POLLOUT };
                fds.push(pollfd(c.sock.as_raw_fd(), read | write));
                fds.len() - 1
            });
            let pending_at = self.push_pending(&mut fds);
            poll(&mut fds, TICK)?;
            if fds[0].revents & sys::POLLIN != 0 {
                self.accept();
            }
            if let Some(i) = master_at {
                let ev = fds[i].revents;
                if ev & (sys::POLLIN | sys::POLLHUP | sys::POLLERR | sys::POLLNVAL) != 0 {
                    self.read_master(&mut buf);
                }
                if ev & sys::POLLOUT != 0 {
                    self.write_master();
                }
            }
            // Its pty read may have dropped it (a failed flush).
            if let Some(i) = client_at.filter(|_| self.client.is_some()) {
                let ev = fds[i].revents;
                if ev & (sys::POLLIN | sys::POLLHUP | sys::POLLERR | sys::POLLNVAL) != 0 {
                    self.read_client(&mut buf);
                }
                if ev & sys::POLLOUT != 0 {
                    self.flush_client();
                }
            }
            // After the client's turn: one promoted now isn't in `fds` either.
            self.serve_pending(&fds[pending_at..], &mut buf);
        }
    }

    /// Add the pending connections to `fds` (in [`Supervisor::pending`]'s
    /// order); returns where they start.
    fn push_pending(&self, fds: &mut Vec<sys::PollFd>) -> usize {
        let at = fds.len();
        fds.extend(self.pending.iter().map(|p| pollfd(p.sock.as_raw_fd(), sys::POLLIN)));
        at
    }

    /// Accept every waiting connection as [`Pending`]: none takes over before
    /// its hello ([`Supervisor::serve_pending`]).
    fn accept(&mut self) {
        while let Ok((sock, _)) = self.listener.accept() {
            if sock.set_nonblocking(true).is_err() {
                continue;
            }
            self.pending.push(Pending { sock, inbuf: Vec::new(), deadline: Instant::now() + HELLO_WAIT });
        }
    }

    /// One read from each pending connection `polled` reports ready (the
    /// first `polled.len()` of them; later ones were accepted since), then
    /// its verdict: a match takes over, a mismatch gets our hello and is
    /// closed, anything bad or past its deadline is closed. Never blocks.
    fn serve_pending(&mut self, polled: &[sys::PollFd], buf: &mut [u8]) {
        let now = Instant::now();
        for (i, mut p) in std::mem::take(&mut self.pending).into_iter().enumerate() {
            let ready = polled.get(i).is_some_and(|f| f.revents != 0);
            let verdict = if !ready {
                Verdict::Waiting
            } else {
                match p.sock.read(buf) {
                    Ok(0) => Verdict::Bad,
                    Ok(n) => {
                        p.inbuf.extend_from_slice(&buf[..n]);
                        hello_verdict(&mut p.inbuf)
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                        Verdict::Waiting
                    }
                    Err(_) => Verdict::Bad,
                }
            };
            match verdict {
                Verdict::Waiting if now < p.deadline => self.pending.push(p),
                Verdict::Waiting | Verdict::Bad => {}
                Verdict::Matched => self.take_over(p),
                Verdict::Mismatched => {
                    // A fresh socket's buffer takes 6 bytes without blocking.
                    let _ = p.sock.write(&encode(&Record::Hello(WIRE_VERSION)));
                    let _ = p.sock.shutdown(Shutdown::Write);
                }
            }
        }
    }

    /// A connection that said hello becomes the client: the previous one gets
    /// `detached`, the new one our hello, then whatever it sent after its own.
    fn take_over(&mut self, p: Pending) {
        if let Some(mut old) = self.client.take() {
            old.out.clear_unstarted();
            old.out.push_always(encode(&Record::Detached));
            // Blocking, bounded, off this thread: the supervisor never
            // waits on a client, but a non-blocking write into a full
            // socket buffer would lose `detached`.
            std::thread::spawn(move || {
                let _ = old.sock.set_nonblocking(false);
                let _ = old.sock.set_write_timeout(Some(FINAL_WRITE));
                let _ = old.out.flush(&mut old.sock);
            });
        }
        let _ = File::create(self.dir.join("client"));
        let mut out = Out::default();
        out.push_always(encode(&Record::Hello(WIRE_VERSION)));
        self.client = Some(Client { sock: p.sock, inbuf: p.inbuf, out, sized: false });
        self.client_records();
        self.flush_client();
    }

    fn drop_client(&mut self) {
        self.client = None;
        let _ = fs::remove_file(self.dir.join("client"));
    }

    /// One read of pty output, queued for the client or dropped.
    fn read_master(&mut self, buf: &mut [u8]) -> bool {
        match (&self.master).read(buf) {
            Ok(0) => self.master_open = false,
            Ok(n) => {
                if let Some(c) = &mut self.client {
                    c.out.push(frame(TAG_DATA, &buf[..n]));
                    self.flush_client();
                }
                return true;
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
            // EIO: every slave fd is closed (Linux); anything else is as final.
            Err(_) => self.master_open = false,
        }
        false
    }

    /// The command exited: forward what it left in the pty (bounded: a
    /// background process still writing there can't keep us here).
    fn drain_master(&mut self, buf: &mut [u8]) {
        for _ in 0..64 {
            if !self.master_open || !self.read_master(buf) {
                break;
            }
        }
    }

    fn write_master(&mut self) {
        match (&self.master).write(&self.input) {
            Ok(n) => {
                self.input.drain(..n);
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
            Err(_) => {
                self.master_open = false;
                self.input.clear();
            }
        }
    }

    fn flush_client(&mut self) {
        let Some(c) = &mut self.client else { return };
        if c.out.flush(&mut c.sock).is_err() {
            self.drop_client();
        }
    }

    /// One read of client records: sizes go to the pty (the first one with
    /// the redraw nudge), data is queued for it. EOF or a bad record drops
    /// the client; the command runs on.
    fn read_client(&mut self, buf: &mut [u8]) {
        let Some(c) = &mut self.client else { return };
        match c.sock.read(buf) {
            Ok(0) => return self.drop_client(),
            Ok(n) => c.inbuf.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => return,
            Err(_) => return self.drop_client(),
        }
        self.client_records();
    }

    /// Handle the client's complete records in its `inbuf`.
    fn client_records(&mut self) {
        let Some(c) = &mut self.client else { return };
        loop {
            match decode(&mut c.inbuf) {
                Ok(None) => break,
                Ok(Some(Record::Size { cols, rows })) => {
                    let fd = self.master.as_raw_fd();
                    if !c.sized {
                        // dtach's redraw: a size one row off, then the real
                        // one, so the program gets SIGWINCH even when the
                        // size didn't change.
                        c.sized = true;
                        let _ = set_size(fd, cols, if rows > 1 { rows - 1 } else { rows + 1 });
                    }
                    let _ = set_size(fd, cols, rows);
                }
                Ok(Some(Record::Data(bytes))) => self.input.extend_from_slice(&bytes),
                Ok(Some(_)) | Err(_) => return self.drop_client(),
            }
        }
    }

    /// The command couldn't be spawned: give a client that attaches soon
    /// the error as output (the caller then sends its `exit`).
    fn show_failure(&mut self, message: Vec<u8>) {
        let deadline = Instant::now() + FAILED_WAIT;
        let mut buf = vec![0u8; 16 * 1024];
        while self.client.is_none() && Instant::now() < deadline {
            let mut fds = vec![pollfd(self.listener.as_raw_fd(), sys::POLLIN)];
            let pending_at = self.push_pending(&mut fds);
            if poll(&mut fds, TICK).is_err() {
                break;
            }
            self.accept();
            self.serve_pending(&fds[pending_at..], &mut buf);
        }
        if let Some(c) = &mut self.client {
            c.out.push_always(frame(TAG_DATA, &message));
        }
    }
}

/// The code a client exits with for a session that ended as `state`, as a
/// shell reports it (`128 + n` for signal `n`).
fn exit_code(state: State) -> i32 {
    match state {
        State::Exited(code) => code,
        State::Killed(sig) => 128 + sig,
        State::Running | State::Lost => NO_SESSION_EXIT,
    }
}

// ---- client --------------------------------------------------------------

/// What a client ends with: its exit code and a note for stderr (printed
/// after the terminal is restored).
#[derive(Debug, PartialEq, Eq)]
struct Done {
    code: i32,
    note: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Exit(i32),
    Detached,
    /// The supervisor's hello names another wire version.
    Version(u8),
    /// The connection ended without an `exit` or `detached`.
    Lost,
}

type SizeFn = Box<dyn FnMut() -> Option<(u16, u16)> + Send>;

/// Saved terminal state for the panic hook (the helper's profile aborts on
/// panic, so `Drop` wouldn't run).
static SAVED: OnceLock<(RawFd, sys::Termios)> = OnceLock::new();

/// Raw mode on a terminal, restored on drop (normal exits) and by a panic
/// hook. A client killed by a signal leaves it raw; that only happens when
/// its terminal is going away anyway (SIGHUP, the exec's pty closing).
struct RawMode {
    fd: RawFd,
    saved: sys::Termios,
}

impl RawMode {
    fn enter(fd: RawFd) -> io::Result<RawMode> {
        let mut t = sys::Termios([0; 256]);
        // SAFETY: `t` is larger than any `struct termios` and aligned for it.
        unsafe { check(sys::tcgetattr(fd, &mut t))? };
        let saved = t;
        unsafe {
            sys::cfmakeraw(&mut t);
            check(sys::tcsetattr(fd, sys::TCSANOW, &t))?;
        }
        if SAVED.set((fd, saved)).is_ok() {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if let Some((fd, t)) = SAVED.get() {
                    unsafe { sys::tcsetattr(*fd, sys::TCSANOW, t) };
                }
                prev(info)
            }));
        }
        Ok(RawMode { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the attributes `enter` read.
        unsafe { sys::tcsetattr(self.fd, sys::TCSANOW, &self.saved) };
    }
}

/// Run a client on this process's stdio: raw mode and size tracking when
/// stdin is a terminal (else plain bytes, no size), the terminal restored
/// before the note is printed, for at most [`TERMINAL_WAIT`]. Returns the
/// exit code, which `main` passes straight to `std::process::exit`: that
/// also ends a thread still stuck writing to a terminal nobody reads.
fn terminal(client: impl FnOnce(io::Stdin, Box<dyn Write + Send>, Option<SizeFn>) -> Done) -> i32 {
    // SAFETY: isatty only inspects the fd.
    let tty = unsafe { sys::isatty(0) } == 1;
    let raw = if tty { RawMode::enter(0).ok() } else { None };
    let size: Option<SizeFn> = if tty { Some(Box::new(|| get_size(0))) } else { None };
    let done = client(io::stdin(), Box::new(io::stdout()), size);
    drop(raw);
    if let Some(note) = done.note {
        let (wrote, waited) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = writeln!(io::stderr(), "devsbd session: {note}");
            drop(wrote);
        });
        let _ = waited.recv_timeout(TERMINAL_WAIT);
    }
    done.code
}

/// Relay a connected client: `input` to the session as `data`, the terminal
/// size (from `size`, when given) at once and on every change, the session's
/// `data` to `out`, until `exit`, `detached` or the connection ends.
///
/// `out` is written on a thread of its own, fed through an [`OUT_QUEUE`]
/// bound that drops output when full, so a stalled terminal never keeps the
/// socket from being read. Owned, not borrowed: that thread may never
/// return. At the end, queued output gets [`TERMINAL_WAIT`] to drain.
///
/// Size changes are polled every [`SIZE_POLL`] on a thread rather than taken
/// from `SIGWINCH`: a handler needs `sigaction` (whose struct layout differs
/// per libc) plus a self-pipe to wake anything, while polling is one ioctl and
/// costs at most [`SIZE_POLL`] of latency on a resize.
fn relay(
    sock: UnixStream,
    input: impl Read + Send + 'static,
    out: Box<dyn Write + Send>,
    size: Option<SizeFn>,
) -> Outcome {
    let Ok(writer) = sock.try_clone() else { return Outcome::Lost };
    let writer = Arc::new(Mutex::new(writer));
    let done = Arc::new(AtomicBool::new(false));
    let send = |w: &Mutex<UnixStream>, record: &[u8]| w.lock().is_ok_and(|mut s| s.write_all(record).is_ok());
    // First, before anything the supervisor would act on: it takes nothing
    // from a connection, nor displaces anyone for it, before this.
    send(&writer, &encode(&Record::Hello(WIRE_VERSION)));
    if let Some(mut size) = size {
        let mut last = size();
        if let Some((cols, rows)) = last {
            send(&writer, &encode(&Record::Size { cols, rows }));
        }
        let (w, done) = (writer.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(SIZE_POLL);
                let now = size();
                if now != last {
                    last = now;
                    if let Some((cols, rows)) = now
                        && !send(&w, &encode(&Record::Size { cols, rows }))
                    {
                        break;
                    }
                }
            }
        });
    }
    let w = writer.clone();
    std::thread::spawn(move || {
        let (mut input, mut buf) = (input, vec![0u8; 4096]);
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) if send(&w, &frame(TAG_DATA, &buf[..n])) => {}
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    let (queue, queued) = mpsc::sync_channel::<Vec<u8>>(OUT_QUEUE);
    let (drained, drain) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let (mut out, _drained) = (out, drained);
        for bytes in queued {
            // The terminal going away isn't a reason to stop: the
            // session's `exit` still decides the code.
            let _ = out.write_all(&bytes).and_then(|()| out.flush());
        }
    });
    let outcome = read_records(&sock, &queue);
    done.store(true, Ordering::Relaxed);
    let _ = sock.shutdown(Shutdown::Both);
    drop(queue);
    let _ = drain.recv_timeout(TERMINAL_WAIT);
    outcome
}

/// Read records until the outcome, `data` handed to `out` (dropped when its
/// queue is full).
fn read_records(mut sock: &UnixStream, out: &SyncSender<Vec<u8>>) -> Outcome {
    let (mut inbuf, mut buf) = (Vec::new(), vec![0u8; 16 * 1024]);
    loop {
        match sock.read(&mut buf) {
            Ok(0) => return Outcome::Lost,
            Ok(n) => inbuf.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Outcome::Lost,
        }
        loop {
            match decode(&mut inbuf) {
                Ok(None) => break,
                Ok(Some(Record::Data(bytes))) => {
                    let _ = out.try_send(bytes);
                }
                Ok(Some(Record::Exit(code))) => return Outcome::Exit(code),
                Ok(Some(Record::Detached)) => return Outcome::Detached,
                Ok(Some(Record::Hello(WIRE_VERSION))) => {}
                Ok(Some(Record::Hello(v))) => return Outcome::Version(v),
                Ok(Some(Record::Size { .. })) | Err(_) => return Outcome::Lost,
            }
        }
    }
}

/// `meta`'s recorded end, if any.
fn ended(dir: &Path) -> Option<State> {
    let meta = runs::parse_meta(&runs::read_capped(&dir.join("meta"), 4 * 1024).ok()?);
    meta.ended.map(|_| meta.exit.unwrap_or(State::Lost))
}

fn no_session(id: &str, why: &str) -> Done {
    Done { code: NO_SESSION_EXIT, note: Some(format!("{why} `{id}`")) }
}

/// Session `id` speaks wire version `theirs` (`None`: from before versions
/// were recorded), not ours.
fn other_version(id: &str, theirs: Option<u8>) -> Done {
    let theirs = theirs.map_or("none".to_string(), |v| v.to_string());
    Done {
        code: VERSION_EXIT,
        note: Some(format!(
            "session `{id}` was started by another devsbd (proto {theirs}, this is {WIRE_VERSION}); end it with session rm"
        )),
    }
}

/// A finished relay (or a connect that failed) as the client's result: the
/// recorded end wins over a lost connection.
fn conclude(dir: &Path, id: &str, outcome: Outcome) -> Done {
    match outcome {
        Outcome::Exit(code) => Done { code, note: None },
        Outcome::Detached => Done { code: DETACHED_EXIT, note: Some(format!("session `{id}` was attached elsewhere")) },
        Outcome::Version(v) => other_version(id, Some(v)),
        Outcome::Lost => match ended(dir) {
            Some(state) => Done { code: exit_code(state), note: None },
            None => no_session(id, "lost the connection to session"),
        },
    }
}

/// Connect to the session's socket, waiting while it's still starting:
/// while `child` (the supervisor `new` just spawned) lives, else while the
/// session is young and its command not recorded yet. `None`: nothing to
/// attach to.
fn connect(dir: &Path, mut child: Option<Child>) -> Option<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(runs::STARTING_GRACE);
    loop {
        if let Ok(sock) = UnixStream::connect(dir.join("sock")) {
            return Some(sock);
        }
        if ended(dir).is_some() || Instant::now() >= deadline {
            return None;
        }
        let starting = match &mut child {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => {
                let meta = runs::read_capped(&dir.join("meta"), 4 * 1024).map(|t| runs::parse_meta(&t));
                meta.is_ok_and(|m| {
                    m.pid.is_none() && runs::now().saturating_sub(m.started.unwrap_or(0)) < runs::STARTING_GRACE
                })
            }
        };
        if !starting {
            // One more look: it may have bound or ended in between.
            return UnixStream::connect(dir.join("sock")).ok();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `session attach`: attach only. An ended session's recorded code at once;
/// unknown or `lost`: [`NO_SESSION_EXIT`].
fn attach(
    root: &Path,
    id: &str,
    input: impl Read + Send + 'static,
    out: Box<dyn Write + Send>,
    size: Option<SizeFn>,
) -> Done {
    join(root, id, None, input, out, size)
}

/// Attach to session `id`, `child` its supervisor when this process just
/// started it (`new`). Before dialing, `meta`: an ended session's recorded
/// code at once (whoever ran it); one whose `proto` isn't [`WIRE_VERSION`]
/// (or has none) is refused with [`VERSION_EXIT`], never connected to, so
/// its attached client stays attached.
fn join(
    root: &Path,
    id: &str,
    child: Option<Child>,
    input: impl Read + Send + 'static,
    out: Box<dyn Write + Send>,
    size: Option<SizeFn>,
) -> Done {
    let Ok(dir) = session_dir(root, id) else { return no_session(id, "no session") };
    // No `meta` yet: another `new` is mid-create; `connect` waits for it as
    // for any starting session.
    if let Ok(meta) = runs::read_capped(&dir.join("meta"), 4 * 1024).map(|t| runs::parse_meta(&t)) {
        if meta.ended.is_some() {
            return Done { code: exit_code(meta.exit.unwrap_or(State::Lost)), note: None };
        }
        if meta.proto != Some(WIRE_VERSION) {
            return other_version(id, meta.proto);
        }
    }
    match connect(&dir, child) {
        Some(sock) => conclude(&dir, id, relay(sock, input, out, size)),
        None => match ended(&dir) {
            Some(state) => Done { code: exit_code(state), note: None },
            None => no_session(id, "no live session"),
        },
    }
}

/// `session new`'s first half: create session `id` and spawn its supervisor
/// (from `supervisor`; tests run it from the test binary) detached in `cwd`:
/// its own session, null stdio. `Ok(None)`: the id is taken, attach to it
/// instead. A bad `cwd` fails here, before anything runs. `meta` records the
/// [`WIRE_VERSION`] (`proto`) that `join` checks. First, quietly,
/// [`prune_settled`].
fn create_with(
    root: &Path,
    id: &str,
    argv: &[String],
    cwd: Option<&str>,
    supervisor: impl FnOnce(&str) -> Command,
) -> io::Result<Option<Child>> {
    outbox::ensure_dir(root)?;
    prune_settled(root, runs::now());
    let dir = root.join(id);
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e),
    }
    // `proto` before anything else, so no attach ever reads a `meta` without it.
    let spawned = runs::append(&dir, &format!("proto {WIRE_VERSION}\n")).and_then(|()| runs::init(&dir, argv, runs::now()));
    let spawned = spawned.and_then(|()| {
        let mut cmd = supervisor(id);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: only setsid between fork and exec. Its own session: no
        // terminal's hangup or job control reaches the supervisor.
        unsafe {
            cmd.pre_exec(|| check(sys::setsid()).map(drop));
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.spawn()
    });
    match spawned {
        Ok(child) => {
            runs::append(&dir, &format!("supervisor {}\n", child.id()))?;
            Ok(Some(child))
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            Err(io::Error::new(e.kind(), format!("cannot start the session: {e}")))
        }
    }
}

// ---- ls / rm / prune -----------------------------------------------------

/// `new`'s automatic prune: every `lost` session and every one ended more
/// than [`ENDED_GRACE`] before `now`, so finished dirs don't pile up and push
/// live sessions out of `ls`'s newest `runs::MAX_LS_RUNS`. As the caller's
/// user, so other users' dirs (sticky dir) just stay; errors are ignored, a
/// failed prune never stops a new session.
fn prune_settled(root: &Path, now: u64) {
    let _ = runs::prune_ended_before(root, now.saturating_sub(ENDED_GRACE));
}

/// `session ls`: one line per session, oldest first, only the newest
/// `runs::MAX_LS_RUNS`: `<id> <state> <attached|detached|-> <started, UTC>
/// <argv…>` (state as in `run ls`; the attachment is `-` unless running),
/// argv rendered and cut as `run ls` does. Unreadable sessions are skipped.
fn ls(root: &Path) -> io::Result<String> {
    let ids = runs::run_ids(root)?;
    let mut out = String::new();
    for id in &ids[ids.len().saturating_sub(runs::MAX_LS_RUNS)..] {
        let dir = root.join(id);
        let Ok((meta, state)) = runs::load_state(&dir, id) else { continue };
        let argv = match runs::read_argv(&dir) {
            Ok(argv) => argv,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(_) => continue,
        };
        let attached = match state {
            State::Running if dir.join("client").exists() => "attached",
            State::Running => "detached",
            _ => "-",
        };
        let started = meta.started.or_else(|| id[..10].parse().ok()).unwrap_or(0);
        out.push_str(&format!("{id} {} {attached} {}", state.label(), crate::boot::format_utc(started)));
        out.push_str(&runs::render_argv(&argv));
        out.push('\n');
    }
    Ok(out)
}

/// Wait up to `timeout` for session `id` to stop running.
fn wait_stopped(dir: &Path, id: &str, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let (_, state) = runs::load_state(dir, id)?;
        if state != State::Running {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(runs::POLL);
    }
}

/// `session rm`: end a running session (SIGHUP to the command's group, as a
/// closing terminal would; SIGKILL if it's still there after [`HUP_WAIT`]),
/// wait for its supervisor to record the end, delete the dir. The group is
/// verified against the process table (`runs::kill_target`), as `run rm
/// --force` does.
fn rm(root: &Path, id: &str) -> io::Result<()> {
    let dir = session_dir(root, id)?;
    if let Ok((meta, State::Running)) = runs::load_state(&dir, id)
        && let Some(pgid) = runs::kill_target(&meta, runs::proc_stat)
    {
        runs::signal_group(pgid, runs::SIGHUP)?;
        if !wait_stopped(&dir, id, HUP_WAIT)? {
            runs::signal_group(pgid, runs::SIGKILL)?;
            if !wait_stopped(&dir, id, runs::KILL_WAIT)? {
                return Err(io::Error::other(format!("session `{id}` is still running after the kill")));
            }
        }
    }
    fs::remove_dir_all(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Under `/tmp`, not `temp_dir()`: macOS's is long enough to push
    /// `<root>/<id>/sock` past `sun_path` (104 bytes).
    fn temp_root(tag: &str) -> PathBuf {
        let root = PathBuf::from(format!("/tmp/devsbd-ses-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn parses_forms() {
        let p = |a: &[&str]| parse(&strings(a));
        let id = "1790000000-a1b2";
        assert_eq!(
            p(&["new", "--id", id, "--cwd=/w", "--", "sh", "-c", "echo --x"]),
            Ok(Cmd::New { id: id.into(), cwd: Some("/w".into()), argv: strings(&["sh", "-c", "echo --x"]) })
        );
        assert_eq!(p(&["attach", id]), Ok(Cmd::Attach { id: id.into() }));
        assert_eq!(p(&["supervise", id]), Ok(Cmd::Supervise { id: id.into() }));
        assert_eq!(p(&["ls"]), Ok(Cmd::Ls));
        assert_eq!(p(&["rm", id]), Ok(Cmd::Rm { id: id.into() }));
        assert_eq!(p(&["prune"]), Ok(Cmd::Prune { keep: 0 }));
        assert_eq!(p(&["prune", "--keep", "5"]), Ok(Cmd::Prune { keep: 5 }));
        assert_eq!(p(&["prune", "--keep=0"]), Ok(Cmd::Prune { keep: 0 }));
        assert!(p(&["prune", "x"]).unwrap_err().contains("no arguments"));
        assert!(p(&["prune", "--keep", "-1"]).unwrap_err().contains("bad number"));
        assert!(p(&["prune", "--keep", "1", "--keep", "2"]).unwrap_err().contains("twice"));
        assert!(p(&["ls", "--keep", "1"]).unwrap_err().contains("unknown option"));
        assert!(p(&["new", "--", "sh"]).unwrap_err().contains("missing --id"));
        assert!(p(&["new", "--id", "x", "--", "sh"]).unwrap_err().contains("bad session id"));
        assert!(p(&["new", "--id", id]).unwrap_err().contains("missing `--"));
        assert!(p(&["new", "--id", id, "sh"]).unwrap_err().contains("after `--`"));
        assert!(p(&["new", "--id", id, "--id", id, "--", "sh"]).unwrap_err().contains("twice"));
        assert!(p(&["attach"]).unwrap_err().contains("exactly one"));
        assert!(p(&["attach", "../x"]).unwrap_err().contains("bad session id"));
        assert!(p(&["rm", id, "--force"]).unwrap_err().contains("unknown option"));
        assert!(p(&["ls", "x"]).unwrap_err().contains("no arguments"));
        assert!(p(&["nope"]).unwrap_err().contains("unknown subcommand"));
    }

    #[test]
    fn codec_round_trips_and_rejects() {
        let records = [
            Record::Hello(WIRE_VERSION),
            Record::Hello(255),
            Record::Size { cols: 300, rows: 2 },
            Record::Data(b"hi\x00\xff".to_vec()),
            Record::Data(Vec::new()),
            Record::Exit(-1),
            Record::Exit(137),
            Record::Detached,
        ];
        let mut stream: Vec<u8> = records.iter().flat_map(encode).collect();
        // Byte by byte: incomplete until the last byte of each.
        let mut buf = Vec::new();
        let mut got = Vec::new();
        for b in stream.drain(..) {
            buf.push(b);
            while let Some(r) = decode(&mut buf).unwrap() {
                got.push(r);
            }
        }
        assert_eq!(got, records);
        assert!(buf.is_empty());
        assert_eq!(encode(&Record::Size { cols: 0x0102, rows: 0x0304 }), [1, 0, 0, 0, 4, 1, 2, 3, 4]);
        // Fixed for good: every version must read another's hello.
        assert_eq!(encode(&Record::Hello(7)), [5, 0, 0, 0, 1, 7]);

        let mut big = vec![TAG_DATA];
        big.extend_from_slice(&(MAX_PAYLOAD as u32 + 1).to_be_bytes());
        assert!(decode(&mut big).is_err());
        assert!(decode(&mut frame(TAG_SIZE, &[1, 2, 3])).is_err());
        assert!(decode(&mut frame(TAG_DETACHED, &[1])).is_err());
        assert!(decode(&mut frame(TAG_HELLO, &[])).is_err());
        assert!(decode(&mut frame(TAG_HELLO, &[1, 2])).is_err());
        assert!(decode(&mut frame(9, &[])).is_err());
    }

    #[test]
    fn hello_verdicts() {
        let v = |bytes: Vec<u8>| {
            let mut buf = bytes;
            hello_verdict(&mut buf)
        };
        let hello = encode(&Record::Hello(WIRE_VERSION));
        assert_eq!(v(hello[..3].to_vec()), Verdict::Waiting);
        assert_eq!(v(Vec::new()), Verdict::Waiting);
        assert_eq!(v(hello.clone()), Verdict::Matched);
        assert_eq!(v(encode(&Record::Hello(WIRE_VERSION + 1))), Verdict::Mismatched);
        assert_eq!(v(encode(&Record::Size { cols: 80, rows: 24 })), Verdict::Bad);
        assert_eq!(v(frame(9, &[])), Verdict::Bad);
        // What follows the hello stays for the client.
        let mut buf = [hello, encode(&Record::Data(b"x".to_vec()))].concat();
        assert_eq!(hello_verdict(&mut buf), Verdict::Matched);
        assert_eq!(decode(&mut buf), Ok(Some(Record::Data(b"x".to_vec()))));
    }

    #[test]
    fn out_queue_caps_and_keeps_frames_whole() {
        let mut out = Out::default();
        assert!(out.push(vec![0; OUT_CAP - 10]));
        assert!(!out.push(vec![0; 11]), "over the cap: dropped");
        assert!(out.push(vec![1; 10]));
        // A writer that takes 3 bytes, then would block.
        struct Takes(usize, Vec<u8>);
        impl Write for Takes {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let n = b.len().min(self.0);
                self.0 -= n;
                self.1.extend_from_slice(&b[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut w = Takes(3, Vec::new());
        out.flush(&mut w).unwrap();
        out.clear_unstarted();
        out.push_always(encode(&Record::Detached));
        let mut w2 = Takes(usize::MAX, Vec::new());
        out.flush(&mut w2).unwrap();
        assert_eq!(w2.1.len(), OUT_CAP - 10 - 3 + 5, "the started frame's rest, then detached");
        assert!(out.is_empty() && out.bytes == 0);
    }

    /// Not a test by itself: the supervisor process the tests below spawn
    /// (from this test binary, which isn't devsbd), over the root and id in
    /// its env.
    #[test]
    #[ignore]
    fn supervisor_process() {
        let (Ok(root), Ok(id)) =
            (std::env::var("DEVSBD_TEST_SESSIONS_ROOT"), std::env::var("DEVSBD_TEST_SESSION_ID"))
        else {
            return;
        };
        supervise(Path::new(&root), &id).unwrap();
    }

    /// `create_with` with this test binary as the supervisor.
    fn create_test(root: &Path, id: &str, argv: &[String], cwd: Option<&str>) -> io::Result<Option<Child>> {
        let exe = std::env::current_exe().unwrap();
        create_with(root, id, argv, cwd, |id| {
            let mut cmd = Command::new(&exe);
            // The id as a second filter: the cmdline names it (`is_supervisor`).
            cmd.args(["--exact", "--ignored", "sessions::tests::supervisor_process", id])
                .env("DEVSBD_TEST_SESSIONS_ROOT", root)
                .env("DEVSBD_TEST_SESSION_ID", id);
            cmd
        })
    }

    fn start(root: &Path, id: &str, script: &str) -> Child {
        create_test(root, id, &strings(&["sh", "-c", script]), None).unwrap().expect("a new session")
    }

    /// Output collected by a client running on a thread.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Sink {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    struct TestClient {
        input: UnixStream,
        output: Sink,
        done: mpsc::Receiver<Done>,
    }

    impl TestClient {
        fn send(&mut self, bytes: &str) {
            self.input.write_all(bytes.as_bytes()).unwrap();
        }
        fn wait(&self) -> Done {
            self.done.recv_timeout(Duration::from_secs(20)).expect("client finished")
        }
        fn wait_output(&self, needle: &str) {
            let t = Instant::now();
            while !self.output.text().contains(needle) {
                assert!(t.elapsed() < Duration::from_secs(20), "no `{needle}` in {:?}", self.output.text());
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// A client on a thread: `child` as `new` passes it (`None`: `attach`).
    fn client(root: &Path, id: &str, child: Option<Child>, size: Option<SizeFn>) -> TestClient {
        let output = Sink::default();
        client_to(root, id, child, size, output.clone(), Box::new(output))
    }

    /// [`client`] writing to `out` (`output` then sees whatever `out` shares).
    fn client_to(
        root: &Path,
        id: &str,
        child: Option<Child>,
        size: Option<SizeFn>,
        output: Sink,
        out: Box<dyn Write + Send>,
    ) -> TestClient {
        let (input, theirs) = UnixStream::pair().unwrap();
        let (tx, done) = mpsc::channel();
        let (root, id) = (root.to_path_buf(), id.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(join(&root, &id, child, theirs, out, size));
        });
        TestClient { input, output, done }
    }

    /// A terminal nobody reads: the first write blocks forever (its thread
    /// is leaked, never joined).
    struct Stalled(Arc<AtomicBool>);
    impl Write for Stalled {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.0.store(true, Ordering::Relaxed);
            loop {
                std::thread::park();
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let t = Instant::now();
        while !cond() {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn meta(root: &Path, id: &str) -> runs::Meta {
        runs::parse_meta(&fs::read_to_string(root.join(id).join("meta")).unwrap())
    }

    #[test]
    fn echo_round_trip_and_exit_codes() {
        let root = temp_root("echo");
        let id = "1790000000-0001";
        let mut c = client(&root, id, Some(start(&root, id, "read l; echo got:$l; exit 3")), None);
        c.send("hi\n");
        let done = c.wait();
        assert_eq!(done, Done { code: 3, note: None }, "{:?}", c.output.text());
        assert!(c.output.text().contains("got:hi"), "{:?}", c.output.text());
        assert_eq!(ended(&root.join(id)), Some(State::Exited(3)));
        assert!(!root.join(id).join("sock").exists(), "socket removed");
        // Attaching to an ended session: its code at once.
        assert_eq!(client(&root, id, None, None).wait(), Done { code: 3, note: None });
        // `new` on a taken id attaches instead.
        assert!(create_with(&root, id, &strings(&["true"]), None, |_| unreachable!()).unwrap().is_none());

        // Killed by a signal: 128 + n; a quick exit may beat the attach.
        let id = "1790000000-0002";
        assert_eq!(client(&root, id, Some(start(&root, id, "kill -9 $$")), None).wait().code, 137);
        let id = "1790000000-0003";
        assert_eq!(client(&root, id, Some(start(&root, id, "exit 7")), None).wait().code, 7);
        // A command that can't run: 127, the error shown.
        let id = "1790000000-0004";
        let child = create_test(&root, id, &strings(&["/nonexistent/cmd"]), None).unwrap();
        let c = client(&root, id, child, None);
        assert_eq!(c.wait().code, 127);
        assert!(c.output.text().contains("cannot run `/nonexistent/cmd`"), "{:?}", c.output.text());
        // A bad cwd fails the start and leaves nothing.
        let id = "1790000000-0005";
        assert!(create_test(&root, id, &strings(&["true"]), Some("/nonexistent/dir")).is_err());
        assert!(!root.join(id).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn attach_to_unknown_is_no_session() {
        let root = temp_root("unknown");
        let done = client(&root, "1790000000-ffff", None, None).wait();
        assert_eq!(done.code, NO_SESSION_EXIT);
        assert!(done.note.unwrap().contains("no session"));
        assert_eq!(client(&root, "../etc", None, None).wait().code, NO_SESSION_EXIT);
    }

    #[test]
    fn resize_reaches_the_program() {
        let root = temp_root("size");
        let id = "1790000000-0001";
        let size = Arc::new(Mutex::new((100u16, 30u16)));
        let s = size.clone();
        let size_fn: SizeFn = Box::new(move || Some(*s.lock().unwrap()));
        let mut c = client(&root, id, Some(start(&root, id, "read a; stty size; read b; stty size")), Some(size_fn));
        c.send("\n");
        c.wait_output("30 100");
        *size.lock().unwrap() = (120, 40);
        std::thread::sleep(SIZE_POLL * 3);
        c.send("\n");
        c.wait_output("40 120");
        assert_eq!(c.wait().code, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_attach_takes_over() {
        let root = temp_root("takeover");
        let id = "1790000000-0001";
        let a = client(&root, id, Some(start(&root, id, "read l; echo got:$l; exit 5")), None);
        let dir = root.join(id);
        wait_for("the first attach", || dir.join("client").exists());
        let mut b = client(&root, id, None, None);
        let done = a.wait();
        assert_eq!(done.code, DETACHED_EXIT);
        assert!(done.note.unwrap().contains("attached elsewhere"));
        b.send("x\n");
        assert_eq!(b.wait().code, 5);
        assert!(b.output.text().contains("got:x"), "{:?}", b.output.text());
        assert!(!a.output.text().contains("got:x"));

        // A first client stuck writing to its terminal still reads its
        // `detached` and finishes, past a full queue and socket buffer.
        let id = "1790000000-0002";
        let stuck = Arc::new(AtomicBool::new(false));
        let script = "read l; head -c 2000000 /dev/zero | tr '\\0' x; read l; exit 6";
        let mut a = client_to(
            &root,
            id,
            Some(start(&root, id, script)),
            None,
            Sink::default(),
            Box::new(Stalled(stuck.clone())),
        );
        wait_for("the first attach", || root.join(id).join("client").exists());
        a.send("\n");
        wait_for("the stalled write", || stuck.load(Ordering::Relaxed));
        let mut b = client(&root, id, None, None);
        let t = Instant::now();
        let done = a.wait();
        assert_eq!(done.code, DETACHED_EXIT, "{done:?}");
        assert!(t.elapsed() < Duration::from_secs(10), "took {:?}", t.elapsed());
        b.send("\n");
        assert_eq!(b.wait().code, 6);
        let _ = fs::remove_dir_all(&root);
    }

    /// The supervisor killed: its client loses the connection, later attaches
    /// find nothing (the socket file left behind refuses), `ls` says `lost`.
    #[test]
    fn lost_after_the_supervisor_dies() {
        let root = temp_root("lost");
        let id = "1790000000-0001";
        let a = client(&root, id, Some(start(&root, id, "sleep 300")), None);
        let dir = root.join(id);
        wait_for("the attach", || dir.join("client").exists());
        wait_for("the command", || meta(&root, id).pgid.is_some());
        let m = meta(&root, id);
        // The supervisor leads its own group (setsid), the command another.
        runs::signal_group(m.supervisor.unwrap(), runs::SIGKILL).unwrap();
        let done = a.wait();
        assert_eq!(done.code, NO_SESSION_EXIT);
        assert!(done.note.unwrap().contains("lost the connection"));
        assert!(dir.join("sock").exists());
        assert_eq!(client(&root, id, None, None).wait().code, NO_SESSION_EXIT);
        // `is_supervisor` reads /proc: off Linux every session reads as lost.
        #[cfg(target_os = "linux")]
        wait_for("lost in ls", || ls(&root).unwrap().starts_with(&format!("{id} lost - ")));
        let _ = runs::signal_group(m.pgid.unwrap(), runs::SIGKILL);
        let _ = fs::remove_dir_all(&root);
    }

    /// A session dir made by hand: `argv` and `meta` as given.
    fn fake_session(root: &Path, id: &str, meta: &str) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("argv"), "arg sh\n").unwrap();
        fs::write(dir.join("meta"), meta).unwrap();
    }

    /// `new`'s prune: `lost` and long-ended sessions go; recently ended ones
    /// (an attach still returns their code), running and unreadable ones
    /// stay. `prune` without `--keep` then takes the recent ones too.
    #[test]
    fn new_prunes_lost_and_long_ended_sessions() {
        let root = temp_root("prune");
        prune_settled(&root, runs::now()); // no root yet: nothing, no panic
        let now = runs::now();
        let ended = |ago: u64| format!("started 1790000000\nended {}\nexit 3\n", now - ago);
        fake_session(&root, "1790000000-0000", &ended(ENDED_GRACE + 1));
        fake_session(&root, "1790000001-0000", &ended(ENDED_GRACE - 60));
        fake_session(&root, "1790000002-0000", "started 1790000002\nsupervisor 4294967\n");
        fake_session(&root, "1790000003-0000", "started 1790000003\n");
        let mut sup = Command::new("sh").args(["-c", "sleep 30; :", "1790000003-0000"]).spawn().unwrap();
        runs::append(&root.join("1790000003-0000"), &format!("supervisor {}\n", sup.id())).unwrap();
        #[cfg(target_os = "linux")]
        wait_for("the fake supervisor", || runs::is_supervisor(sup.id(), "1790000003-0000"));
        fs::create_dir_all(root.join("1790000004-0000/meta")).unwrap();
        let left = || runs::run_ids(&root).unwrap();

        // Through `new`: the prune runs before the new dir exists.
        let id = "1790000005-0000";
        assert_eq!(client(&root, id, Some(start(&root, id, "exit 0")), None).wait().code, 0);
        // `is_supervisor` reads /proc: off Linux the running one reads as lost.
        #[cfg(target_os = "linux")]
        assert_eq!(left(), ["1790000001-0000", "1790000003-0000", "1790000004-0000", id]);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(left(), ["1790000001-0000", "1790000004-0000", id]);
        // Still within the grace: its code.
        assert_eq!(client(&root, "1790000001-0000", None, None).wait().code, 3);
        prune_settled(&root, runs::now());
        assert_eq!(left().len(), if cfg!(target_os = "linux") { 4 } else { 3 }, "nothing more to prune");
        // Past the grace, the recently ended ones go too.
        prune_settled(&root, runs::now() + ENDED_GRACE + 1);
        #[cfg(target_os = "linux")]
        assert_eq!(left(), ["1790000003-0000", "1790000004-0000"]);

        // `session prune`: every finished one but the newest `keep`.
        fake_session(&root, "1790000006-0000", &ended(0));
        fake_session(&root, "1790000007-0000", &ended(0));
        assert_eq!(runs::prune(&root, 1).unwrap(), 1);
        assert!(left().contains(&"1790000007-0000".to_string()));
        assert_eq!(runs::prune(&root, 0).unwrap(), 1);
        #[cfg(target_os = "linux")]
        assert_eq!(left(), ["1790000003-0000", "1790000004-0000"]);
        let _ = sup.kill();
        let _ = sup.wait();
        let _ = fs::remove_dir_all(&root);
    }

    /// Ended sessions piling up no longer hide an older live one from `ls`:
    /// `new` prunes the long-ended ones. Linux only (`is_supervisor`).
    #[cfg(target_os = "linux")]
    #[test]
    fn ls_keeps_an_old_live_session_past_many_ended_ones() {
        let root = temp_root("ls-live");
        let live = "1790000000-0001";
        let _child = start(&root, live, "sleep 300");
        wait_for("the command", || meta(&root, live).pgid.is_some());
        let done = format!("started 1790000001\nended {}\nexit 0\n", runs::now() - ENDED_GRACE - 1);
        for n in 0..runs::MAX_LS_RUNS + 10 {
            fake_session(&root, &format!("{:010}-0000", 1790000001 + n), &done);
        }
        assert!(!ls(&root).unwrap().contains(live), "hidden before the prune");
        let id = "1800000000-0001";
        assert_eq!(client(&root, id, Some(start(&root, id, "exit 0")), None).wait().code, 0);
        assert_eq!(runs::run_ids(&root).unwrap(), [live, id]);
        assert!(ls(&root).unwrap().starts_with(&format!("{live} running detached ")));
        rm(&root, live).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// Output with no client, then with one that never reads, doesn't block
    /// the program; a later attach still gets the session.
    #[test]
    fn output_while_detached_never_blocks() {
        let root = temp_root("drain");
        let id = "1790000000-0001";
        let lines = "i=0; while [ $i -lt 3000 ]; do echo line-$i-0123456789012345678901234567890123456789; i=$((i+1)); done";
        let script = format!("{lines}; echo > m1; read a; {lines}; echo > m2; read b; echo got:$b; exit 4");
        let dir = root.join(id);
        let work = temp_root("drain-w");
        fs::create_dir_all(&work).unwrap();
        let child = create_test(&root, id, &strings(&["sh", "-c", &script]), Some(work.to_str().unwrap()));
        let child = child.unwrap().unwrap();
        wait_for("output with no client", || work.join("m1").exists());
        // A client that sends but never reads.
        let mut stalled = UnixStream::connect(dir.join("sock")).unwrap();
        stalled.write_all(&encode(&Record::Hello(WIRE_VERSION))).unwrap();
        stalled.write_all(&encode(&Record::Size { cols: 80, rows: 24 })).unwrap();
        stalled.write_all(&encode(&Record::Data(b"\n".to_vec()))).unwrap();
        wait_for("output to a stalled client", || work.join("m2").exists());
        let mut b = client(&root, id, None, None);
        b.send("x\n");
        assert_eq!(b.wait().code, 4);
        assert!(b.output.text().contains("got:x"), "{:?}", b.output.text());
        drop((stalled, child));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&work);
    }

    /// A session of another wire version: refused from `meta` before any
    /// dial, and a hand-made connection with another hello (or none, or a
    /// bad first record) is answered or dropped, the attached client never
    /// displaced. An ended session's code doesn't depend on its version.
    #[test]
    fn another_wire_version_never_displaces_the_attached_client() {
        let root = temp_root("version");
        let id = "1790000000-0001";
        let script = "read l; echo got:$l; read l; echo got:$l; exit 8";
        let mut a = client(&root, id, Some(start(&root, id, script)), None);
        let dir = root.join(id);
        wait_for("the attach", || dir.join("client").exists());
        wait_for("the command", || meta(&root, id).pgid.is_some());
        assert_eq!(meta(&root, id).proto, Some(WIRE_VERSION));

        let real = fs::read_to_string(dir.join("meta")).unwrap();
        let ours = format!("proto {WIRE_VERSION}\n");
        for (text, theirs) in [(real.replace(&ours, "proto 200\n"), "proto 200"), (real.replace(&ours, ""), "proto none")] {
            fs::write(dir.join("meta"), text).unwrap();
            let done = client(&root, id, None, None).wait();
            assert_eq!(done.code, VERSION_EXIT);
            let note = done.note.unwrap();
            assert!(note.contains(&format!("({theirs}, this is {WIRE_VERSION}); end it with session rm")), "{note}");
        }
        fs::write(dir.join("meta"), &real).unwrap();
        a.send("x\n");
        a.wait_output("got:x");

        let dial = || {
            let s = UnixStream::connect(dir.join("sock")).unwrap();
            s.set_read_timeout(Some(HELLO_WAIT + Duration::from_secs(5))).unwrap();
            s
        };
        let answer = |mut s: UnixStream| {
            let mut got = Vec::new();
            s.read_to_end(&mut got).expect("closed by the supervisor");
            got
        };
        // Another hello: ours back, then closed.
        let mut s = dial();
        // Only the hello: anything still unread when the supervisor closes
        // makes Linux reset our read (ECONNRESET), or fails the write.
        s.write_all(&encode(&Record::Hello(WIRE_VERSION + 1))).unwrap();
        assert_eq!(answer(s), encode(&Record::Hello(WIRE_VERSION)));
        // A bad first record, or nothing until the deadline: closed.
        let mut s = dial();
        s.write_all(&encode(&Record::Data(b"y\n".to_vec()))).unwrap();
        assert_eq!(answer(s), b"");
        let t = Instant::now();
        assert_eq!(answer(dial()), b"");
        assert!(t.elapsed() >= HELLO_WAIT - Duration::from_millis(200), "{:?}", t.elapsed());
        assert!(dir.join("client").exists(), "still attached");

        a.send("z\n");
        assert_eq!(a.wait().code, 8);
        assert!(a.output.text().contains("got:z"), "{:?}", a.output.text());

        // Ended: its code, whatever wrote it.
        fake_session(&root, "1790000000-0002", "started 1790000000\nended 1790000001\nexit 3\n");
        assert_eq!(client(&root, "1790000000-0002", None, None).wait(), Done { code: 3, note: None });
        let _ = fs::remove_dir_all(&root);
    }

    /// `rm` on a running session: SIGHUP ends a shell's `sleep`, the end is
    /// recorded, the dir goes. `kill_target` reads /proc, so Linux only.
    #[cfg(target_os = "linux")]
    #[test]
    fn rm_ends_a_running_session() {
        let root = temp_root("rm");
        let id = "1790000000-0001";
        let _child = start(&root, id, "sleep 300");
        wait_for("the command", || meta(&root, id).pgid.is_some());
        wait_for("running in ls", || ls(&root).unwrap().starts_with(&format!("{id} running detached ")));
        let pgid = meta(&root, id).pgid.unwrap();
        rm(&root, id).unwrap();
        assert!(!root.join(id).exists());
        // The supervisor reaped it before recording the end.
        assert!(runs::proc_stat(pgid).is_none(), "command {pgid} still there");
        assert_eq!(rm(&root, id).unwrap_err().kind(), io::ErrorKind::NotFound);
        let _ = fs::remove_dir_all(&root);
    }
}
