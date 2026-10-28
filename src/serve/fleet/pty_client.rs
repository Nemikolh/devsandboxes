//! The hub's end of a fleet link's `PTY` stream
//! (docs/architecture/daemon/fleet.md, *PTY*): a terminal on this host into
//! an offered worker's instance, what a tty `exec` on that machine runs
//! instead of ssh (`machines::cli`), and so the dashboard's terminal tabs
//! (which spawn that `exec`).
//!
//! One [`dial`] is one stream on the worker's `fleet-<name>.pty.sock`
//! (`endpoint::fleet_pty_socket`, bound by its `fleet-link`): the
//! [`header`] (`session.attach` with the [`Request`]), the terminal's size
//! at once and on every change, stdin as `stdin` records (EOF:
//! `stdin-eof`); the worker's `stdout` records to stdout until its one end
//! record. How it ended becomes this process's exit code ([`exit_of`]), in
//! the codes an ssh dial gives, so `cli::Reconnect` (and the dashboard's
//! `Redial`) treat both alike: 255 is a lost link and nothing else.
//!
//! [`header`] and [`exit_of`] are pure; [`attach`] is the relay over any
//! stream and input fd, the terminal (raw mode, size) injected; [`dial`]
//! the I/O on this process's stdio.

use std::io::{self, Write};
use std::net::Shutdown;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::pty::{METHOD, code};
use crate::devsbd::rpc::{self, Call, Exit, Obj, Record, code as helper};
use crate::serve::endpoint::{self, Stream};
use crate::serve::machines::cli::{DETACHED_EXIT, NO_SESSION_EXIT, SSH_FAILED, VERSION_EXIT};

/// How often the input side looks for a size change (and a stop): as the
/// helper's own session client (`devsbd/src/sessions.rs` `SIZE_POLL`), a
/// poll rather than a `SIGWINCH` handler, which would need `sigaction` and a
/// self-pipe to wake anything for at most this much latency on a resize.
pub const SIZE_POLL: Duration = Duration::from_millis(200);

/// The exit code of a refusal (`bad-request`, `denied`, `not-found`,
/// `not-running`, `unsupported`, `failed`): an error like any CLI's, never
/// 255, so no reconnect loop retries it.
pub const REFUSED_EXIT: i32 = 1;

/// What one dial asks the worker for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The instance's config root on the worker (absolute).
    pub root: PathBuf,
    /// The instance's exact name there (or its id).
    pub instance: String,
    /// The session id (`control::valid_run_id`).
    pub session: String,
    /// `None`: attach only. `Some(argv)`: create the session running `argv`
    /// (empty: the login shell) unless it exists, then attach.
    pub create: Option<Vec<String>>,
}

/// The stream's first record: a `session.attach` call of `r`
/// (`pty::parse_header` reads it on the worker).
pub fn header(r: &Request) -> Record {
    let mut params = Obj::new()
        .with("root", r.root.to_string_lossy().as_ref())
        .with("instance", r.instance.as_str())
        .with("session", r.session.as_str());
    if let Some(argv) = &r.create {
        let create = if argv.is_empty() { Obj::new() } else { Obj::new().with("argv", argv.clone()) };
        params = params.with("create", create);
    }
    Record::Call(Call { method: METHOD.into(), params, meta: Obj::new() })
}

/// How a dial ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// The session's program ended (its `exit` record).
    Exit(Exit),
    /// The worker's `error` record.
    Error { code: String, message: String },
    /// No end record: the socket isn't there (the link is down), or the
    /// stream ended or broke before one; why.
    Lost(String),
}

/// This process's exit code for `end`, and the line for stderr. An exit is
/// the program's code (`128 + n` for a signal), 255 reported as 254 under
/// `reserve_255` (as `exec::reported_code`: 255 then only means a lost
/// link); `no-session` / `detached` / `version` the helper's codes
/// ([`NO_SESSION_EXIT`], [`DETACHED_EXIT`], [`VERSION_EXIT`]) with its note
/// as its own session client prints it; `lost` and an end without a record
/// [`SSH_FAILED`]; any other error [`REFUSED_EXIT`] with the worker's
/// message.
pub fn exit_of(end: &End, reserve_255: bool) -> (i32, Option<String>) {
    match end {
        End::Exit(Exit::Code(n)) => (crate::commands::exec::reported_code(*n, reserve_255), None),
        End::Exit(Exit::Signal(n)) => (128 + n, None),
        End::Error { code: c, message } => {
            let session = |exit: i32| (exit, Some(format!("devsbd session: {message}")));
            match c.as_str() {
                helper::NO_SESSION => session(NO_SESSION_EXIT),
                helper::DETACHED => session(DETACHED_EXIT),
                helper::VERSION => session(VERSION_EXIT),
                code::LOST => (SSH_FAILED, Some(format!("the terminal's connection was lost: {message}"))),
                _ => (REFUSED_EXIT, Some(format!("error: {message} ({c})"))),
            }
        }
        End::Lost(why) => (SSH_FAILED, Some(why.clone())),
    }
}

/// The terminal one [`attach`] relays: `input` read (polled, so the relay
/// can stop without a byte to read), `output` written, `size` its size
/// (`None`: not a terminal, no `resize` sent).
pub struct Terminal<'a> {
    pub input: RawFd,
    pub output: &'a mut dyn Write,
    pub size: &'a (dyn Fn() -> Option<(u16, u16)> + Sync),
}

/// Relay one `PTY` stream to its end: `header` first, then the size and
/// `input` on a thread of their own, the worker's `stdout` to `output`
/// (`stderr` to this process's) until its end record. Closing the stream
/// at the end (or on a write failure) leaves the session running there.
pub fn attach(stream: Stream, header: &Record, term: Terminal) -> End {
    if let Err(e) = rpc::write_record(&mut &stream, header) {
        return End::Lost(format!("cannot start the terminal: {e}"));
    }
    let stop = AtomicBool::new(false);
    let Terminal { input, output, size } = term;
    std::thread::scope(|s| {
        if let Ok(writer) = stream.try_clone() {
            let stop = &stop;
            s.spawn(move || feed(writer, input, size, stop));
        }
        let end = loop {
            match rpc::read_record(&mut &stream) {
                Ok(Some(Record::Stdout(bytes))) => {
                    // A terminal that went away: nothing left to show it on.
                    if output.write_all(&bytes).and_then(|()| output.flush()).is_err() {
                        break End::Lost("cannot write to the terminal".into());
                    }
                }
                Ok(Some(Record::Stderr(bytes))) => {
                    let _ = io::stderr().write_all(&bytes);
                }
                Ok(Some(Record::Exit(exit))) => break End::Exit(exit),
                Ok(Some(Record::Error { code, message })) => break End::Error { code, message },
                Ok(Some(_)) => {}
                Ok(None) => break End::Lost("the fleet link closed the terminal without an end".into()),
                Err(e) => break End::Lost(format!("the fleet link's terminal broke: {e}")),
            }
        };
        stop.store(true, Ordering::SeqCst);
        let _ = stream.shutdown(Shutdown::Both);
        end
    })
}

/// The input side: the size at once and whenever it changes (looked at
/// every [`SIZE_POLL`]), `input`'s bytes as `stdin` and its EOF as
/// `stdin-eof`, until `stop` or the stream refuses a write.
fn feed(mut w: Stream, input: RawFd, size: &(dyn Fn() -> Option<(u16, u16)> + Sync), stop: &AtomicBool) {
    let mut send = |rec: &Record| rpc::write_record(&mut w, rec).is_ok();
    let resize = |s: (u16, u16)| Record::Resize { cols: s.0, rows: s.1 };
    let mut last = size();
    if let Some(s) = last
        && !send(&resize(s))
    {
        return;
    }
    let mut eof = false;
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::SeqCst) {
        if eof {
            std::thread::sleep(SIZE_POLL);
        } else {
            match read_ready(input, &mut buf) {
                Ok(Some(0)) => {
                    eof = true;
                    if !send(&Record::StdinEof) {
                        return;
                    }
                }
                Ok(Some(n)) => {
                    if !send(&Record::Stdin(buf[..n].to_vec())) {
                        return;
                    }
                }
                Ok(None) => {}
                // An input that can't be read: as its end.
                Err(_) => {
                    eof = true;
                    if !send(&Record::StdinEof) {
                        return;
                    }
                }
            }
        }
        let now = size();
        if now != last {
            last = now;
            if let Some(s) = now
                && !send(&resize(s))
            {
                return;
            }
        }
    }
}

/// One read of `fd` if it becomes readable within [`SIZE_POLL`]: `None`
/// when it didn't (or a signal interrupted the wait), `Some(0)` at its end.
fn read_ready(fd: RawFd, buf: &mut [u8]) -> io::Result<Option<usize>> {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd for the duration of the call.
    let n = unsafe { libc::poll(&mut p, 1, SIZE_POLL.as_millis() as libc::c_int) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(None) } else { Err(e) };
    }
    if n == 0 {
        return Ok(None);
    }
    // SAFETY: `buf` is valid for `buf.len()` bytes of writes.
    let got = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if got < 0 {
        let e = io::Error::last_os_error();
        return match e.kind() {
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => Ok(None),
            _ => Err(e),
        };
    }
    Ok(Some(got as usize))
}

/// This process's terminal in raw mode while held (crossterm's, as the
/// dashboard's), restored on drop: every return and an unwinding panic.
struct Raw;

impl Raw {
    /// Raw mode when stdin is a terminal; else nothing to restore.
    fn enter() -> Option<Raw> {
        use std::io::IsTerminal;
        (io::stdin().is_terminal() && crossterm::terminal::enable_raw_mode().is_ok()).then_some(Raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// One dial of `r` to offered machine `machine`, whose `fleet-link`
/// sockets are in `dir`, on this process's stdio: its exit code
/// ([`exit_of`], its line on stderr once the terminal is restored).
pub fn dial(dir: &Path, machine: &str, r: &Request, reserve_255: bool) -> i32 {
    let path = endpoint::fleet_pty_socket(dir, machine);
    let end = match Stream::connect(&path) {
        Err(e) => End::Lost(format!("machine `{machine}`: its fleet link isn't up ({}: {e})", path.display())),
        Ok(stream) => {
            let raw = Raw::enter();
            let size = || crate::build_log::terminal_size(0).map(|s| (s.cols, s.rows));
            let mut out = io::stdout();
            let end = attach(stream, &header(r), Terminal { input: 0, output: &mut out, size: &size });
            drop(raw);
            end
        }
    };
    let (code, note) = exit_of(&end, reserve_255);
    if let Some(note) = note {
        // Raw mode is off: a plain line again.
        eprintln!("{note}");
    }
    code
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex;

    use super::*;
    use crate::serve::fleet::pty;

    const ID: &str = "1700000000-ab12";

    fn req(create: Option<Vec<String>>) -> Request {
        Request { root: "/w/fleet/hub/cfg".into(), instance: "web-1".into(), session: ID.into(), create }
    }

    /// What the worker parses back is what was asked.
    #[test]
    fn the_header_is_what_the_worker_parses() {
        for create in [None, Some(Vec::new()), Some(vec!["htop".to_string(), "-d".into()])] {
            let h = pty::parse_header(header(&req(create.clone()))).unwrap();
            assert_eq!(h, pty::Header { root: "/w/fleet/hub/cfg".into(), instance: "web-1".into(), session: ID.into(), create });
        }
        // The login shell is `create` without an argv.
        let Record::Call(c) = header(&req(Some(Vec::new()))) else { panic!() };
        assert_eq!(c.params.obj("create"), Some(&Obj::new()));
    }

    #[test]
    fn ends_map_to_the_codes_an_ssh_dial_gives() {
        let err = |c: &str| End::Error { code: c.into(), message: format!("why {c}") };
        assert_eq!(exit_of(&End::Exit(Exit::Code(0)), true), (0, None));
        assert_eq!(exit_of(&End::Exit(Exit::Code(255)), false), (255, None));
        assert_eq!(exit_of(&End::Exit(Exit::Code(255)), true), (254, None), "255 stays the link's");
        assert_eq!(exit_of(&End::Exit(Exit::Signal(9)), true), (137, None));
        assert_eq!(exit_of(&err("no-session"), true), (250, Some("devsbd session: why no-session".into())));
        assert_eq!(exit_of(&err("detached"), true).0, 251);
        assert_eq!(exit_of(&err("version"), true).0, 252);
        assert_eq!(exit_of(&err("lost"), true).0, 255);
        assert_eq!(exit_of(&End::Lost("gone".into()), true), (255, Some("gone".into())));
        for c in ["bad-request", "failed", "denied", "not-found", "not-running", "unsupported"] {
            let (code, note) = exit_of(&err(c), true);
            assert_eq!(code, REFUSED_EXIT, "{c}");
            assert_eq!(note, Some(format!("error: why {c} ({c})")));
        }
        // None of them is retried as a lost connection but `lost`.
        let mut r = crate::serve::machines::cli::Reconnect::default();
        assert!(matches!(r.after(REFUSED_EXIT, Duration::from_secs(60)), crate::serve::machines::cli::Next::Done { .. }));
    }

    /// A worker end on `s`: reads the header, sends `stdout` `hi`, echoes
    /// `stdin` as `stdout`, a `resize` as `stdout` `<cols>x<rows>`, and at
    /// `stdin-eof` sends `exit 3` (or, `then` given, ends with it instead,
    /// or with nothing when `None`).
    fn worker(s: UnixStream, then: Option<Option<Record>>) -> std::thread::JoinHandle<Vec<Record>> {
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let send = |rec: Record| rpc::write_record(&mut &s, &rec);
            let Ok(Some(first)) = rpc::read_record(&mut &s) else { return seen };
            seen.push(first);
            let _ = send(Record::Stdout(b"hi ".to_vec()));
            if let Some(end) = then {
                if let Some(rec) = end {
                    let _ = send(rec);
                }
                let _ = s.shutdown(Shutdown::Both);
                return seen;
            }
            while let Ok(Some(rec)) = rpc::read_record(&mut &s) {
                seen.push(rec.clone());
                let reply = match rec {
                    Record::Stdin(b) => Record::Stdout(b),
                    Record::Resize { cols, rows } => Record::Stdout(format!("[{cols}x{rows}]").into_bytes()),
                    Record::StdinEof => {
                        let _ = send(Record::Exit(Exit::Code(3)));
                        break;
                    }
                    _ => continue,
                };
                let _ = send(reply);
            }
            let _ = s.shutdown(Shutdown::Both);
            seen
        })
    }

    /// [`attach`] over a socket pair, its input another pair's end (written
    /// by `typed` and closed), its size from `sizes` (the first answer, then
    /// the rest in turn). Returns what was written, how it ended, and what
    /// the worker read.
    fn run(then: Option<Option<Record>>, typed: &[u8], sizes: Vec<Option<(u16, u16)>>) -> (String, End, Vec<Record>) {
        let (hub, w) = UnixStream::pair().unwrap();
        let worker = worker(w, then);
        let (keys, input) = UnixStream::pair().unwrap();
        let sizes = Mutex::new(sizes);
        let size = || {
            let mut s = sizes.lock().unwrap();
            if s.len() > 1 { s.remove(0) } else { s[0] }
        };
        let mut out = Vec::new();
        let typed = typed.to_vec();
        let typist = std::thread::spawn(move || {
            // After the first size went out: the order the worker reads is then fixed.
            std::thread::sleep(Duration::from_millis(50));
            (&keys).write_all(&typed).unwrap();
            std::thread::sleep(SIZE_POLL * 2);
            drop(keys);
        });
        let end = attach(hub, &header(&req(Some(Vec::new()))), Terminal { input: input.as_raw_fd(), output: &mut out, size: &size });
        typist.join().unwrap();
        (String::from_utf8(out).unwrap(), end, worker.join().unwrap())
    }

    #[test]
    fn a_dial_relays_the_terminal_to_the_end_record() {
        let (out, end, seen) = run(None, b"ls\n", vec![Some((80, 24)), Some((80, 24)), Some((120, 40))]);
        assert_eq!(end, End::Exit(Exit::Code(3)));
        assert!(matches!(&seen[0], Record::Call(c) if c.method == METHOD), "{seen:?}");
        assert_eq!(seen[1], Record::Resize { cols: 80, rows: 24 }, "the size first");
        assert!(seen.contains(&Record::Stdin(b"ls\n".to_vec())), "{seen:?}");
        assert!(seen.contains(&Record::Resize { cols: 120, rows: 40 }), "a change: {seen:?}");
        assert_eq!(seen.last(), Some(&Record::StdinEof));
        assert!(out.starts_with("hi [80x24]"), "{out}");
        assert!(out.contains("ls\n") && out.contains("[120x40]"), "{out}");
        assert_eq!(exit_of(&end, true).0, 3);
    }

    #[test]
    fn a_refusal_or_a_lost_stream_ends_the_dial() {
        let refused = Record::error(code::NOT_FOUND, "no instance `x`");
        let (out, end, _) = run(Some(Some(refused)), b"", vec![None]);
        assert_eq!(out, "hi ");
        assert_eq!(end, End::Error { code: "not-found".into(), message: "no instance `x`".into() });
        assert_eq!(exit_of(&end, true).0, REFUSED_EXIT);
        let (_, end, seen) = run(Some(None), b"", vec![None]);
        assert!(matches!(&end, End::Lost(why) if why.contains("without an end")), "{end:?}");
        assert_eq!(exit_of(&end, true).0, 255);
        assert_eq!(seen.len(), 1, "no size: no `resize`");
    }

    #[test]
    fn a_link_that_isnt_up_is_a_lost_dial() {
        let dir = std::env::temp_dir().join(format!("dsv-ptyc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(dial(&dir, "lap", &req(None), true), 255);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
